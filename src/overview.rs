//! What a user can reach on the host: the data behind `status`, the port
//! summary and `hangar vault-ui` (data only). Rendering lives in `output`.

use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

use crate::bay;
use crate::bay::Bay;
use crate::broker::{Broker, BrokerHealth, Policy};
use crate::config::{BaySettings, Settings};
use crate::error::{Error, Result};
use crate::hangar::Hangar;
use crate::http;
use crate::mounts::{self, MountStatus};
use crate::sandbox::{BoxState, PublishedPort, Sandbox, TOWER_VM, bay_vm};
use crate::vm_record::VmRecord;

pub(crate) struct Vm {
    pub(crate) name: String,
    /// What the sandbox reports, for people (`Running`, `missing`, …).
    pub(crate) shown: String,
    /// `None` when the sandbox itself failed.
    pub(crate) state: Option<BoxState>,
}

impl Vm {
    fn inspect(sandbox: &dyn Sandbox, name: &str) -> Self {
        let (state, shown) = match sandbox.describe(name) {
            Ok((state, shown)) => (Some(state), shown),
            Err(error) => (None, format!("unknown ({error})")),
        };
        Self {
            name: name.to_string(),
            shown,
            state,
        }
    }

    pub(crate) fn state_str(&self) -> &'static str {
        self.state.map_or("unknown", BoxState::as_str)
    }

    pub(crate) fn running(&self) -> bool {
        self.state == Some(BoxState::Running)
    }
}

/// A `run` entry's state; `Unknown` keeps why it couldn't be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RunState {
    Running,
    Stopped,
    Unknown(String),
}

impl RunState {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Unknown(_) => "unknown",
        }
    }
}

pub(crate) fn published(
    broker: &dyn Broker,
    settings: &Settings,
) -> Vec<PublishedPort> {
    let mut ports = broker.ports();
    ports.extend(settings.bays.iter().flat_map(|bay| bay.ports.clone()));
    ports
}

/// One host port can be published once; a clash between bays suggests
/// moving the second one.
pub(crate) fn check_ports(ports: &[PublishedPort]) -> Result<()> {
    for (index, port) in ports.iter().enumerate() {
        let Some(other) = ports[..index].iter().find(|o| o.host == port.host)
        else {
            continue;
        };
        let message = format!(
            "port {}: {} and {}",
            port.host,
            other.owner(),
            port.owner()
        );
        return Err(match &port.bay {
            Some(bay) => Error::with_hint(
                message,
                format!("set ports.{} in bay {bay}", port.name),
            ),
            None => Error::new(message),
        });
    }
    Ok(())
}

/// A port and whether it answers (`None`: not probed).
pub(crate) struct PortStatus {
    pub(crate) port: PublishedPort,
    pub(crate) reachable: Option<bool>,
}

pub(crate) struct Ports(pub(crate) Vec<PortStatus>);

impl Ports {
    /// The tower's non-HTTP ports are only for the bays: not probed.
    fn probe(ports: Vec<PublishedPort>) -> Self {
        Self(
            ports
                .into_iter()
                .map(|port| PortStatus {
                    reachable: (port.http || port.bay.is_some())
                        .then(|| reachable(&port)),
                    port,
                })
                .collect(),
        )
    }
}

fn reachable(port: &PublishedPort) -> bool {
    if port.http {
        http::get(port.host, "/").is_ok()
    } else {
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port.host));
        TcpStream::connect_timeout(&address, Duration::from_secs(2)).is_ok()
    }
}

/// An enabled app's hosts and the credential each one injects (`None`:
/// passthrough).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AppHosts {
    pub(crate) name: String,
    /// Route name, host and credential.
    pub(crate) routes: Vec<(String, String, Option<String>)>,
    pub(crate) has_setup: bool,
}

fn app_hosts(settings: &Settings, bay: &BaySettings) -> Vec<AppHosts> {
    bay.apps
        .iter()
        .map(|app| AppHosts {
            name: app.name.clone(),
            routes: app
                .routes
                .iter()
                .filter_map(|name| settings.routes.get(name))
                .map(|route| {
                    let credentials = route.auth.credentials();
                    let credential = (!credentials.is_empty()).then(|| {
                        credentials.into_iter().collect::<Vec<_>>().join(", ")
                    });
                    (route.name.clone(), route.host.clone(), credential)
                })
                .collect(),
            has_setup: app.setup.is_some(),
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq, miniserde::Serialize)]
pub(crate) struct Cache {
    pub(crate) bytes: u64,
    pub(crate) host: String,
}

pub(crate) struct BayStatus {
    pub(crate) name: String,
    pub(crate) vm: Vm,
    pub(crate) apps: Vec<AppHosts>,
    /// Each `run` entry (`Stopped` while the bay isn't running).
    pub(crate) run: Vec<(String, RunState)>,
    pub(crate) mounts: Vec<MountStatus>,
    pub(crate) cache: Option<Cache>,
    pub(crate) ports: Ports,
}

impl BayStatus {
    fn collect(hangar: &Hangar, bay: &Bay) -> Self {
        let sandbox = hangar.sandbox.as_ref();
        let vm = Vm::inspect(sandbox, &bay.vm);
        let run = bay
            .settings
            .run
            .keys()
            .map(|name| {
                let status = if vm.running() {
                    bay::run_state(sandbox, &bay.vm, name).unwrap_or_else(
                        |error| RunState::Unknown(error.to_string()),
                    )
                } else {
                    RunState::Stopped
                };
                (name.clone(), status)
            })
            .collect();
        // A missing VM has no mounts, whatever an old record says; an
        // existing one without a record has unknown ones.
        let recorded = if vm.state.is_some_and(|s| s != BoxState::Missing) {
            VmRecord::load(&bay.dir.vm()).map(|record| record.mounts)
        } else {
            Some(Vec::new())
        };
        let cache = bay.settings.cache.then(|| Cache {
            host: bay.cache.display().to_string(),
            bytes: dir_size(&bay.cache),
        });
        let (home_mount, cache_mount) = bay.own_mounts();
        Self {
            name: bay.name.to_string(),
            vm,
            apps: app_hosts(&hangar.settings, bay.settings),
            run,
            mounts: mounts::status(
                &bay.settings.mounts,
                home_mount.as_ref(),
                cache_mount.as_ref(),
                &hangar.host_home,
                recorded.as_deref(),
            ),
            cache,
            ports: Ports::probe(bay.settings.ports.clone()),
        }
    }

    pub(crate) fn has_setup(&self, name: &str) -> bool {
        self.apps
            .iter()
            .any(|app| app.name == name && app.has_setup)
    }

    /// Running, with every `run` entry running.
    pub(crate) fn healthy(&self) -> bool {
        self.vm.running()
            && self
                .run
                .iter()
                .all(|(_, state)| *state == RunState::Running)
    }
}

pub(crate) struct Leftover {
    pub(crate) name: String,
    pub(crate) vm: Vm,
}

pub(crate) struct UpReport {
    pub(crate) status: StatusReport,
    pub(crate) failed: Vec<(String, String)>,
}

/// A route the tower serves, credential names only; `app` is the first
/// enabled app that brings it, `None` for `tower.routes`.
#[derive(Debug, PartialEq, Eq, miniserde::Serialize)]
pub(crate) struct TowerRoute {
    pub(crate) app: Option<String>,
    pub(crate) auth: &'static str,
    pub(crate) credentials: Vec<String>,
    pub(crate) host: String,
    pub(crate) name: String,
}

fn tower_routes(settings: &Settings) -> Vec<TowerRoute> {
    settings
        .routes
        .values()
        .map(|route| TowerRoute {
            name: route.name.clone(),
            host: route.host.clone(),
            auth: route.auth.kind(),
            credentials: route.auth.credentials().into_iter().collect(),
            app: settings
                .bays
                .iter()
                .flat_map(|bay| &bay.apps)
                .find(|app| app.routes.contains(&route.name))
                .map(|app| app.name.clone()),
        })
        .collect()
}

pub(crate) struct StatusReport {
    pub(crate) tower: Vm,
    pub(crate) broker_backend: String,
    pub(crate) broker: BrokerHealth,
    pub(crate) tower_ports: Ports,
    pub(crate) routes: Vec<TowerRoute>,
    pub(crate) bays: Vec<BayStatus>,
    pub(crate) leftovers: Vec<Leftover>,
}

impl StatusReport {
    pub(crate) fn collect(hangar: &Hangar) -> Self {
        let sandbox = hangar.sandbox.as_ref();
        Self {
            tower: Vm::inspect(sandbox, TOWER_VM),
            broker_backend: hangar.settings.broker_backend.clone(),
            broker: hangar.broker.health(),
            tower_ports: Ports::probe(hangar.broker.ports()),
            routes: tower_routes(&hangar.settings),
            bays: hangar
                .bays()
                .iter()
                .map(|bay| BayStatus::collect(hangar, bay))
                .collect(),
            leftovers: hangar
                .leftovers()
                .into_iter()
                .map(|name| Leftover {
                    vm: Vm::inspect(sandbox, &bay_vm(&name)),
                    name,
                })
                .collect(),
        }
    }

    /// The tower running, healthy and denying unlisted hosts, and every
    /// configured bay healthy; leftovers don't count.
    pub(crate) fn healthy(&self) -> bool {
        self.tower.running()
            && self.broker.healthy
            && self.broker.unlisted == Some(Policy::Deny)
            && self.bays.iter().all(BayStatus::healthy)
    }
}

/// Bytes of the regular files under `path`, symlinks not followed; 0 when
/// it doesn't exist yet.
fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            Some(if meta.is_dir() {
                dir_size(&entry.path())
            } else if meta.is_file() {
                meta.len()
            } else {
                0
            })
        })
        .sum()
}

/// Where the broker login is; never the password itself.
pub(crate) struct VaultLogin {
    pub(crate) url: String,
    pub(crate) login: String,
    /// Copied to the clipboard; otherwise only in `password_file`.
    pub(crate) copied: bool,
    pub(crate) password_file: String,
}

#[cfg(test)]
mod tests {
    use super::{Ports, check_ports, published};
    use crate::hangar::broker;
    use crate::sandbox::PublishedPort;
    use crate::sandbox::fake::FakeSandbox;
    use crate::testing::{closed_port, settings};
    use std::net::TcpListener;
    use std::path::Path;
    use std::rc::Rc;

    #[test]
    fn broker_ports_come_first_and_one_host_port_is_published_once() {
        let ports = |config: &str| {
            let settings = settings(config);
            let sandbox = Rc::new(FakeSandbox::default());
            let broker =
                broker(&settings, sandbox, Path::new("/nonexistent")).unwrap();
            published(broker.as_ref(), &settings)
        };
        let names = |config: &str| -> Vec<String> {
            ports(config).into_iter().map(|port| port.name).collect()
        };
        assert_eq!(names("{}"), ["vault-ui", "proxy"]);
        assert_eq!(
            names(
                r#"{"bays": [{"name": "default", "apps": ["nix", "github", "paperclip"]}]}"#
            ),
            ["vault-ui", "proxy", "paperclip"]
        );
        let clash = ports(
            r#"{"bays": [{"name": "default", "apps": ["nix", "github", "paperclip"]}],
                "tower": {"agentVault": {"adminPort": 3100}}}"#,
        );
        assert_eq!(
            check_ports(&clash).unwrap_err().to_string(),
            "port 3100: vault-ui and default/paperclip: set ports.paperclip \
             in bay default"
        );
        let two = r#"{"bays": [{"name": "work", "apps": ["nix", "github", "paperclip"]},
                               {"name": "oss", "apps": ["nix", "github", "paperclip"]}]}"#;
        assert_eq!(
            check_ports(&ports(two)).unwrap_err().to_string(),
            "port 3100: work/paperclip and oss/paperclip: set ports.paperclip \
             in bay oss"
        );
        let moved = two.replace(
            r#""name": "oss", "apps": ["nix", "github", "paperclip"]"#,
            r#""name": "oss", "apps": ["nix", "github", "paperclip"],
                "ports": {"paperclip": 3200}"#,
        );
        assert!(check_ports(&ports(&moved)).is_ok());
        assert!(check_ports(&ports("{}")).is_ok());
    }

    #[test]
    fn ports_are_probed_over_http_or_tcp_unless_only_a_vm_uses_them() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let open = listener.local_addr().unwrap().port();
        let closed = closed_port();
        let port = |host, http, bay: Option<&str>| PublishedPort {
            name: "p".into(),
            app: None,
            bay: bay.map(Into::into),
            host,
            vm: host,
            purpose: "p".into(),
            http,
        };
        let probed = Ports::probe(vec![
            port(open, false, Some("b")),
            port(closed, false, Some("b")),
            port(closed, true, None),
            port(open, false, None),
        ]);
        let reachable: Vec<Option<bool>> =
            probed.0.iter().map(|status| status.reachable).collect();
        assert_eq!(reachable, [Some(true), Some(false), Some(false), None]);
        drop(listener);
    }
}
