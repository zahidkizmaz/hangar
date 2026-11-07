//! What `ps` and `inspect` show, read without changing anything.

use std::path::PathBuf;

use crate::bay::{self, Bay};
use crate::hangar::Hangar;
use crate::lifecycle::Resource;
use crate::mounts::{self, MountStatus};
use crate::proxy::{Health, Location, Managed, Proxy, ProxyPort, proxy_vm};
use crate::sandbox::{BoxState, PublishedPort, data_volume};
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
    pub(crate) state: Option<BoxState>,
    pub(crate) proxy: Option<String>,
    pub(crate) image: String,
    pub(crate) apps: Vec<(String, RunState)>,
    pub(crate) ports: Vec<PublishedPort>,
    pub(crate) mounts: Vec<MountStatus>,
}

pub(crate) struct ProxyStatus {
    pub(crate) name: String,
    pub(crate) kind: &'static str,
    pub(crate) managed: bool,
    pub(crate) state: Option<BoxState>,
    pub(crate) image: Option<String>,
    pub(crate) health: Option<Health>,
    pub(crate) port: Option<u16>,
    pub(crate) published: Vec<ProxyPort>,
}

pub(crate) enum Detail {
    Bay(BayDetail),
    Proxy(ProxyDetail),
}

pub(crate) struct BayDetail {
    pub(crate) status: BayStatus,
    pub(crate) vm: String,
    pub(crate) cpus: u32,
    pub(crate) memory: String,
    pub(crate) disk: String,
    pub(crate) proxy_port: Option<u16>,
    pub(crate) packages: Vec<String>,
    pub(crate) apps: Vec<String>,
    pub(crate) env: Vec<String>,
    pub(crate) home: PathBuf,
    pub(crate) cache: PathBuf,
    pub(crate) volume: String,
}

pub(crate) struct ProxyDetail {
    pub(crate) status: ProxyStatus,
    pub(crate) vm: Option<String>,
    pub(crate) url: Option<String>,
    pub(crate) data: PathBuf,
    pub(crate) log: Option<PathBuf>,
    pub(crate) bays: Vec<String>,
    pub(crate) tokens: Vec<(String, TokenSource)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenSource {
    Keychain,
    Command,
    Missing,
    Unknown,
}

impl TokenSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Keychain => "keychain",
            Self::Command => "command",
            Self::Missing => "missing",
            Self::Unknown => "unknown",
        }
    }
}

pub(crate) fn of(hangar: &Hangar, names: &[&str]) -> Overview {
    Overview {
        bays: hangar
            .bays()
            .iter()
            .filter(|bay| names.contains(&bay.name))
            .map(|bay| bay_status(hangar, bay, false))
            .collect(),
        proxies: hangar
            .proxies
            .iter()
            .filter(|(name, _)| names.contains(&name.as_str()))
            .map(|(name, proxy)| proxy_status(hangar, name, proxy.as_ref()))
            .collect(),
        leftovers: Vec::new(),
    }
}

pub(crate) fn collect(hangar: &Hangar) -> Overview {
    Overview {
        bays: hangar
            .bays()
            .iter()
            .map(|bay| bay_status(hangar, bay, true))
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
    let settings = bay.settings;
    let proxy_port = hangar
        .proxy_of(settings)
        .and_then(|(_, proxy)| proxy.egress().host_port());
    Detail::Bay(BayDetail {
        status: bay_status(hangar, bay, true),
        vm: bay.vm.clone(),
        cpus: settings.cpus,
        memory: settings.memory.clone(),
        disk: settings.disk.clone(),
        proxy_port,
        packages: settings.packages.clone(),
        apps: settings.apps.iter().map(|app| app.name.clone()).collect(),
        env: settings.env.keys().cloned().collect(),
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
    let url = match hangar.settings.proxies.get(name).map(|e| &e.location) {
        Some(Location::External(url)) => Some(url.clone()),
        _ => None,
    };
    let bays = hangar
        .settings
        .bays
        .iter()
        .filter(|bay| bay.proxy.as_deref() == Some(name))
        .map(|bay| bay.name.clone())
        .collect();
    Detail::Proxy(ProxyDetail {
        status: proxy_status(hangar, name, proxy),
        vm: managed.map(|_| proxy_vm(name)),
        url,
        data: hangar.state.proxy(name).data(),
        log: managed.map(Managed::log_file),
        bays,
        tokens: tokens(hangar, name, proxy),
    })
}

fn tokens(
    hangar: &Hangar,
    name: &str,
    proxy: &dyn Proxy,
) -> Vec<(String, TokenSource)> {
    let keys = crate::proxy::token_keys(proxy);
    let secrets = match hangar.secrets(name) {
        Ok(secrets) => secrets,
        Err(error) => {
            log::warn!("proxy {name}: tokens: {error}");
            return keys
                .into_iter()
                .map(|key| (key, TokenSource::Unknown))
                .collect();
        }
    };
    keys.into_iter()
        .map(|key| {
            let source = if secrets.is_command(&key) {
                TokenSource::Command
            } else {
                match secrets.has(&key) {
                    Ok(true) => TokenSource::Keychain,
                    Ok(false) => TokenSource::Missing,
                    Err(error) => {
                        log::warn!("proxy {name}: {key}: {error}");
                        TokenSource::Unknown
                    }
                }
            };
            (key, source)
        })
        .collect()
}

fn bay_status(hangar: &Hangar, bay: &Bay, apps: bool) -> BayStatus {
    let state = vm_state(hangar, &bay.vm);
    let apps = match state {
        Some(BoxState::Running) if apps => app_states(hangar, bay),
        _ => Vec::new(),
    };
    let (home, cache) = bay.own_mounts();
    let recorded = VmRecord::load(&bay.dir.vm()).map(|record| record.mounts);
    BayStatus {
        name: bay.name.to_string(),
        state,
        proxy: bay.settings.proxy.clone(),
        image: bay.settings.image_ref(),
        apps,
        ports: bay.settings.ports.clone(),
        mounts: mounts::status(
            &bay.settings.mounts,
            home.as_ref(),
            cache.as_ref(),
            &hangar.host_home,
            recorded.as_deref(),
        ),
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
        port: proxy.egress().host_port(),
        published: managed
            .map(|managed| managed.vm().publish)
            .unwrap_or_default(),
    }
}

fn vm_state(hangar: &Hangar, vm: &str) -> Option<BoxState> {
    match hangar.sandbox.state(vm) {
        Ok(state) => Some(state),
        Err(error) => {
            log::warn!("{vm}: {error}");
            None
        }
    }
}

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
            ports: Vec::new(),
            mounts: Vec::new(),
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
            port: None,
            published: Vec::new(),
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
