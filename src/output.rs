//! Commands return results; this module renders them, as text or JSON
//! (`--json`), on stdout. Errors go through here too, on stderr. Progress is
//! `log` on stderr and never mixes into stdout.

use std::path::Path;

use miniserde::json::{Number, Object};

use crate::commands::UpReport;
use crate::error::Error;
use crate::files::Copied;
use crate::json::{self, Json};
use crate::lifecycle::{self, Change};
use crate::logger;
use crate::mounts::MountKind;
use crate::proxy::Health;
use crate::sandbox::BoxState;
use crate::status::{self, BayStatus, Detail, Overview, ProxyStatus};

/// Every JSON document's `version`: 1 until the first release, then bumped
/// when a field changes meaning or goes away.
const VERSION: u64 = 1;

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
    /// The command wrote its own output.
    Streamed,
    /// What `up`, `down`, `restart` and `rm` did, one line each.
    Changes(UpReport),
    Overview(Overview),
    Detail(Detail),
    Copied(Copied),
}

impl Outcome {
    /// 1 when any change failed, 3 when `ps` detected an event.
    pub(crate) fn exit_code(&self) -> u8 {
        let failed = |change: &Change| {
            matches!(change.outcome, lifecycle::Outcome::Failed(_))
        };
        match self {
            Self::Changes(report) if report.changes.iter().any(failed) => 1,
            Self::Overview(overview)
                if !status::events(overview).is_empty() =>
            {
                3
            }
            _ => 0,
        }
    }
}

/// What changed goes to the log file too, so it stays a full record.
fn record(result: &Outcome) {
    if matches!(result, Outcome::Changes(_) | Outcome::Copied(_)) {
        for line in result.human().lines() {
            log::info!(target: logger::REPORT, "{line}");
        }
    }
}

pub(crate) fn emit(format: Format, result: &Outcome) {
    record(result);
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
        Format::Json => {
            log::error!(target: logger::REPORT, "{error}");
            eprintln!("{}", json::stringify(&error_json(error)));
        }
    }
}

fn error_json(error: &Error) -> Json {
    let hint = error.hint().map_or(Json::Null, json::string);
    json::object([("error", json::string(error.message())), ("hint", hint)])
}

fn document<const N: usize>(fields: [(&str, Json); N]) -> Json {
    let mut object = Object::new();
    object.insert("version".into(), Json::Number(Number::U64(VERSION)));
    for (key, value) in fields {
        object.insert(key.into(), value);
    }
    Json::Object(object)
}

fn strings<S: AsRef<str>>(items: &[S]) -> Json {
    Json::Array(
        items
            .iter()
            .map(|item| json::string(item.as_ref()))
            .collect(),
    )
}

fn optional(text: Option<&str>) -> Json {
    text.map_or(Json::Null, json::string)
}

fn path(path: &Path) -> Json {
    json::string(&path.display().to_string())
}

impl Render for Outcome {
    fn human(&self) -> String {
        match self {
            Self::Done | Self::Streamed => String::new(),
            Self::Changes(report) => {
                lines(report.changes.iter().map(change_line))
            }
            Self::Overview(overview) => lines(table(&overview_rows(overview))),
            Self::Detail(detail) => detail_text(detail),
            Self::Copied(copied) => lines(
                copied
                    .copied
                    .iter()
                    .map(|path| format!("copied {path}"))
                    .chain(
                        copied
                            .removed
                            .iter()
                            .map(|path| format!("removed {path}")),
                    ),
            ),
        }
    }

    fn json(&self) -> Json {
        match self {
            Self::Done => document([("ok", Json::Bool(true))]),
            Self::Streamed => Json::Null,
            Self::Changes(report) => document([(
                "changes",
                Json::Array(report.changes.iter().map(change_json).collect()),
            )]),
            Self::Overview(overview) => overview_json(overview),
            Self::Detail(detail) => detail_json(detail),
            Self::Copied(copied) => document([
                ("copied", strings(&copied.copied)),
                ("removed", strings(&copied.removed)),
            ]),
        }
    }
}

fn lines(lines: impl IntoIterator<Item = String>) -> String {
    lines.into_iter().collect::<Vec<_>>().join("\n")
}

/// The outcome's word and its reasons (or changes).
fn outcome_parts(outcome: &lifecycle::Outcome) -> (&'static str, &[String]) {
    use lifecycle::Outcome::{
        Created, Failed, Recreated, Removed, Stopped, Unchanged, Updated,
    };
    match outcome {
        Created => ("created", &[]),
        Recreated(reasons) => ("recreated", reasons),
        Updated(changes) => ("updated", changes),
        Unchanged => ("unchanged", &[]),
        Stopped => ("stopped", &[]),
        Removed => ("removed", &[]),
        Failed(_) => ("failed", &[]),
    }
}

fn change_line(change: &Change) -> String {
    let (word, reasons) = outcome_parts(&change.outcome);
    let what = match &change.outcome {
        lifecycle::Outcome::Failed(error) => match error.hint() {
            Some(hint) => format!("{word}: {} (try: {hint})", error.message()),
            None => format!("{word}: {}", error.message()),
        },
        _ if reasons.is_empty() => word.to_string(),
        lifecycle::Outcome::Recreated(_) => {
            format!("{word} ({})", reasons.join(", "))
        }
        _ => format!("{word} ({})", reasons.join("; ")),
    };
    format!("{}: {what}", change.name)
}

fn change_json(change: &Change) -> Json {
    let (word, reasons) = outcome_parts(&change.outcome);
    let error = match &change.outcome {
        lifecycle::Outcome::Failed(error) => error_json(error),
        _ => Json::Null,
    };
    json::object([
        ("name", json::string(&change.name)),
        ("type", json::string(change.resource.as_str())),
        ("outcome", json::string(word)),
        ("reasons", strings(reasons)),
        ("error", error),
    ])
}

fn overview_rows(overview: &Overview) -> Vec<Vec<String>> {
    let header = cells(["NAME", "TYPE", "STATE", "PROXY", "IMAGE"]);
    let bays = overview.bays.iter().map(|bay| {
        let proxy = bay.proxy.as_deref().unwrap_or("direct");
        cells([&bay.name, "bay", state_word(bay.state), proxy, &bay.image])
    });
    let proxies = overview.proxies.iter().map(|proxy| {
        let image = proxy.image.as_deref().unwrap_or("-");
        let state = proxy_state(proxy).unwrap_or("external");
        cells([&proxy.name, "proxy", state, "-", image])
    });
    let leftovers = overview.leftovers.iter().map(|(resource, name)| {
        cells([name, resource.as_str(), "leftover", "-", "-"])
    });
    std::iter::once(header)
        .chain(bays)
        .chain(proxies)
        .chain(leftovers)
        .collect()
}

fn overview_json(overview: &Overview) -> Json {
    let bays = overview.bays.iter().map(|bay| {
        json::object([
            ("name", json::string(&bay.name)),
            ("state", json::string(state_word(bay.state))),
            ("proxy", optional(bay.proxy.as_deref())),
            ("image", json::string(&bay.image)),
            ("apps", apps_json(bay)),
        ])
    });
    let proxies = overview.proxies.iter().map(|proxy| {
        json::object([
            ("name", json::string(&proxy.name)),
            ("kind", json::string(proxy.kind)),
            ("state", optional(proxy_state(proxy))),
            ("image", optional(proxy.image.as_deref())),
            ("health", optional(health_word(proxy))),
        ])
    });
    let leftovers = overview.leftovers.iter().map(|(resource, name)| {
        json::object([
            ("name", json::string(name)),
            ("type", json::string(resource.as_str())),
        ])
    });
    document([
        ("bays", Json::Array(bays.collect())),
        ("proxies", Json::Array(proxies.collect())),
        ("leftovers", Json::Array(leftovers.collect())),
    ])
}

/// `unknown` when the sandbox couldn't tell.
fn state_word(state: Option<BoxState>) -> &'static str {
    state.map_or("unknown", BoxState::as_str)
}

/// `None` for a proxy that runs elsewhere.
fn proxy_state(proxy: &ProxyStatus) -> Option<&'static str> {
    proxy.managed.then(|| state_word(proxy.state))
}

fn health_word(proxy: &ProxyStatus) -> Option<&'static str> {
    match proxy.health {
        None => None,
        Some(Health::Up) => Some("up"),
        Some(Health::Down(_)) => Some("unreachable"),
    }
}

fn apps_json(bay: &BayStatus) -> Json {
    Json::Array(
        bay.apps
            .iter()
            .map(|(name, state)| {
                json::object([
                    ("name", json::string(name)),
                    ("state", json::string(state.as_str())),
                ])
            })
            .collect(),
    )
}

fn mode(writable: bool) -> &'static str {
    if writable { "rw" } else { "ro" }
}

fn mount_kind(kind: MountKind) -> &'static str {
    match kind {
        MountKind::User => "user",
        MountKind::Home => "home",
        MountKind::Cache => "cache",
    }
}

fn detail_text(detail: &Detail) -> String {
    let mut text = Vec::new();
    match detail {
        Detail::Bay(bay) => {
            let status = &bay.status;
            text.push(format!("name: {}", status.name));
            text.push("type: bay".into());
            text.push(format!("state: {}", state_word(status.state)));
            let proxy = status.proxy.as_deref().unwrap_or("direct");
            text.push(format!("proxy: {proxy}"));
            text.push(format!("image: {}", status.image));
            text.push(format!("vm: {}", bay.vm));
            text.push(format!("home: {}", bay.home.display()));
            text.push(format!("cache: {}", bay.cache.display()));
            text.push(format!("volume: {}", bay.volume));
            text.push(format!("apps: {}", bay.apps.join(" ")));
            text.push(format!("packages: {}", bay.packages.join(" ")));
            text.push("mounts:".into());
            text.extend(bay.mounts.iter().map(|mount| {
                format!(
                    "  {} <- {} ({})",
                    mount.vm,
                    mount.host,
                    mode(mount.writable)
                )
            }));
        }
        Detail::Proxy(proxy) => {
            let status = &proxy.status;
            text.push(format!("name: {}", status.name));
            text.push("type: proxy".into());
            text.push(format!("kind: {}", status.kind));
            let state = proxy_state(status).unwrap_or("external");
            text.push(format!("state: {state}"));
            if let (Some(vm), Some(image)) = (&proxy.vm, &status.image) {
                text.push(format!("vm: {vm}"));
                text.push(format!("image: {image}"));
            }
            text.push(format!("data: {}", proxy.data.display()));
            if let Some(log) = &proxy.log {
                text.push(format!("log: {}", log.display()));
            }
        }
    }
    text.join("\n")
}

fn detail_json(detail: &Detail) -> Json {
    match detail {
        Detail::Bay(bay) => {
            let status = &bay.status;
            let mounts = bay.mounts.iter().map(|mount| {
                json::object([
                    ("vm", json::string(&mount.vm)),
                    ("host", json::string(&mount.host)),
                    ("mode", json::string(mode(mount.writable))),
                    ("kind", json::string(mount_kind(mount.kind))),
                    ("applied", mount.applied.map_or(Json::Null, Json::Bool)),
                ])
            });
            document([
                ("name", json::string(&status.name)),
                ("type", json::string("bay")),
                ("state", json::string(state_word(status.state))),
                ("proxy", optional(status.proxy.as_deref())),
                ("image", json::string(&status.image)),
                ("vm", json::string(&bay.vm)),
                ("home", path(&bay.home)),
                ("cache", path(&bay.cache)),
                ("volume", json::string(&bay.volume)),
                ("apps", strings(&bay.apps)),
                ("packages", strings(&bay.packages)),
                ("mounts", Json::Array(mounts.collect())),
            ])
        }
        Detail::Proxy(proxy) => {
            let status = &proxy.status;
            document([
                ("name", json::string(&status.name)),
                ("type", json::string("proxy")),
                ("kind", json::string(status.kind)),
                ("state", optional(proxy_state(status))),
                ("image", optional(status.image.as_deref())),
                ("vm", optional(proxy.vm.as_deref())),
                ("data", path(&proxy.data)),
                ("log", proxy.log.as_deref().map_or(Json::Null, path)),
            ])
        }
    }
}

/// Rows padded to each column's widest cell, two spaces apart; nothing
/// without rows below the header.
fn table(rows: &[Vec<String>]) -> Vec<String> {
    if rows.len() < 2 {
        return Vec::new();
    }
    let mut widths: Vec<usize> = Vec::new();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            let width = cell.chars().count();
            match widths.get_mut(index) {
                Some(widest) => *widest = (*widest).max(width),
                None => widths.push(width),
            }
        }
    }
    rows.iter()
        .map(|row| {
            let cells: Vec<String> = row
                .iter()
                .zip(&widths)
                .map(|(cell, width)| format!("{cell:<width$}"))
                .collect();
            cells.join("  ").trim_end().to_string()
        })
        .collect()
}

fn cells<const N: usize>(values: [&str; N]) -> Vec<String> {
    values.map(String::from).to_vec()
}

#[cfg(test)]
mod tests {
    use super::{Outcome, Render, error_json};
    use crate::commands::UpReport;
    use crate::error::Error;
    use crate::lifecycle::{self, Change, Resource};
    use crate::sandbox::BoxState;
    use crate::status::{
        BayStatus, Detail, Overview, ProxyDetail, ProxyStatus, RunState,
    };
    use std::path::PathBuf;

    fn changes() -> Outcome {
        use lifecycle::Outcome::{
            Created, Failed, Recreated, Removed, Stopped, Unchanged, Updated,
        };
        let outcomes = [
            ("p", Resource::Proxy, Created),
            (
                "a",
                Resource::Bay,
                Recreated(vec!["image".into(), "ports".into()]),
            ),
            (
                "b",
                Resource::Bay,
                Updated(vec!["env".into(), "copies +x".into()]),
            ),
            ("c", Resource::Bay, Unchanged),
            ("d", Resource::Bay, Stopped),
            ("e", Resource::Bay, Removed),
            (
                "f",
                Resource::Bay,
                Failed(Error::with_hint("boom", "retry")),
            ),
            ("g", Resource::Bay, Failed(Error::new("bust"))),
        ];
        let changes = outcomes
            .into_iter()
            .map(|(name, resource, outcome)| Change {
                name: name.into(),
                resource,
                outcome,
            })
            .collect();
        Outcome::Changes(UpReport { changes })
    }

    #[test]
    fn up_summary_lines() {
        let outcome = changes();
        assert_eq!(
            outcome.human(),
            "p: created\n\
             a: recreated (image, ports)\n\
             b: updated (env; copies +x)\n\
             c: unchanged\n\
             d: stopped\n\
             e: removed\n\
             f: failed: boom (try: retry)\n\
             g: failed: bust"
        );
        assert_eq!(outcome.exit_code(), 1);
        let ok = Outcome::Changes(UpReport {
            changes: Vec::new(),
        });
        assert_eq!(ok.exit_code(), 0);
        assert_eq!(Outcome::Done.exit_code(), 0);
    }

    #[test]
    fn up_json_shape() {
        let json = to_string(&changes().json());
        assert!(json.starts_with(
            r#"{"changes":[{"error":null,"name":"p","outcome":"created","reasons":[],"type":"proxy"},{"error":null,"name":"a","outcome":"recreated","reasons":["image","ports"],"type":"bay"}"#
        ), "{json}");
        assert!(json.contains(
            r#"{"error":{"error":"boom","hint":"retry"},"name":"f","outcome":"failed","reasons":[],"type":"bay"}"#
        ), "{json}");
        assert!(json.ends_with(r#""version":1}"#), "{json}");
    }

    fn proxy_status(state: Option<BoxState>) -> ProxyStatus {
        ProxyStatus {
            name: "p".into(),
            kind: "fake",
            managed: state.is_some(),
            state,
            image: state.map(|_| "fake:1".into()),
            health: None,
        }
    }

    #[test]
    fn overview_lists_bays_proxies_and_leftovers() {
        let overview = Outcome::Overview(Overview {
            bays: vec![BayStatus {
                name: "work".into(),
                state: Some(BoxState::Running),
                proxy: Some("p".into()),
                image: "img:1".into(),
                apps: vec![("web".into(), RunState::Exited)],
            }],
            proxies: vec![
                proxy_status(Some(BoxState::Stopped)),
                ProxyStatus {
                    name: "ext".into(),
                    ..proxy_status(None)
                },
            ],
            leftovers: vec![(Resource::Proxy, "gone".into())],
        });
        assert_eq!(
            overview.human(),
            "NAME  TYPE   STATE     PROXY  IMAGE\n\
             work  bay    running   p      img:1\n\
             p     proxy  stopped   -      fake:1\n\
             ext   proxy  external  -      -\n\
             gone  proxy  leftover  -      -"
        );
        assert_eq!(
            to_string(&overview.json()),
            r#"{"bays":[{"apps":[{"name":"web","state":"exited"}],"image":"img:1","name":"work","proxy":"p","state":"running"}],"leftovers":[{"name":"gone","type":"proxy"}],"proxies":[{"health":null,"image":"fake:1","kind":"fake","name":"p","state":"stopped"},{"health":null,"image":null,"kind":"fake","name":"ext","state":null}],"version":1}"#
        );
    }

    #[test]
    fn a_proxy_detail_names_its_vm_data_and_log() {
        let detail = |state, vm: Option<&str>| {
            Outcome::Detail(Detail::Proxy(ProxyDetail {
                status: proxy_status(state),
                vm: vm.map(String::from),
                data: PathBuf::from("/d/proxies/p/data"),
                log: vm.map(|_| PathBuf::from("/d/p.log")),
            }))
        };
        let managed = detail(Some(BoxState::Running), Some("hangar-proxy-p"));
        assert_eq!(
            managed.human(),
            "name: p\ntype: proxy\nkind: fake\nstate: running\n\
             vm: hangar-proxy-p\nimage: fake:1\ndata: /d/proxies/p/data\n\
             log: /d/p.log"
        );
        assert!(to_string(&managed.json()).contains(r#""log":"/d/p.log""#));
        let external = detail(None, None);
        assert_eq!(
            external.human(),
            "name: p\ntype: proxy\nkind: fake\nstate: external\n\
             data: /d/proxies/p/data"
        );
        assert!(to_string(&external.json()).contains(r#""log":null"#));
    }

    #[test]
    fn copied_lists_paths() {
        let copied = Outcome::Copied(crate::files::Copied {
            copied: vec!["/a".into()],
            removed: vec!["/b".into()],
        });
        assert_eq!(copied.human(), "copied /a\nremoved /b");
        assert_eq!(
            to_string(&copied.json()),
            r#"{"copied":["/a"],"removed":["/b"],"version":1}"#
        );
    }

    fn to_string(value: &crate::json::Json) -> String {
        crate::json::stringify(value)
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
        let hinted = Error::with_hint("the proxy isn't running", "run up");
        assert_eq!(
            to_string(&error_json(&hinted)),
            r#"{"error":"the proxy isn't running","hint":"run up"}"#
        );
        assert_eq!(
            to_string(&error_json(&Error::new("plain"))),
            r#"{"error":"plain","hint":null}"#
        );
    }
}
