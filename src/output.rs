//! Commands return results; this module renders them, as text or JSON
//! (`--json`), on stdout. Errors go through here too, on stderr. Progress is
//! `log` on stderr and never mixes into stdout.

use std::path::Path;

use miniserde::json::{Number, Object};

use crate::bay::VM_HOME;
use crate::commands::UpReport;
use crate::error::Error;
use crate::files::Copied;
use crate::json::{self, Json};
use crate::lifecycle::{self, Change};
use crate::logger;
use crate::mounts::{Mode, MountKind, MountStatus};
use crate::proxy::{Health, ProxyPort};
use crate::sandbox::{BoxState, PublishedPort};
use crate::status::{
    self, BayDetail, BayStatus, Detail, Overview, ProxyDetail, ProxyStatus,
};

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
    /// `ps`; `wide` adds the image, apps and mounts.
    Overview {
        overview: Overview,
        wide: bool,
    },
    Detail(Box<Detail>),
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
            Self::Overview { overview, .. }
                if !status::events(overview).is_empty() =>
            {
                3
            }
            _ => 0,
        }
    }
}

/// What changed goes to the log file too, so it stays a full record.
/// The log file gets one `name: outcome` line per change.
fn record(result: &Outcome) {
    let lines: Vec<String> = match result {
        Outcome::Changes(report) => {
            report.changes.iter().map(change_line).collect()
        }
        Outcome::Copied(_) => {
            result.human().lines().map(String::from).collect()
        }
        _ => Vec::new(),
    };
    for line in lines {
        log::info!(target: logger::REPORT, "{line}");
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
            Self::Changes(report) => match &report.overview {
                Some(overview) => lines(table(&state_rows(report, overview))),
                None => lines(table(&change_rows(report))),
            },
            Self::Overview { overview, wide } => {
                lines(table(&overview_rows(overview, *wide)))
            }
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
                Json::Array(
                    report
                        .changes
                        .iter()
                        .map(|change| {
                            let addresses = report
                                .addresses
                                .get(&change.name)
                                .map_or(&[][..], Vec::as_slice);
                            change_json(change, addresses)
                        })
                        .collect(),
                ),
            )]),
            Self::Overview { overview, .. } => overview_json(overview),
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
    format!("{}: {}", change.name, result_text(change))
}

/// The changes as a table; ADDRESS, one per line, only when one has any.
fn change_rows(report: &UpReport) -> Vec<Vec<String>> {
    let addressed = report.addresses.values().any(|list| !list.is_empty());
    let mut header =
        vec![one("NAME"), one("TYPE"), one("RESULT"), one("DETAILS")];
    if addressed {
        header.push(one("ADDRESS"));
    }
    let rows = report.changes.iter().map(|change| {
        let (word, reasons) = outcome_parts(&change.outcome);
        let details = match &change.outcome {
            lifecycle::Outcome::Failed(error) => match error.hint() {
                Some(hint) => format!("{} (try: {hint})", error.message()),
                None => error.message().to_string(),
            },
            lifecycle::Outcome::Recreated(_) => reasons.join(", "),
            _ => reasons.join("; "),
        };
        let mut row = vec![
            one(&change.name),
            one(change.resource.as_str()),
            one(word),
            one(&details),
        ];
        if addressed {
            let found = report.addresses.get(&change.name);
            row.push(found.cloned().unwrap_or_else(|| one("")));
        }
        row
    });
    spread(std::iter::once(header).chain(rows).collect())
}

fn change_json(change: &Change, addresses: &[String]) -> Json {
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
        ("addresses", strings(addresses)),
    ])
}

/// A table cell: its lines, `-` for none.
fn cell(lines: impl IntoIterator<Item = String>) -> Vec<String> {
    let lines: Vec<String> = lines.into_iter().collect();
    if lines.is_empty() {
        vec!["-".into()]
    } else {
        lines
    }
}

fn one(text: &str) -> Vec<String> {
    vec![text.to_string()]
}

/// Rows of multi-line cells as printed rows: a row takes as many lines as
/// its tallest cell, the others left blank below their first line.
fn spread(rows: Vec<Vec<Vec<String>>>) -> Vec<Vec<String>> {
    rows.into_iter()
        .flat_map(|row| {
            let height = row.iter().map(Vec::len).max().unwrap_or(1);
            (0..height)
                .map(|line| {
                    row.iter()
                        .map(|cell| cell.get(line).cloned().unwrap_or_default())
                        .collect()
                })
                .collect::<Vec<Vec<String>>>()
        })
        .collect()
}

fn overview_header(wide: bool) -> Vec<Vec<String>> {
    let mut header = vec![
        one("NAME"),
        one("TYPE"),
        one("STATE"),
        one("PROXY"),
        one("PORTS"),
    ];
    if wide {
        header.extend([one("IMAGE"), one("APPS"), one("MOUNTS")]);
    }
    header
}

fn bay_row(bay: &BayStatus, wide: bool) -> Vec<Vec<String>> {
    let proxy = bay.proxy.as_deref().unwrap_or("direct");
    let mut row = vec![
        one(&bay.name),
        one("bay"),
        one(state_word(bay.state)),
        one(proxy),
        cell(bay.ports.iter().map(port_address)),
    ];
    if wide {
        let apps = bay
            .apps
            .iter()
            .map(|(app, state)| format!("{app}:{}", state.as_str()));
        let mounts = bay
            .mounts
            .iter()
            .filter(|mount| mount.kind == MountKind::User)
            .map(|mount| {
                format!(
                    "{}:{}:{}",
                    mount.written,
                    in_home(&mount.vm),
                    mount_mode(mount)
                )
            });
        row.extend([one(&short_image(&bay.image)), cell(apps), cell(mounts)]);
    }
    row
}

fn proxy_row(proxy: &ProxyStatus, wide: bool) -> Vec<Vec<String>> {
    let state = proxy_state(proxy).unwrap_or("external");
    let ports = if proxy.published.is_empty() {
        cell(proxy.port.map(loopback))
    } else {
        cell(proxy.published.iter().map(proxy_address))
    };
    let mut row =
        vec![one(&proxy.name), one("proxy"), one(state), one("-"), ports];
    if wide {
        let image = proxy.image.as_deref().map_or("-".into(), short_image);
        row.extend([one(&image), one("-"), one("-")]);
    }
    row
}

fn overview_rows(overview: &Overview, wide: bool) -> Vec<Vec<String>> {
    let bays = overview.bays.iter().map(|bay| bay_row(bay, wide));
    let proxies = overview.proxies.iter().map(|proxy| proxy_row(proxy, wide));
    let leftovers = overview.leftovers.iter().map(|(resource, name)| {
        let mut row = vec![
            one(name),
            one(resource.as_str()),
            one("leftover"),
            one("-"),
            one("-"),
        ];
        if wide {
            row.extend([one("-"), one("-"), one("-")]);
        }
        row
    });
    spread(
        std::iter::once(overview_header(wide))
            .chain(bays)
            .chain(proxies)
            .chain(leftovers)
            .collect(),
    )
}

/// What a change came to, with its reasons or error.
fn result_text(change: &Change) -> String {
    let (word, reasons) = outcome_parts(&change.outcome);
    match &change.outcome {
        lifecycle::Outcome::Failed(error) => match error.hint() {
            Some(hint) => format!("{word}: {} (try: {hint})", error.message()),
            None => format!("{word}: {}", error.message()),
        },
        _ if reasons.is_empty() => word.to_string(),
        lifecycle::Outcome::Recreated(_) => {
            format!("{word} ({})", reasons.join(", "))
        }
        _ => format!("{word} ({})", reasons.join("; ")),
    }
}

/// `ps`'s rows for what the command touched, each with what it came to:
/// a name changed twice (`restart`) shows its last change.
fn state_rows(report: &UpReport, overview: &Overview) -> Vec<Vec<String>> {
    let mut header = overview_header(false);
    header.push(one("RESULT"));
    let mut names: Vec<&str> = Vec::new();
    for change in &report.changes {
        if !names.contains(&change.name.as_str()) {
            names.push(&change.name);
        }
    }
    let rows = names.into_iter().map(|name| {
        let last = report.changes.iter().rev().find(|c| c.name == name);
        let mut row = if let Some(bay) =
            overview.bays.iter().find(|bay| bay.name == name)
        {
            bay_row(bay, false)
        } else if let Some(proxy) =
            overview.proxies.iter().find(|proxy| proxy.name == name)
        {
            proxy_row(proxy, false)
        } else {
            let kind = last.map_or("-", |change| change.resource.as_str());
            vec![one(name), one(kind), one("-"), one("-"), one("-")]
        };
        row.push(one(&last.map(result_text).unwrap_or_default()));
        row
    });
    spread(std::iter::once(header).chain(rows).collect())
}

fn loopback(port: u16) -> String {
    format!("127.0.0.1:{port}")
}

/// What to open: a URL for an HTTP port, else the address.
pub(crate) fn port_address(port: &PublishedPort) -> String {
    if port.http {
        format!("http://{}", loopback(port.host))
    } else {
        loopback(port.host)
    }
}

/// An image ref with its digest cut to 12 hex digits, as `docker` shows
/// IDs; `inspect` keeps it whole.
fn short_image(image: &str) -> String {
    const SHORT: usize = 12;
    match image.split_once("@sha256:") {
        Some((name, digest)) if digest.len() > SHORT => {
            format!("{name}@sha256:{}", &digest[..SHORT])
        }
        _ => image.into(),
    }
}

pub(crate) fn proxy_address(port: &ProxyPort) -> String {
    if port.http {
        format!("http://{}", loopback(port.host))
    } else {
        loopback(port.host)
    }
}

/// A VM path in pilot's home as `~/…`.
fn in_home(vm: &str) -> String {
    match vm.strip_prefix(VM_HOME) {
        Some("") => "~".into(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => vm.into(),
    }
}

fn overview_json(overview: &Overview) -> Json {
    let bays = overview.bays.iter().map(|bay| {
        json::object([
            ("name", json::string(&bay.name)),
            ("state", json::string(state_word(bay.state))),
            ("proxy", optional(bay.proxy.as_deref())),
            ("image", json::string(&bay.image)),
            ("apps", apps_json(bay)),
            ("ports", ports_json(&bay.ports)),
        ])
    });
    let proxies = overview.proxies.iter().map(|proxy| {
        json::object([
            ("name", json::string(&proxy.name)),
            ("kind", json::string(proxy.kind)),
            ("state", optional(proxy_state(proxy))),
            ("image", optional(proxy.image.as_deref())),
            ("health", optional(health_word(proxy))),
            ("port", proxy.port.map_or(Json::Null, number)),
            ("published", proxy_ports_json(&proxy.published)),
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

fn number(value: impl Into<u64>) -> Json {
    Json::Number(Number::U64(value.into()))
}

fn proxy_ports_json(ports: &[ProxyPort]) -> Json {
    Json::Array(
        ports
            .iter()
            .map(|port| {
                json::object([
                    ("name", json::string(port.name)),
                    ("host", number(port.host)),
                    ("vm", number(port.vm)),
                    ("address", json::string(&proxy_address(port))),
                ])
            })
            .collect(),
    )
}

fn ports_json(ports: &[PublishedPort]) -> Json {
    Json::Array(
        ports
            .iter()
            .map(|port| {
                json::object([
                    ("name", json::string(&port.name)),
                    ("app", json::string(&port.app)),
                    ("host", number(port.host)),
                    ("vm", number(port.vm)),
                    ("address", json::string(&port_address(port))),
                    ("purpose", json::string(&port.purpose)),
                ])
            })
            .collect(),
    )
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

fn mount_mode(mount: &MountStatus) -> &'static str {
    match mount.mode {
        Mode::Ro => "ro",
        Mode::Rw => "rw",
        Mode::Copy => "copy",
    }
}

fn mount_kind(kind: MountKind) -> &'static str {
    match kind {
        MountKind::User => "user",
        MountKind::Home => "home",
        MountKind::Cache => "cache",
    }
}

/// `label:` then the first value, the rest lined up under it.
fn field(label: &str, values: Vec<String>) -> Vec<String> {
    let label = format!("{label}:");
    let mut values = values.into_iter();
    let first = values.next().unwrap_or_else(|| "-".into());
    std::iter::once(format!("  {label:<10}{first}"))
        .chain(values.map(|value| format!("  {:<10}{value}", "")))
        .collect()
}

fn bay_text(bay: &BayDetail) -> Vec<String> {
    let status = &bay.status;
    let network = match (&status.proxy, bay.proxy_port) {
        (Some(proxy), Some(port)) => {
            format!("only through proxy {proxy} ({})", loopback(port))
        }
        (Some(proxy), None) => format!("only through proxy {proxy}"),
        (None, _) => "the open internet (no proxy)".into(),
    };
    let apps = bay.apps.iter().map(|app| {
        match status.apps.iter().find(|(name, _)| name == app) {
            Some((_, state)) => format!("{app} ({})", state.as_str()),
            None => app.clone(),
        }
    });
    let ports = status.ports.iter().map(|port| {
        format!(
            "{}  {}  {} (app {})",
            port.name,
            port_address(port),
            port.purpose,
            port.app
        )
    });
    let mounts = status.mounts.iter().map(|mount| {
        let applied = match mount.applied {
            Some(false) => ", not applied yet",
            _ => "",
        };
        format!(
            "{}:{}:{} ({}{applied})",
            mount.host,
            mount.vm,
            mount_mode(mount),
            mount_kind(mount.kind)
        )
    });
    [
        vec![format!("bay {}", status.name)],
        field("state", vec![state_word(status.state).into()]),
        field("network", vec![network]),
        field("image", vec![status.image.clone()]),
        field(
            "vm",
            vec![format!(
                "{}: {} CPUs, {} memory, {} disk",
                bay.vm, bay.cpus, bay.memory, bay.disk
            )],
        ),
        field("apps", apps.collect()),
        field("ports", ports.collect()),
        field("packages", bay.packages.clone()),
        field(
            "env",
            vec![bay.env.join(", ")]
                .into_iter()
                .filter(|e| !e.is_empty())
                .collect(),
        ),
        field("mounts", mounts.collect()),
        field("home", vec![bay.home.display().to_string()]),
        field("cache", vec![bay.cache.display().to_string()]),
        field("volume", vec![bay.volume.clone()]),
    ]
    .concat()
}

fn proxy_text(proxy: &ProxyDetail) -> Vec<String> {
    let status = &proxy.status;
    let runs = match &proxy.url {
        Some(url) => format!("{}, runs at {url}", status.kind),
        None => format!("{}, run by hangar", status.kind),
    };
    let health = match &status.health {
        None => "not checked: its VM isn't running".into(),
        Some(Health::Up) => "up".into(),
        Some(Health::Down(why)) => format!("unreachable: {why}"),
    };
    let tokens = proxy
        .tokens
        .iter()
        .map(|(key, source)| format!("{key} ({})", source.as_str()));
    let mut text = [
        vec![format!("proxy {}", status.name)],
        field("kind", vec![runs]),
        field(
            "state",
            vec![proxy_state(status).unwrap_or("external").into()],
        ),
        field("health", vec![health]),
        if status.published.is_empty() {
            field("address", status.port.map(loopback).into_iter().collect())
        } else {
            let ports = status
                .published
                .iter()
                .map(|port| format!("{}  {}", port.name, proxy_address(port)));
            field("ports", ports.collect())
        },
        field("bays", proxy.bays.clone()),
        field("tokens", tokens.collect()),
    ]
    .concat();
    if let (Some(vm), Some(image)) = (&proxy.vm, &status.image) {
        text.extend(field("vm", vec![vm.clone()]));
        text.extend(field("image", vec![image.clone()]));
    }
    text.extend(field("data", vec![proxy.data.display().to_string()]));
    if let Some(log) = &proxy.log {
        text.extend(field("log", vec![log.display().to_string()]));
    }
    text
}

fn detail_text(detail: &Detail) -> String {
    match detail {
        Detail::Bay(bay) => bay_text(bay),
        Detail::Proxy(proxy) => proxy_text(proxy),
    }
    .join("\n")
}

fn detail_json(detail: &Detail) -> Json {
    match detail {
        Detail::Bay(bay) => {
            let status = &bay.status;
            let mounts = status.mounts.iter().map(|mount| {
                json::object([
                    ("vm", json::string(&mount.vm)),
                    ("host", json::string(&mount.host)),
                    ("mode", json::string(mount_mode(mount))),
                    ("kind", json::string(mount_kind(mount.kind))),
                    ("applied", mount.applied.map_or(Json::Null, Json::Bool)),
                ])
            });
            document([
                ("name", json::string(&status.name)),
                ("type", json::string("bay")),
                ("state", json::string(state_word(status.state))),
                ("proxy", optional(status.proxy.as_deref())),
                ("proxy_port", bay.proxy_port.map_or(Json::Null, number)),
                ("image", json::string(&status.image)),
                ("vm", json::string(&bay.vm)),
                ("cpus", number(bay.cpus)),
                ("memory", json::string(&bay.memory)),
                ("disk", json::string(&bay.disk)),
                ("home", path(&bay.home)),
                ("cache", path(&bay.cache)),
                ("volume", json::string(&bay.volume)),
                ("apps", strings(&bay.apps)),
                ("app_states", apps_json(status)),
                ("ports", ports_json(&status.ports)),
                ("packages", strings(&bay.packages)),
                ("env", strings(&bay.env)),
                ("mounts", Json::Array(mounts.collect())),
            ])
        }
        Detail::Proxy(proxy) => {
            let status = &proxy.status;
            let tokens = proxy.tokens.iter().map(|(key, source)| {
                json::object([
                    ("name", json::string(key)),
                    ("source", json::string(source.as_str())),
                ])
            });
            let why = match &status.health {
                Some(Health::Down(why)) => json::string(why),
                _ => Json::Null,
            };
            document([
                ("name", json::string(&status.name)),
                ("type", json::string("proxy")),
                ("kind", json::string(status.kind)),
                ("state", optional(proxy_state(status))),
                ("health", optional(health_word(status))),
                ("health_error", why),
                ("port", status.port.map_or(Json::Null, number)),
                ("published", proxy_ports_json(&status.published)),
                ("url", optional(proxy.url.as_deref())),
                ("image", optional(status.image.as_deref())),
                ("vm", optional(proxy.vm.as_deref())),
                ("data", path(&proxy.data)),
                ("log", proxy.log.as_deref().map_or(Json::Null, path)),
                ("bays", strings(&proxy.bays)),
                ("tokens", Json::Array(tokens.collect())),
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

#[cfg(test)]
mod tests {
    use super::{Outcome, Render, error_json};
    use crate::commands::UpReport;
    use crate::error::Error;
    use crate::lifecycle::{self, Change, Resource};
    use crate::mounts::{Mode, MountKind, MountStatus};
    use crate::proxy::{Health, ProxyPort};
    use crate::sandbox::{BoxState, PublishedPort};
    use crate::status::{
        BayDetail, BayStatus, Detail, Overview, ProxyDetail, ProxyStatus,
        RunState, TokenSource,
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
        let addresses = [(
            "p".to_string(),
            vec!["127.0.0.1:3100".to_string(), "http://127.0.0.1:3101".into()],
        )]
        .into();
        Outcome::Changes(UpReport {
            changes,
            addresses,
            overview: None,
        })
    }

    #[test]
    fn up_summary_lines() {
        let outcome = changes();
        assert_eq!(
            outcome.human(),
            "NAME  TYPE   RESULT     DETAILS            ADDRESS\n\
             p     proxy  created                       127.0.0.1:3100\n\
             \x20                                          http://127.0.0.1:3101\n\
             a     bay    recreated  image, ports\n\
             b     bay    updated    env; copies +x\n\
             c     bay    unchanged\n\
             d     bay    stopped\n\
             e     bay    removed\n\
             f     bay    failed     boom (try: retry)\n\
             g     bay    failed     bust"
        );
        assert_eq!(outcome.exit_code(), 1);
        let ok = Outcome::Changes(UpReport::default());
        assert_eq!(ok.exit_code(), 0);
        assert_eq!(Outcome::Done.exit_code(), 0);
    }

    #[test]
    fn up_json_shape() {
        let json = to_string(&changes().json());
        assert!(json.starts_with(
            r#"{"changes":[{"addresses":["127.0.0.1:3100","http://127.0.0.1:3101"],"error":null,"name":"p","outcome":"created","reasons":[],"type":"proxy"},{"addresses":[],"error":null,"name":"a","outcome":"recreated","reasons":["image","ports"],"type":"bay"}"#
        ), "{json}");
        assert!(json.contains(
            r#"{"addresses":[],"error":{"error":"boom","hint":"retry"},"name":"f","outcome":"failed","reasons":[],"type":"bay"}"#
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
            port: Some(3100),
            published: state
                .map(|_| vec![ProxyPort::proxy(3100, 3100)])
                .unwrap_or_default(),
        }
    }

    fn work_bay() -> BayStatus {
        BayStatus {
            name: "work".into(),
            state: Some(BoxState::Running),
            proxy: Some("p".into()),
            image: "img:1".into(),
            apps: vec![("web".into(), RunState::Exited)],
            ports: vec![
                PublishedPort {
                    app: "web".into(),
                    name: "ui".into(),
                    host: 3000,
                    vm: 3000,
                    purpose: "Web UI".into(),
                    http: true,
                },
                PublishedPort {
                    app: "web".into(),
                    name: "db".into(),
                    host: 5433,
                    vm: 5432,
                    purpose: "Database".into(),
                    http: false,
                },
            ],
            mounts: vec![
                MountStatus {
                    vm: "/home/pilot".into(),
                    host: "/d/bays/work/home".into(),
                    written: "/d/bays/work/home".into(),
                    mode: Mode::Rw,
                    kind: MountKind::Home,
                    applied: Some(true),
                },
                MountStatus {
                    vm: "/home/pilot/notes".into(),
                    host: "/u/notes".into(),
                    written: "~/notes".into(),
                    mode: Mode::Ro,
                    kind: MountKind::User,
                    applied: Some(false),
                },
                MountStatus {
                    vm: "/home/pilot/.gitconfig".into(),
                    host: "/u/.gitconfig".into(),
                    written: "~/.gitconfig".into(),
                    mode: Mode::Copy,
                    kind: MountKind::User,
                    applied: None,
                },
            ],
        }
    }

    fn overview(wide: bool) -> Outcome {
        Outcome::Overview {
            overview: Overview {
                bays: vec![work_bay()],
                proxies: vec![
                    proxy_status(Some(BoxState::Stopped)),
                    ProxyStatus {
                        name: "ext".into(),
                        ..proxy_status(None)
                    },
                ],
                leftovers: vec![(Resource::Proxy, "gone".into())],
            },
            wide,
        }
    }

    #[test]
    fn overview_lists_bays_proxies_and_leftovers_with_their_ports() {
        assert_eq!(
            overview(false).human(),
            "NAME  TYPE   STATE     PROXY  PORTS\n\
             work  bay    running   p      http://127.0.0.1:3000\n\
             \x20                             127.0.0.1:5433\n\
             p     proxy  stopped   -      127.0.0.1:3100\n\
             ext   proxy  external  -      127.0.0.1:3100\n\
             gone  proxy  leftover  -      -"
        );
        assert_eq!(
            to_string(&overview(false).json()),
            r#"{"bays":[{"apps":[{"name":"web","state":"exited"}],"image":"img:1","name":"work","ports":[{"address":"http://127.0.0.1:3000","app":"web","host":3000,"name":"ui","purpose":"Web UI","vm":3000},{"address":"127.0.0.1:5433","app":"web","host":5433,"name":"db","purpose":"Database","vm":5432}],"proxy":"p","state":"running"}],"leftovers":[{"name":"gone","type":"proxy"}],"proxies":[{"health":null,"image":"fake:1","kind":"fake","name":"p","port":3100,"published":[{"address":"127.0.0.1:3100","host":3100,"name":"proxy","vm":3100}],"state":"stopped"},{"health":null,"image":null,"kind":"fake","name":"ext","port":3100,"published":[],"state":null}],"version":1}"#
        );
    }

    #[test]
    fn a_wide_overview_adds_image_apps_and_the_users_mounts() {
        let wide = overview(true).human();
        let rows: Vec<&str> = wide.lines().collect();
        assert!(rows[0].ends_with("IMAGE   APPS        MOUNTS"), "{wide}");
        assert!(
            rows[1].ends_with("img:1   web:exited  ~/notes:~/notes:ro"),
            "{wide}"
        );
        assert!(rows[2].trim_start().starts_with("127.0.0.1:5433"), "{wide}");
        assert!(
            rows[2].ends_with("~/.gitconfig:~/.gitconfig:copy"),
            "{wide}"
        );
        assert_eq!(rows[2].find('~'), rows[0].find("MOUNTS"), "{wide}");
        assert!(rows[3].ends_with("fake:1  -           -"), "{wide}");
        assert_eq!(
            to_string(&overview(true).json()),
            to_string(&overview(false).json())
        );
    }

    #[test]
    fn ps_cuts_an_image_digest_to_twelve_digits() {
        let digest = "9624fb475ae8a6d45ef815c7a16331c7";
        assert_eq!(
            super::short_image(&format!("example/proxy:1@sha256:{digest}")),
            "example/proxy:1@sha256:9624fb475ae8"
        );
        assert_eq!(super::short_image("img:1"), "img:1");
        assert_eq!(super::short_image("img@sha256:abc"), "img@sha256:abc");
    }

    #[test]
    fn a_bay_detail_says_how_it_reaches_the_network_and_what_it_publishes() {
        let bay = |proxy_port| BayDetail {
            status: work_bay(),
            vm: "hangar-bay-work".into(),
            cpus: 2,
            memory: "4G".into(),
            disk: "20G".into(),
            proxy_port,
            packages: vec!["nixpkgs#jq".into()],
            apps: vec!["web".into(), "tool".into()],
            env: vec!["A".into(), "B".into()],
            home: PathBuf::from("/d/bays/work/home"),
            cache: PathBuf::from("/c/bays/work"),
            volume: "hangar-bay-work-data".into(),
        };
        let detail = |bay| Outcome::Detail(Box::new(Detail::Bay(bay)));
        assert_eq!(
            detail(bay(Some(3100))).human(),
            "bay work\n  \
             state:    running\n  \
             network:  only through proxy p (127.0.0.1:3100)\n  \
             image:    img:1\n  \
             vm:       hangar-bay-work: 2 CPUs, 4G memory, 20G disk\n  \
             apps:     web (exited)\n            \
             tool\n  \
             ports:    ui  http://127.0.0.1:3000  Web UI (app web)\n            \
             db  127.0.0.1:5433  Database (app web)\n  \
             packages: nixpkgs#jq\n  \
             env:      A, B\n  \
             mounts:   /d/bays/work/home:/home/pilot:rw (home)\n            \
             /u/notes:/home/pilot/notes:ro (user, not applied yet)\n            \
             /u/.gitconfig:/home/pilot/.gitconfig:copy (user)\n  \
             home:     /d/bays/work/home\n  \
             cache:    /c/bays/work\n  \
             volume:   hangar-bay-work-data"
        );
        let json = to_string(&detail(bay(Some(3100))).json());
        for part in [
            r#""proxy_port":3100"#,
            r#""cpus":2,"#,
            r#""env":["A","B"]"#,
            r#""mode":"copy""#,
            r#""app_states":[{"name":"web","state":"exited"}]"#,
            r#""address":"http://127.0.0.1:3000""#,
        ] {
            assert!(json.contains(part), "{part}: {json}");
        }
        let mut direct = work_bay();
        direct.proxy = None;
        let shown = detail(BayDetail {
            status: direct,
            env: Vec::new(),
            ..bay(None)
        })
        .human();
        assert!(
            shown.contains("network:  the open internet (no proxy)\n"),
            "{shown}"
        );
        assert!(shown.contains("env:      -\n"), "{shown}");
    }

    #[test]
    fn a_proxy_detail_says_where_it_runs_its_health_bays_and_tokens() {
        let detail = |state, vm: Option<&str>, health| {
            Outcome::Detail(Box::new(Detail::Proxy(ProxyDetail {
                status: ProxyStatus {
                    health,
                    ..proxy_status(state)
                },
                vm: vm.map(String::from),
                url: vm.is_none().then(|| "http://127.0.0.1:3100".into()),
                data: PathBuf::from("/d/proxies/p/data"),
                log: vm.map(|_| PathBuf::from("/d/p.log")),
                bays: vec!["work".into()],
                tokens: vec![
                    ("proxy-token".into(), TokenSource::Keychain),
                    ("session-token".into(), TokenSource::Missing),
                ],
            })))
        };
        let managed = detail(
            Some(BoxState::Running),
            Some("hangar-proxy-p"),
            Some(Health::Up),
        );
        assert_eq!(
            managed.human(),
            "proxy p\n  \
             kind:     fake, run by hangar\n  \
             state:    running\n  \
             health:   up\n  \
             ports:    proxy  127.0.0.1:3100\n  \
             bays:     work\n  \
             tokens:   proxy-token (keychain)\n            \
             session-token (missing)\n  \
             vm:       hangar-proxy-p\n  \
             image:    fake:1\n  \
             data:     /d/proxies/p/data\n  \
             log:      /d/p.log"
        );
        let json = to_string(&managed.json());
        assert!(json.contains(r#""log":"/d/p.log""#), "{json}");
        assert!(
            json.contains(r#"{"name":"session-token","source":"missing"}"#),
            "{json}"
        );
        let external = detail(None, None, Some(Health::Down("refused".into())));
        let shown = external.human();
        assert!(
            shown.contains("kind:     fake, runs at http://127.0.0.1:3100\n"),
            "{shown}"
        );
        assert!(
            shown.contains("health:   unreachable: refused\n"),
            "{shown}"
        );
        let json = to_string(&external.json());
        assert!(json.contains(r#""log":null"#), "{json}");
        assert!(json.contains(r#""health_error":"refused""#), "{json}");
        let stopped = detail(Some(BoxState::Stopped), Some("vm"), None).human();
        assert!(
            stopped.contains("health:   not checked: its VM isn't running\n"),
            "{stopped}"
        );
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

    #[test]
    fn overview_json_names_each_proxy_health() {
        let health = |health| ProxyStatus {
            health,
            ..proxy_status(None)
        };
        let json = to_string(
            &Outcome::Overview {
                overview: Overview {
                    bays: Vec::new(),
                    proxies: vec![
                        health(Some(Health::Up)),
                        health(Some(Health::Down("refused".into()))),
                        health(None),
                    ],
                    leftovers: Vec::new(),
                },
                wide: false,
            }
            .json(),
        );
        assert!(json.contains(r#""health":"up""#), "{json}");
        assert!(json.contains(r#""health":"unreachable""#), "{json}");
        assert!(json.contains(r#""health":null"#), "{json}");
    }

    #[test]
    fn an_overview_of_nothing_prints_no_table() {
        let empty = Outcome::Overview {
            overview: Overview {
                bays: Vec::new(),
                proxies: Vec::new(),
                leftovers: Vec::new(),
            },
            wide: false,
        };
        assert_eq!(empty.human(), "");
    }
}
