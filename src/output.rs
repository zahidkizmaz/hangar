//! Commands return results; this module renders them, as text or JSON
//! (`--json`), on stdout. Errors go through here too, on stderr. Progress is
//! `log` on stderr and never mixes into stdout.

use miniserde::json::{Array, Number, Object};

use crate::broker::Policy;
use crate::credential::CredentialList;
use crate::error::Error;
use crate::files::Copied;
use crate::json::{self, Json};
use crate::mounts::{MountKind, MountStatus};
use crate::overview::{
    AppHosts, BayStatus, PortStatus, Ports, RunState, StatusReport, UpReport,
    VaultLogin, Vm,
};
use crate::sandbox::{BoxState, PublishedPort};

/// Every JSON document's `version`: 1 until the first release, then bumped
/// when a field changes meaning or goes away.
const VERSION: u64 = 1;

const UNHEALTHY: u8 = 3;

#[derive(Clone, Copy)]
pub(crate) enum Format {
    Human,
    Json,
}

trait Render {
    /// Text for people; empty prints nothing.
    fn human(&self) -> String;
    /// The documented `--json` shape; `Null` prints nothing.
    fn json(&self) -> Json;
}

pub(crate) enum Outcome {
    Done,
    /// The command wrote its own output (`shell`, `logs`).
    Streamed,
    Status(StatusReport),
    /// `up` ends with the ports; as JSON, the whole status and the bays
    /// that failed.
    Up(UpReport),
    Credentials(CredentialList),
    VaultLogin(VaultLogin),
    Copied(String, Copied),
    Restarted(String, Vec<String>),
}

impl Outcome {
    /// `up` exits 1 when a bay failed, never 3: health is `status`'s.
    pub(crate) fn exit_code(&self) -> u8 {
        match self {
            Self::Status(status) if !status.healthy() => UNHEALTHY,
            Self::Up(up) if !up.failed.is_empty() => 1,
            _ => 0,
        }
    }
}

pub(crate) fn emit(format: Format, result: &Outcome) {
    match format {
        Format::Human => {
            let text = result.human();
            if !text.is_empty() {
                println!("{text}");
            }
        }
        Format::Json => {
            let value = result.json();
            if !matches!(value, Json::Null) {
                println!("{}", json::stringify(&value));
            }
        }
    }
}

pub(crate) fn emit_error(format: Format, error: &Error) {
    match format {
        Format::Human => log::error!("{error}"),
        Format::Json => eprintln!("{}", error_json(error)),
    }
}

fn error_json(error: &Error) -> String {
    let hint = error.hint().map_or(Json::Null, json::string);
    let value = json::object([
        ("error", json::string(error.message())),
        ("hint", hint),
    ]);
    json::stringify(&value)
}

fn document<const N: usize>(fields: [(&str, Json); N]) -> Json {
    let mut object = Object::new();
    object.insert("version".into(), Json::Number(Number::U64(VERSION)));
    for (key, value) in fields {
        object.insert(key.into(), value);
    }
    Json::Object(object)
}

impl Render for Outcome {
    fn human(&self) -> String {
        match self {
            Self::Done | Self::Streamed => String::new(),
            Self::Status(status) => status.human(),
            Self::Up(up) => up
                .status
                .bays
                .iter()
                .flat_map(|bay| bay.apps.iter().map(app_line))
                .chain([port_table(&up.status)])
                .collect::<Vec<_>>()
                .join("\n"),
            Self::Credentials(list) => list.human(),
            Self::VaultLogin(login) => login.human(),
            Self::Copied(_, copied) => copied_lines(copied),
            Self::Restarted(_, names) => names
                .iter()
                .map(|name| format!("restarted {name}"))
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    fn json(&self) -> Json {
        match self {
            Self::Done => document([("ok", Json::Bool(true))]),
            Self::Streamed => Json::Null,
            Self::Status(status) => status.json(),
            Self::Up(up) => up.json(),
            Self::Credentials(list) => list.json(),
            Self::VaultLogin(login) => login.json(),
            Self::Copied(bay, copied) => document([
                ("ok", Json::Bool(true)),
                ("bay", json::string(bay)),
                ("copied", strings(&copied.copied)),
                ("removed", strings(&copied.removed)),
            ]),
            Self::Restarted(bay, names) => document([
                ("ok", Json::Bool(true)),
                ("bay", json::string(bay)),
                ("restarted", strings(names)),
            ]),
        }
    }
}

fn copied_lines(copied: &Copied) -> String {
    let lines: Vec<String> = copied
        .copied
        .iter()
        .map(|path| format!("copied {path}"))
        .chain(copied.removed.iter().map(|path| format!("removed {path}")))
        .collect();
    if lines.is_empty() {
        "nothing changed".into()
    } else {
        lines.join("\n")
    }
}

fn strings(values: &[String]) -> Json {
    Json::Array(values.iter().map(|value| json::string(value)).collect())
}

impl Render for StatusReport {
    fn human(&self) -> String {
        let mut lines =
            vec![format!("{}: {}", self.tower.name, self.tower.shown)];
        lines.push(if self.broker.healthy {
            format!(
                "vault: healthy, unlisted hosts: {}",
                self.broker.unlisted.map_or("unknown", Policy::as_str)
            )
        } else {
            "vault: unreachable".into()
        });
        for bay in &self.bays {
            bay_lines(bay, &mut lines);
        }
        for leftover in &self.leftovers {
            lines.push(format!(
                "{}: {} (bay {} is not in the config: {})",
                leftover.vm.name,
                leftover.vm.shown,
                leftover.name,
                leftover_fix(&leftover.name, &leftover.vm)
            ));
        }
        lines.push(port_table(self));
        lines.join("\n")
    }

    fn json(&self) -> Json {
        self.json_with([])
    }
}

impl StatusReport {
    fn json_with<const N: usize>(&self, extra: [(&str, Json); N]) -> Json {
        let unlisted = self
            .broker
            .unlisted
            .map_or(Json::Null, |policy| json::string(policy.as_str()));
        let mut doc = document([
            ("healthy", Json::Bool(self.healthy())),
            (
                "tower",
                json::object([
                    ("vm", json::string(self.tower.state_str())),
                    ("backend", json::string(&self.broker_backend)),
                    ("reachable", Json::Bool(self.broker.reachable)),
                    ("healthy", Json::Bool(self.broker.healthy)),
                    ("unlistedHosts", unlisted),
                    ("ports", ports_json(&self.tower_ports)),
                ]),
            ),
            (
                "bays",
                Json::Array(self.bays.iter().map(bay_json).collect()),
            ),
            (
                "leftovers",
                Json::Array(
                    self.leftovers
                        .iter()
                        .map(|leftover| {
                            json::object([
                                ("name", json::string(&leftover.name)),
                                ("vm", json::string(&leftover.vm.name)),
                                (
                                    "state",
                                    json::string(leftover.vm.state_str()),
                                ),
                            ])
                        })
                        .collect(),
                ),
            ),
        ]);
        if let Json::Object(object) = &mut doc {
            for (key, value) in extra {
                object.insert(key.into(), value);
            }
        }
        doc
    }
}

impl UpReport {
    fn json(&self) -> Json {
        let failed = self
            .failed
            .iter()
            .map(|(name, error)| {
                json::object([
                    ("name", json::string(name)),
                    ("error", json::string(error)),
                ])
            })
            .collect();
        self.status.json_with([("failed", Json::Array(failed))])
    }
}

/// The fix for a bay that's no longer configured: its VM first, then its
/// folder.
pub(crate) fn leftover_fix(name: &str, vm: &Vm) -> String {
    if vm.state == Some(BoxState::Missing) {
        format!("hangar destroy {name} --state")
    } else {
        format!("hangar destroy {name}")
    }
}

fn bay_lines(bay: &BayStatus, lines: &mut Vec<String>) {
    lines.push(format!("{}: {}", bay.vm.name, bay.vm.shown));
    lines.extend(bay.apps.iter().map(app_line));
    if bay.vm.running() {
        for (name, state) in &bay.run {
            let shown = match state {
                RunState::Unknown(why) => format!("unknown ({why})"),
                RunState::Stopped if bay.has_setup(name) => {
                    format!("stopped (needs setup? hangar setup {name})")
                }
                known => known.as_str().to_string(),
            };
            lines.push(format!("run {name}: {shown}"));
        }
    }
    let vm_exists = bay.vm.state.is_some_and(|s| s != BoxState::Missing);
    for mount in &bay.mounts {
        let mode = match (mount.kind, mount.writable) {
            (MountKind::Home, _) => "rw, home",
            (MountKind::Cache, _) => "rw, cache",
            (MountKind::User, true) => "rw",
            (MountKind::User, false) => "ro",
        };
        let mut shown =
            format!("mount {} <- {} ({mode})", mount.vm, mount.host);
        let missing = match mount.applied {
            _ if !vm_exists => None,
            Some(true) => None,
            Some(false) => Some("not in the VM"),
            None => Some("unknown (no record of the VM)"),
        };
        if let Some(missing) = missing {
            shown = format!(
                "{shown}, {missing}: hangar destroy {name} && hangar up {name}",
                name = bay.name
            );
        }
        lines.push(shown);
    }
    if let Some(cache) = &bay.cache {
        lines.push(format!(
            "package cache: {} ({})",
            size(cache.bytes),
            cache.host
        ));
    }
}

fn bay_json(bay: &BayStatus) -> Json {
    let run = bay
        .run
        .iter()
        .map(|(name, state)| (name.clone(), json::string(state.as_str())))
        .collect::<Object>();
    json::object([
        ("name", json::string(&bay.name)),
        ("vm", json::string(&bay.vm.name)),
        ("state", json::string(bay.vm.state_str())),
        ("healthy", Json::Bool(bay.healthy())),
        (
            "apps",
            Json::Object(
                bay.apps
                    .iter()
                    .map(|app| (app.name.clone(), app_json(app)))
                    .collect(),
            ),
        ),
        ("run", Json::Object(run)),
        (
            "mounts",
            Json::Array(bay.mounts.iter().map(mount_json).collect()),
        ),
        (
            "cache",
            bay.cache.as_ref().map_or(Json::Null, |cache| {
                json::object([
                    ("host", json::string(&cache.host)),
                    ("bytes", Json::Number(Number::U64(cache.bytes))),
                ])
            }),
        ),
        ("ports", ports_json(&bay.ports)),
    ])
}

/// Every published port, the tower's first; a bay's are named `bay/port`
/// once there's more than one bay.
fn port_table(status: &StatusReport) -> String {
    let qualified = status.bays.len() > 1;
    let ports: Vec<&PortStatus> = status
        .tower_ports
        .0
        .iter()
        .chain(status.bays.iter().flat_map(|bay| &bay.ports.0))
        .collect();
    port_lines(&ports, qualified)
}

/// `applied` is `null` when the VM has no record of its mounts.
fn mount_json(mount: &MountStatus) -> Json {
    json::object([
        ("vm", json::string(&mount.vm)),
        ("host", json::string(&mount.host)),
        ("writable", Json::Bool(mount.writable)),
        ("home", Json::Bool(mount.kind == MountKind::Home)),
        ("cache", Json::Bool(mount.kind == MountKind::Cache)),
        ("applied", mount.applied.map_or(Json::Null, Json::Bool)),
    ])
}

/// `app NAME: host ← CREDENTIAL, host, …`.
fn app_line(app: &AppHosts) -> String {
    let hosts = if app.routes.is_empty() {
        "no hosts".to_string()
    } else {
        app.routes
            .iter()
            .map(|(_, host, credential)| match credential {
                Some(credential) => format!("{host} ← {credential}"),
                None => host.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!("app {}: {hosts}", app.name)
}

fn app_json(app: &AppHosts) -> Json {
    let routes = app
        .routes
        .iter()
        .map(|(name, host, credential)| {
            json::object([
                ("name", json::string(name)),
                ("host", json::string(host)),
                (
                    "credential",
                    credential.as_deref().map_or(Json::Null, json::string),
                ),
            ])
        })
        .collect();
    json::object([("routes", Json::Array(routes))])
}

fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    #[expect(clippy::cast_precision_loss, reason = "display only")]
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn port_lines(ports: &[&PortStatus], qualified: bool) -> String {
    let label = |port: &PublishedPort| {
        if qualified {
            port.owner()
        } else {
            port.name.clone()
        }
    };
    let urls: Vec<String> =
        ports.iter().map(|status| url(&status.port)).collect();
    let labels: Vec<String> =
        ports.iter().map(|status| label(&status.port)).collect();
    let name_width = labels.iter().map(String::len).max().unwrap_or(0);
    let url_width = urls.iter().map(String::len).max().unwrap_or(0);
    ports
        .iter()
        .zip(urls.iter().zip(&labels))
        .map(|(status, (url, name))| {
            let reach = match status.reachable {
                Some(true) => "reachable",
                Some(false) => "unreachable",
                None => "-",
            };
            format!(
                "{name:<name_width$}  {url:<url_width$}  {reach:<11}  {}",
                status.port.purpose
            )
        })
        .chain(["all ports bind to 127.0.0.1 only".to_string()])
        .collect::<Vec<_>>()
        .join("\n")
}

fn url(port: &PublishedPort) -> String {
    if port.http {
        format!("http://127.0.0.1:{}", port.host)
    } else {
        format!("127.0.0.1:{}", port.host)
    }
}

fn ports_json(ports: &Ports) -> Json {
    Json::Array(
        ports
            .0
            .iter()
            .map(|status| {
                let port = &status.port;
                json::object([
                    ("name", json::string(&port.name)),
                    (
                        "app",
                        port.app.as_deref().map_or(Json::Null, json::string),
                    ),
                    ("url", Json::String(url(port))),
                    ("purpose", json::string(&port.purpose)),
                    (
                        "reachable",
                        status.reachable.map_or(Json::Null, Json::Bool),
                    ),
                ])
            })
            .collect(),
    )
}

impl Render for CredentialList {
    fn human(&self) -> String {
        self.entries
            .iter()
            .map(|(name, source)| format!("{name}\t{source}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn json(&self) -> Json {
        let credentials = self
            .entries
            .iter()
            .map(|(name, source)| {
                json::object([
                    ("name", json::string(name)),
                    ("source", json::string(source)),
                ])
            })
            .collect::<Array>();
        document([("credentials", Json::Array(credentials))])
    }
}

impl Render for VaultLogin {
    fn human(&self) -> String {
        let password = if self.copied {
            "copied to the clipboard".to_string()
        } else {
            format!("in {} (not shown)", self.password_file)
        };
        format!(
            "vault UI: {}\nlogin:    {}\npassword: {password}",
            self.url, self.login
        )
    }

    fn json(&self) -> Json {
        let password = if self.copied { "clipboard" } else { "file" };
        document([
            ("url", json::string(&self.url)),
            ("login", json::string(&self.login)),
            ("password", json::string(password)),
            ("passwordFile", json::string(&self.password_file)),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Outcome, Render, UNHEALTHY, error_json, port_lines, ports_json,
    };
    use crate::broker::{BrokerHealth, Policy};
    use crate::credential::CredentialList;
    use crate::error::Error;
    use crate::mounts::{MountKind, MountStatus};
    use crate::overview::{
        AppHosts, BayStatus, Cache, Leftover, PortStatus, Ports, RunState,
        StatusReport, UpReport, VaultLogin, Vm,
    };
    use crate::sandbox::{BoxState, PublishedPort};

    fn to_string(value: &crate::json::Json) -> String {
        crate::json::stringify(value)
    }

    fn port(
        name: &str,
        app: Option<&str>,
        host: u16,
        http: bool,
        reachable: Option<bool>,
    ) -> PortStatus {
        PortStatus {
            port: PublishedPort {
                name: name.into(),
                app: app.map(Into::into),
                bay: app.map(|_| "default".into()),
                host,
                vm: host,
                purpose: format!("{name} purpose"),
                http,
            },
            reachable,
        }
    }

    fn tower_ports() -> Ports {
        Ports(vec![
            port("vault-ui", None, 15000, true, Some(true)),
            port("proxy", None, 15001, false, None),
        ])
    }

    fn bay_ports() -> Ports {
        Ports(vec![
            port("web", Some("web-app"), 4000, true, Some(false)),
            port("db", Some("web-app"), 5432, false, Some(true)),
        ])
    }

    fn vm(name: &str, shown: &str, state: BoxState) -> Vm {
        Vm {
            name: name.into(),
            shown: shown.into(),
            state: Some(state),
        }
    }

    fn healthy_bay() -> BayStatus {
        BayStatus {
            name: "default".into(),
            vm: vm("hangar-bay-default", "Running", BoxState::Running),
            apps: vec![
                AppHosts {
                    name: "coder".into(),
                    routes: vec![
                        (
                            "coder-api".into(),
                            "api.example.com".into(),
                            Some("CODER_TOKEN".into()),
                        ),
                        ("coder-cdn".into(), "cdn.example.com".into(), None),
                    ],
                    has_setup: false,
                },
                AppHosts {
                    name: "web-app".into(),
                    routes: Vec::new(),
                    has_setup: false,
                },
            ],
            run: vec![("web-app".into(), RunState::Running)],
            mounts: Vec::new(),
            cache: None,
            ports: bay_ports(),
        }
    }

    fn healthy_status() -> StatusReport {
        StatusReport {
            tower: vm("hangar-tower", "Running", BoxState::Running),
            broker_backend: "agent-vault".into(),
            broker: BrokerHealth {
                reachable: true,
                healthy: true,
                unlisted: Some(Policy::Deny),
            },
            tower_ports: tower_ports(),
            bays: vec![healthy_bay()],
            leftovers: Vec::new(),
        }
    }

    #[test]
    fn ports_render_their_purpose_and_reachability() {
        let mut all = tower_ports().0;
        all.extend(bay_ports().0);
        let ports = Ports(all);
        assert_eq!(
            port_lines(&ports.0.iter().collect::<Vec<_>>(), false),
            "vault-ui  http://127.0.0.1:15000  reachable    vault-ui purpose\n\
             proxy     127.0.0.1:15001         -            proxy purpose\n\
             web       http://127.0.0.1:4000   unreachable  web purpose\n\
             db        127.0.0.1:5432          reachable    db purpose\n\
             all ports bind to 127.0.0.1 only"
        );
        assert_eq!(
            to_string(&ports_json(&ports)),
            concat!(
                r#"[{"app":null,"name":"vault-ui","purpose":"vault-ui "#,
                r#"purpose","reachable":true,"url":"http://127.0.0.1:15000"},"#,
                r#"{"app":null,"name":"proxy","purpose":"proxy purpose","#,
                r#""reachable":null,"url":"127.0.0.1:15001"},"#,
                r#"{"app":"web-app","name":"web","purpose":"web purpose","#,
                r#""reachable":false,"url":"http://127.0.0.1:4000"},"#,
                r#"{"app":"web-app","name":"db","purpose":"db purpose","#,
                r#""reachable":true,"url":"127.0.0.1:5432"}]"#,
            )
        );
    }

    #[test]
    fn a_healthy_status_renders_both_ways_and_exits_0() {
        let status = healthy_status();
        assert!(status.human().starts_with(
            "hangar-tower: Running\n\
             vault: healthy, unlisted hosts: deny\n\
             hangar-bay-default: Running\n\
             app coder: api.example.com ← CODER_TOKEN, cdn.example.com\n\
             app web-app: no hosts\n\
             run web-app: running\n\
             vault-ui "
        ));
        let json = to_string(&status.json());
        assert!(
            json.starts_with(concat!(
                r#"{"bays":[{"apps":{"coder":{"routes":[{"credential":"CODER_TOKEN","#,
                r#""host":"api.example.com","name":"coder-api"},{"credential":null,"#,
                r#""host":"cdn.example.com","name":"coder-cdn"}]},"#,
                r#""web-app":{"routes":[]}},"cache":null,"healthy":true,"#,
                r#""mounts":[],"name":"default","ports":["#
            )),
            "{json}"
        );
        assert!(json.contains(r#""run":{"web-app":"running"},"state":"running","vm":"hangar-bay-default"}],"#), "{json}");
        assert!(json.contains(r#""healthy":true,"leftovers":[],"tower":{"backend":"agent-vault","healthy":true,"ports":["#), "{json}");
        assert!(json.ends_with(r#""reachable":true,"unlistedHosts":"deny","vm":"running"},"version":1}"#), "{json}");
        assert_eq!(Outcome::Status(status).exit_code(), 0);
    }

    #[test]
    fn several_bays_name_their_ports_and_leftovers_say_how_to_go() {
        let mut status = healthy_status();
        let mut oss = healthy_bay();
        oss.name = "oss".into();
        oss.vm = vm("hangar-bay-oss", "Stopped", BoxState::Stopped);
        status.bays.push(oss);
        status.leftovers = vec![
            Leftover {
                name: "old".into(),
                vm: vm("hangar-bay-old", "Running", BoxState::Running),
            },
            Leftover {
                name: "gone".into(),
                vm: vm("hangar-bay-gone", "missing", BoxState::Missing),
            },
        ];
        let human = status.human();
        assert!(
            human.contains("default/web  http://127.0.0.1:4000"),
            "{human}"
        );
        assert!(human.contains(
            "hangar-bay-old: Running (bay old is not in the config: hangar destroy old)\n\
             hangar-bay-gone: missing (bay gone is not in the config: hangar destroy gone --state)\n"
        ), "{human}");
        // A stopped configured bay is unhealthy; leftovers don't count.
        assert!(!status.healthy());
        status.bays.pop();
        assert!(status.healthy());
        let json = to_string(&status.json());
        assert!(json.contains(
            r#""leftovers":[{"name":"old","state":"running","vm":"hangar-bay-old"},"#
        ), "{json}");
    }

    #[test]
    fn mounts_show_their_mode_and_whether_the_vm_has_them() {
        let mut status = healthy_status();
        status.bays[0].mounts = vec![
            MountStatus {
                vm: "/home/pilot".into(),
                host: "/home/you/hangar/home".into(),
                writable: true,
                kind: MountKind::Home,
                applied: Some(true),
            },
            MountStatus {
                vm: "/home/pilot/.paperclip".into(),
                host: "/home/you/hangar/paperclip".into(),
                writable: true,
                kind: MountKind::User,
                applied: Some(true),
            },
            MountStatus {
                vm: "/home/pilot/skills".into(),
                host: "/home/you/skills".into(),
                writable: false,
                kind: MountKind::User,
                applied: Some(false),
            },
        ];
        let human = status.human();
        assert!(
            human.contains(
                "mount /home/pilot <- /home/you/hangar/home (rw, home)\n\
             mount /home/pilot/.paperclip <- /home/you/hangar/paperclip (rw)\n\
             mount /home/pilot/skills <- /home/you/skills (ro), not in the VM: \
             hangar destroy default && hangar up default\n"
            ),
            "{human}"
        );
        let json = to_string(&status.json());
        assert!(json.contains(concat!(
            r#""mounts":[{"applied":true,"cache":false,"home":true,"host":"/home/you/hangar/home","#,
            r#""vm":"/home/pilot","writable":true},"#,
            r#"{"applied":true,"cache":false,"home":false,"host":"/home/you/hangar/paperclip","#,
            r#""vm":"/home/pilot/.paperclip","writable":true},"#,
            r#"{"applied":false,"cache":false,"home":false,"host":"/home/you/skills","#,
            r#""vm":"/home/pilot/skills","writable":false}]"#
        )), "{json}");
        // Mounts don't make status unhealthy: up warns about them instead.
        assert_eq!(Outcome::Status(status).exit_code(), 0);
        // A missing VM has nothing to compare with.
        let mut missing = healthy_status();
        missing.bays[0].vm =
            vm("hangar-bay-default", "missing", BoxState::Missing);
        missing.bays[0].mounts = vec![MountStatus {
            vm: "/home/pilot/a".into(),
            host: "/h".into(),
            writable: false,
            kind: MountKind::User,
            applied: Some(false),
        }];
        assert!(missing.human().contains("mount /home/pilot/a <- /h (ro)\n"));
        // An existing VM without a record: unknown, `null` as JSON.
        let mut unknown = healthy_status();
        unknown.bays[0].mounts = vec![MountStatus {
            vm: "/home/pilot/a".into(),
            host: "/h".into(),
            writable: false,
            kind: MountKind::User,
            applied: None,
        }];
        assert!(unknown.human().contains(
            "mount /home/pilot/a <- /h (ro), unknown (no record of the VM): \
             hangar destroy default && hangar up default\n"
        ));
        assert!(to_string(&unknown.json()).contains(r#""applied":null"#));
    }

    #[test]
    fn the_package_cache_shows_its_mount_and_size() {
        let mut status = healthy_status();
        status.bays[0].mounts = vec![MountStatus {
            vm: "/var/cache/hangar".into(),
            host: "/c/bays/default".into(),
            writable: true,
            kind: MountKind::Cache,
            applied: Some(true),
        }];
        status.bays[0].cache = Some(Cache {
            host: "/c/bays/default".into(),
            bytes: 3 * 1024 * 1024 + 512 * 1024,
        });
        let human = status.human();
        assert!(
            human.contains(
                "mount /var/cache/hangar <- /c/bays/default (rw, cache)\n\
                 package cache: 3.5 MiB (/c/bays/default)\n"
            ),
            "{human}"
        );
        let json = to_string(&status.json());
        assert!(
            json.contains(
                r#""cache":{"bytes":3670016,"host":"/c/bays/default"}"#
            ),
            "{json}"
        );
        assert!(json.contains(r#""applied":true,"cache":true,"home":false"#));
        assert!(!to_string(&healthy_status().json()).contains(r#""cache":{"#));
        assert_eq!(super::size(0), "0 B");
        assert_eq!(super::size(2048), "2.0 KiB");
        assert_eq!(super::size(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }

    #[test]
    fn anything_unhealthy_exits_3() {
        let mut stopped_run = healthy_status();
        stopped_run.bays[0].run[0].1 = RunState::Stopped;
        let mut allow = healthy_status();
        allow.broker.unlisted = Some(Policy::Allow);
        let mut stopped_vm = healthy_status();
        stopped_vm.bays[0].vm =
            vm("hangar-bay-default", "Stopped", BoxState::Stopped);
        let mut stopped_tower = healthy_status();
        stopped_tower.tower = vm("hangar-tower", "Stopped", BoxState::Stopped);
        let mut no_vault = healthy_status();
        no_vault.broker.healthy = false;
        for status in [stopped_run, allow, stopped_vm, stopped_tower, no_vault]
        {
            assert!(!status.healthy());
            assert_eq!(Outcome::Status(status).exit_code(), UNHEALTHY);
        }
    }

    #[test]
    fn up_exits_0_or_1_and_reports_failed_bays() {
        let mut stopped = healthy_status();
        stopped.broker.healthy = false;
        let up = Outcome::Up(UpReport {
            status: stopped,
            failed: Vec::new(),
        });
        // Health is status's business: up never exits 3.
        assert_eq!(up.exit_code(), 0);
        assert!(to_string(&up.json()).contains(r#""failed":[]"#));
        let failed = Outcome::Up(UpReport {
            status: healthy_status(),
            failed: vec![("oss".into(), "boot timed out".into())],
        });
        assert_eq!(failed.exit_code(), 1);
        let json = to_string(&failed.json());
        assert!(
            json.contains(
                r#""failed":[{"error":"boot timed out","name":"oss"}]"#
            ),
            "{json}"
        );
        assert!(json.contains(r#""bays":["#), "{json}");
        assert!(failed.human().starts_with(
            "app coder: api.example.com ← CODER_TOKEN, cdn.example.com\n\
             app web-app: no hosts\nvault-ui "
        ));
    }

    #[test]
    fn a_stopped_bay_hides_run_lines() {
        let mut status = healthy_status();
        status.bays[0].vm =
            vm("hangar-bay-default", "Stopped", BoxState::Stopped);
        status.bays[0].run[0].1 = RunState::Stopped;
        assert!(!status.human().contains("run web-app"));
        let json = to_string(&status.json());
        assert!(json.contains(r#""run":{"web-app":"stopped"}"#), "{json}");
    }

    #[test]
    fn an_unreadable_run_entry_is_unknown_with_its_reason() {
        let mut status = healthy_status();
        status.bays[0].run[0].1 = RunState::Unknown("msb broke".into());
        assert!(status.human().contains("run web-app: unknown (msb broke)"));
        let json = to_string(&status.json());
        assert!(json.contains(r#""run":{"web-app":"unknown"}"#), "{json}");
        assert!(!status.healthy());
    }

    #[test]
    fn credential_lists_never_carry_values() {
        let list = CredentialList {
            entries: vec![
                ("GITHUB_TOKEN".into(), "config"),
                ("CLAUDE_CODE_OAUTH_TOKEN".into(), "user"),
            ],
        };
        assert_eq!(
            list.human(),
            "GITHUB_TOKEN\tconfig\nCLAUDE_CODE_OAUTH_TOKEN\tuser"
        );
        assert_eq!(
            to_string(&list.json()),
            concat!(
                r#"{"credentials":[{"name":"GITHUB_TOKEN","source":"config"},"#,
                r#"{"name":"CLAUDE_CODE_OAUTH_TOKEN","source":"user"}],"#,
                r#""version":1}"#,
            )
        );
        let empty = CredentialList { entries: vec![] };
        assert_eq!(empty.human(), "");
    }

    #[test]
    fn the_vault_login_names_where_the_password_is_never_the_password() {
        let login = VaultLogin {
            url: "http://127.0.0.1:14321".into(),
            login: "owner@hangar.local".into(),
            copied: false,
            password_file: "/state/owner-password".into(),
        };
        assert_eq!(
            login.human(),
            "vault UI: http://127.0.0.1:14321\nlogin:    owner@hangar.local\n\
             password: in /state/owner-password (not shown)"
        );
        assert_eq!(
            to_string(&login.json()),
            concat!(
                r#"{"login":"owner@hangar.local","password":"file","#,
                r#""passwordFile":"/state/owner-password","#,
                r#""url":"http://127.0.0.1:14321","version":1}"#,
            )
        );
        let copied = VaultLogin {
            copied: true,
            ..login
        };
        assert!(
            copied
                .human()
                .ends_with("password: copied to the clipboard")
        );
    }

    #[test]
    fn copies_and_restarts_report_the_bay_paths_and_names_only() {
        let copied = Outcome::Copied(
            "work".into(),
            crate::files::Copied {
                copied: vec!["/home/pilot/a".into()],
                removed: vec!["/home/pilot/b".into()],
            },
        );
        assert_eq!(
            copied.human(),
            "copied /home/pilot/a\nremoved /home/pilot/b"
        );
        assert_eq!(
            to_string(&copied.json()),
            r#"{"bay":"work","copied":["/home/pilot/a"],"ok":true,"removed":["/home/pilot/b"],"version":1}"#
        );
        let nothing =
            Outcome::Copied("work".into(), crate::files::Copied::default());
        assert_eq!(nothing.human(), "nothing changed");
        let restarted =
            Outcome::Restarted("work".into(), vec!["web".into(), "db".into()]);
        assert_eq!(restarted.human(), "restarted web\nrestarted db");
        assert_eq!(
            to_string(&restarted.json()),
            r#"{"bay":"work","ok":true,"restarted":["web","db"],"version":1}"#
        );
        assert_eq!(restarted.exit_code(), 0);
    }

    #[test]
    fn done_and_streamed_render_nothing_for_people() {
        assert_eq!(Outcome::Done.human(), "");
        assert_eq!(
            to_string(&Outcome::Done.json()),
            r#"{"ok":true,"version":1}"#
        );
        assert_eq!(Outcome::Streamed.human(), "");
        assert!(matches!(Outcome::Streamed.json(), crate::json::Json::Null));
    }

    #[test]
    fn errors_as_json_keep_the_hint_apart() {
        let hinted = Error::with_hint("the vault isn't running", "run up");
        assert_eq!(
            error_json(&hinted),
            r#"{"error":"the vault isn't running","hint":"run up"}"#
        );
        assert_eq!(
            error_json(&Error::new("plain")),
            r#"{"error":"plain","hint":null}"#
        );
    }
}
