//! What `ps` and `inspect` show, read without changing anything.

use std::path::PathBuf;

use crate::bay::{self, Bay};
use crate::hangar::Hangar;
use crate::lifecycle::Resource;
use crate::mounts::{self, MountStatus};
use crate::proxy::{Health, Managed, Proxy, proxy_vm};
use crate::sandbox::{BoxState, data_volume};
use crate::vm_record::VmRecord;

pub(crate) struct Overview {
    pub(crate) bays: Vec<BayStatus>,
    pub(crate) proxies: Vec<ProxyStatus>,
    pub(crate) leftovers: Vec<(Resource, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunState {
    Running,
    Exited,
    /// Never launched: its setup check fails.
    NeedsSetup,
}

impl RunState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Exited => "exited",
            Self::NeedsSetup => "needs-setup",
        }
    }
}

pub(crate) struct BayStatus {
    pub(crate) name: String,
    /// `None` when the sandbox can't tell.
    pub(crate) state: Option<BoxState>,
    pub(crate) proxy: Option<String>,
    pub(crate) image: String,
    /// The configured apps' state; empty unless the bay runs.
    pub(crate) apps: Vec<(String, RunState)>,
}

pub(crate) struct ProxyStatus {
    pub(crate) name: String,
    pub(crate) kind: &'static str,
    /// Runs in its own VM, whose state is `state` (`None`: unknown).
    pub(crate) managed: bool,
    pub(crate) state: Option<BoxState>,
    pub(crate) image: Option<String>,
    /// `None` when there is nothing to probe: the proxy's VM isn't up.
    pub(crate) health: Option<Health>,
}

pub(crate) enum Detail {
    Bay(BayDetail),
    Proxy(ProxyDetail),
}

pub(crate) struct BayDetail {
    pub(crate) status: BayStatus,
    pub(crate) vm: String,
    pub(crate) mounts: Vec<MountStatus>,
    pub(crate) packages: Vec<String>,
    pub(crate) apps: Vec<String>,
    pub(crate) home: PathBuf,
    pub(crate) cache: PathBuf,
    pub(crate) volume: String,
}

pub(crate) struct ProxyDetail {
    pub(crate) status: ProxyStatus,
    pub(crate) vm: Option<String>,
    pub(crate) data: PathBuf,
    pub(crate) log: Option<PathBuf>,
}

pub(crate) fn collect(hangar: &Hangar) -> Overview {
    Overview {
        bays: hangar
            .bays()
            .iter()
            .map(|bay| bay_status(hangar, bay))
            .collect(),
        proxies: hangar
            .proxies
            .iter()
            .map(|(name, proxy)| proxy_status(hangar, name, proxy.as_ref()))
            .collect(),
        leftovers: hangar.leftovers(),
    }
}

pub(crate) fn bay_detail(hangar: &Hangar, bay: &Bay) -> Detail {
    let (home, cache) = bay.own_mounts();
    let recorded = VmRecord::load(&bay.dir.vm()).map(|record| record.mounts);
    let settings = bay.settings;
    Detail::Bay(BayDetail {
        status: bay_status(hangar, bay),
        vm: bay.vm.clone(),
        mounts: mounts::status(
            &settings.mounts,
            home.as_ref(),
            cache.as_ref(),
            &hangar.host_home,
            recorded.as_deref(),
        ),
        packages: settings.packages.clone(),
        apps: settings.apps.iter().map(|app| app.name.clone()).collect(),
        home: bay.dir.home(),
        cache: bay.cache.clone(),
        volume: data_volume(bay.name),
    })
}

pub(crate) fn proxy_detail(
    hangar: &Hangar,
    name: &str,
    proxy: &dyn Proxy,
) -> Detail {
    let managed = proxy.managed();
    Detail::Proxy(ProxyDetail {
        status: proxy_status(hangar, name, proxy),
        vm: managed.map(|_| proxy_vm(name)),
        data: hangar.state.proxy(name).data(),
        log: managed.map(Managed::log_file),
    })
}

fn bay_status(hangar: &Hangar, bay: &Bay) -> BayStatus {
    let state = vm_state(hangar, &bay.vm);
    let apps = match state {
        Some(BoxState::Running) => app_states(hangar, bay),
        _ => Vec::new(),
    };
    BayStatus {
        name: bay.name.to_string(),
        state,
        proxy: bay.settings.proxy.clone(),
        image: bay.settings.image_ref(),
        apps,
    }
}

fn app_states(hangar: &Hangar, bay: &Bay) -> Vec<(String, RunState)> {
    let probed = bay::active_runs(hangar, bay)
        .and_then(|active| Ok((active, bay::needing_setup(hangar, bay)?)));
    let (active, unset) = match probed {
        Ok(probed) => probed,
        Err(error) => {
            log::warn!("bay {}: apps: {error}", bay.name);
            return Vec::new();
        }
    };
    bay.settings
        .run
        .keys()
        .map(|name| {
            let state = if active.contains(name) {
                RunState::Running
            } else if unset.contains(name) {
                RunState::NeedsSetup
            } else {
                RunState::Exited
            };
            (name.clone(), state)
        })
        .collect()
}

fn proxy_status(hangar: &Hangar, name: &str, proxy: &dyn Proxy) -> ProxyStatus {
    let managed = proxy.managed();
    let state = managed.and_then(|_| vm_state(hangar, &proxy_vm(name)));
    let probeable = managed.is_none() || state == Some(BoxState::Running);
    ProxyStatus {
        name: name.to_string(),
        kind: proxy.kind(),
        managed: managed.is_some(),
        state,
        image: managed.map(|managed| managed.vm().image),
        health: probeable.then(|| proxy.health()),
    }
}

/// `None`, with a warning, when the sandbox can't tell.
fn vm_state(hangar: &Hangar, vm: &str) -> Option<BoxState> {
    match hangar.sandbox.state(vm) {
        Ok(state) => Some(state),
        Err(error) => {
            log::warn!("{vm}: {error}");
            None
        }
    }
}

/// What `ps` found wrong, one sentence each; they are logged and make `ps`
/// exit 3.
pub(crate) fn events(overview: &Overview) -> Vec<String> {
    let apps = overview.bays.iter().flat_map(|bay| {
        bay.apps.iter().filter_map(|(app, state)| {
            let name = &bay.name;
            match state {
                RunState::Running => None,
                RunState::Exited => Some(format!(
                    "bay {name}: app {app} isn't running; \
                     see 'hangar logs {name} {app}'"
                )),
                RunState::NeedsSetup => Some(format!(
                    "bay {name}: app {app} needs setup; \
                     run 'hangar setup {name} {app}'"
                )),
            }
        })
    });
    let proxies = overview.proxies.iter().filter_map(|proxy| {
        let Some(Health::Down(why)) = &proxy.health else {
            return None;
        };
        Some(format!("proxy {} is unreachable: {why}", proxy.name))
    });
    let leftovers = overview.leftovers.iter().map(|(resource, name)| {
        format!(
            "leftover {} {name}: 'hangar rm {name} --data' removes it",
            resource.as_str()
        )
    });
    apps.chain(proxies).chain(leftovers).collect()
}

#[cfg(test)]
mod tests {
    use super::{BayStatus, Overview, ProxyStatus, RunState, events};
    use crate::lifecycle::Resource;
    use crate::proxy::Health;
    use crate::sandbox::BoxState;

    fn bay(apps: Vec<(String, RunState)>) -> BayStatus {
        BayStatus {
            name: "work".into(),
            state: Some(BoxState::Running),
            proxy: None,
            image: "img:1".into(),
            apps,
        }
    }

    fn proxy(health: Option<Health>) -> ProxyStatus {
        ProxyStatus {
            name: "infi".into(),
            kind: "fake",
            managed: false,
            state: None,
            image: None,
            health,
        }
    }

    fn overview(
        bays: Vec<BayStatus>,
        proxies: Vec<ProxyStatus>,
        leftovers: Vec<(Resource, String)>,
    ) -> Overview {
        Overview {
            bays,
            proxies,
            leftovers,
        }
    }

    #[test]
    fn exited_app_is_an_event() {
        let apps = vec![
            ("web".to_string(), RunState::Running),
            ("paperclip".to_string(), RunState::Exited),
        ];
        assert_eq!(
            events(&overview(vec![bay(apps)], vec![], vec![])),
            ["bay work: app paperclip isn't running; \
              see 'hangar logs work paperclip'"]
        );
    }

    #[test]
    fn app_needing_setup_is_an_event_pointing_at_setup() {
        let apps = vec![("paperclip".to_string(), RunState::NeedsSetup)];
        assert_eq!(
            events(&overview(vec![bay(apps)], vec![], vec![])),
            ["bay work: app paperclip needs setup; \
              run 'hangar setup work paperclip'"]
        );
    }

    #[test]
    fn unreachable_proxy_is_an_event() {
        let down = proxy(Some(Health::Down("connection refused".into())));
        assert_eq!(
            events(&overview(vec![], vec![down], vec![])),
            ["proxy infi is unreachable: connection refused"]
        );
    }

    #[test]
    fn leftovers_are_events() {
        let leftovers = vec![
            (Resource::Bay, "old".to_string()),
            (Resource::Proxy, "gone".to_string()),
        ];
        assert_eq!(
            events(&overview(vec![], vec![], leftovers)),
            [
                "leftover bay old: 'hangar rm old --data' removes it",
                "leftover proxy gone: 'hangar rm gone --data' removes it"
            ]
        );
    }

    #[test]
    fn no_events_when_all_well() {
        let apps = vec![("web".to_string(), RunState::Running)];
        let all_well = overview(
            vec![bay(apps), bay(vec![])],
            vec![proxy(Some(Health::Up)), proxy(None)],
            vec![],
        );
        assert_eq!(events(&all_well), Vec::<String>::new());
    }

    #[test]
    fn run_states_have_stable_words() {
        assert_eq!(RunState::Running.as_str(), "running");
        assert_eq!(RunState::Exited.as_str(), "exited");
        assert_eq!(RunState::NeedsSetup.as_str(), "needs-setup");
    }
}
