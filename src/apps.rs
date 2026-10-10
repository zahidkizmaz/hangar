//! Apps as data: what an app needs in a bay (packages, routes, env, a run
//! entry, ports), from `config/apps/*.json` or `appDefinitions`, enabled
//! per bay by its `apps`. No code here knows an app by name.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::broker::Route;
use crate::config::{
    BaySettings, Fields, checked_vars, route_json, routes, valid_name,
};
use crate::error::{Context, Error, Result, bail};
use crate::json::{self, Json};
use crate::sandbox::PublishedPort;
use miniserde::json::Object;

/// The built-in definitions; `appDefinitions` replaces one whole. The
/// route-only ones hold the hosts tools and `packages` need.
const BUILTINS: [(&str, &str); 10] = [
    (
        "claude-code",
        include_str!("../config/apps/claude-code.json"),
    ),
    ("codex", include_str!("../config/apps/codex.json")),
    ("docker", include_str!("../config/apps/docker.json")),
    ("github", include_str!("../config/apps/github.json")),
    (
        "github-token",
        include_str!("../config/apps/github-token.json"),
    ),
    ("nix", include_str!("../config/apps/nix.json")),
    ("node", include_str!("../config/apps/node.json")),
    ("paperclip", include_str!("../config/apps/paperclip.json")),
    ("python", include_str!("../config/apps/python.json")),
    ("rust", include_str!("../config/apps/rust.json")),
];

#[derive(Clone)]
pub(crate) struct AppDef {
    name: String,
    packages: Vec<String>,
    routes: BTreeMap<String, Route>,
    env: BTreeMap<String, String>,
    /// Fixed, non-secret credential values its routes send.
    credentials: BTreeMap<String, String>,
    run: Option<String>,
    setup: Option<Setup>,
    ports: Vec<PublishedPort>,
}

/// An app's one-time interactive step, and the check that says it's done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Setup {
    pub(crate) command: String,
    pub(crate) check: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct App {
    pub(crate) name: String,
    pub(crate) routes: Vec<String>,
    pub(crate) setup: Option<Setup>,
}

/// Where a resolved route came from, in merge order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Origin {
    /// The app's position among every bay's apps, and its name.
    App(usize, String),
    Config,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::App(_, name) => write!(f, "app {name} route"),
            Self::Config => f.write_str("config route"),
        }
    }
}

/// `appDefinitions`, every entry checked, enabled or not, so a typo fails
/// loudly; the built-ins are read when a bay names them.
pub(crate) struct Catalog {
    defined: BTreeMap<String, AppDef>,
}

impl Catalog {
    pub(crate) fn new(definitions: Option<Json>) -> Result<Self> {
        let definitions = match definitions {
            None | Some(Json::Null) => Object::new(),
            Some(Json::Object(definitions)) => definitions,
            Some(_) => bail!("config.appDefinitions: expected an object"),
        };
        let mut defined = BTreeMap::new();
        for (name, value) in &definitions {
            if !valid_name(name) {
                bail!(
                    "config.appDefinitions.{name}: names are lowercase, \
                     digits and -"
                );
            }
            defined.insert(name.clone(), parse(name, value)?);
        }
        Ok(Self { defined })
    }

    pub(crate) fn enabled(
        &self,
        apps: Option<&Json>,
        path: &str,
    ) -> Result<Vec<AppDef>> {
        app_names(apps, path)?
            .iter()
            .map(|name| match self.defined.get(name) {
                Some(definition) => Ok(definition.clone()),
                None => self.builtin(name),
            })
            .collect()
    }

    fn builtin(&self, name: &str) -> Result<AppDef> {
        let Some((_, text)) = BUILTINS.iter().find(|(known, _)| *known == name)
        else {
            let known: BTreeSet<&str> = BUILTINS
                .iter()
                .map(|(known, _)| *known)
                .chain(self.defined.keys().map(String::as_str))
                .collect();
            let known = known.into_iter().collect::<Vec<_>>().join(", ");
            bail!("unknown app {name}; known: {known}");
        };
        parse(name, &json::parse(text)?)
    }
}

fn app_names(apps: Option<&Json>, path: &str) -> Result<Vec<String>> {
    let items = match apps {
        None | Some(Json::Null) => return Ok(Vec::new()),
        Some(Json::Array(items)) => items,
        Some(_) => bail!("{path}: expected strings"),
    };
    let mut seen = BTreeSet::new();
    items
        .iter()
        .map(|item| {
            let Json::String(name) = item else {
                bail!("{path}: expected strings");
            };
            if !seen.insert(name) {
                bail!("{path}: {name} is listed twice");
            }
            Ok(name.clone())
        })
        .collect()
}

fn parse(name: &str, value: &Json) -> Result<AppDef> {
    let definition = || -> Result<AppDef> {
        let fields = Fields::new(value, "")?;
        fields.only(&[
            "packages",
            "routes",
            "env",
            "credentials",
            "setup",
            "run",
            "ports",
        ])?;
        Ok(AppDef {
            name: name.to_string(),
            packages: fields.strings("packages")?,
            routes: app_routes(&fields)?,
            env: checked_vars(&fields, "env")?,
            credentials: checked_vars(&fields, "credentials")?,
            run: fields.optional_string("run")?,
            setup: setup(&fields)?,
            ports: ports(name, &fields)?,
        })
    };
    definition().context(format!("app {name}"))
}

/// `extra` reaches the broker unchecked, so it is trusted user config only.
fn app_routes(fields: &Fields) -> Result<BTreeMap<String, Route>> {
    let routes = routes(fields.map.get("routes"), "routes")?;
    if let Some(name) = routes
        .iter()
        .find(|(_, route)| !route.extra.is_empty())
        .map(|(name, _)| name)
    {
        bail!("routes.{name}.extra: only config.tower.routes may set it");
    }
    Ok(routes)
}
fn setup(fields: &Fields) -> Result<Option<Setup>> {
    if !fields.map.contains_key("setup") {
        return Ok(None);
    }
    let setup = fields.object("setup")?;
    setup.only(&["command", "check"])?;
    Ok(Some(Setup {
        command: setup.string("command")?,
        check: setup.string("check")?,
    }))
}

fn ports(app: &str, fields: &Fields) -> Result<Vec<PublishedPort>> {
    let items = match fields.map.get("ports") {
        None | Some(Json::Null) => return Ok(Vec::new()),
        Some(Json::Array(items)) => items,
        Some(_) => bail!("ports: expected a list"),
    };
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let port = Fields::new(item, &format!("ports.{index}"))?;
            port.only(&["name", "vm", "host", "purpose", "http"])?;
            let name = port.string("name")?;
            if !valid_name(&name) {
                bail!("{}: names are lowercase, digits and -", port.at("name"));
            }
            let vm = port.number("vm")?;
            let host = match port.map.get("host") {
                None | Some(Json::Null) => vm,
                Some(_) => port.number("host")?,
            };
            Ok(PublishedPort {
                app: Some(app.to_string()),
                bay: None,
                name,
                vm,
                host,
                purpose: port.string("purpose")?,
                http: port.flag_or("http", true)?,
            })
        })
        .collect()
}

/// Every bay's apps' routes (each app once, in bay order), then the
/// config's own. Two apps may share a route only if it's the same, and a
/// config route may not take an app route's name: neither shadows the
/// other. Returns where each route came from.
pub(crate) fn merge_routes(
    bays: &[Vec<AppDef>],
    user: BTreeMap<String, Route>,
) -> Result<(BTreeMap<String, Route>, BTreeMap<String, Origin>)> {
    let mut routes: BTreeMap<String, Route> = BTreeMap::new();
    let mut origins: BTreeMap<String, Origin> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let apps = bays.iter().flatten().filter(|app| seen.insert(&app.name));
    for (index, app) in apps.enumerate() {
        for (name, route) in &app.routes {
            if let Some(Origin::App(_, other)) = origins.get(name)
                && routes.get(name).map(|r| json::stringify(&route_json(r)))
                    != Some(json::stringify(&route_json(route)))
            {
                return Err(Error::with_hint(
                    format!(
                        "app {}: route {name} differs from app {other}'s",
                        app.name
                    ),
                    format!(
                        "enable only one of the two apps ({other}, {})",
                        app.name
                    ),
                ));
            }
            routes.insert(name.clone(), route.clone());
            origins
                .entry(name.clone())
                .or_insert_with(|| Origin::App(index, app.name.clone()));
        }
    }
    for (name, route) in user {
        if let Some(origin) = origins.get(&name) {
            return Err(Error::with_hint(
                format!(
                    "config.tower.routes.{name}: {origin} {name} has that name"
                ),
                "give your route another name; an app's routes come with \
                 the app, so leave the app out to route its host yourself",
            ));
        }
        routes.insert(name.clone(), route);
        origins.insert(name, Origin::Config);
    }
    Ok((routes, origins))
}

pub(crate) fn check_routes(
    routes: &BTreeMap<String, Route>,
    origins: &BTreeMap<String, Origin>,
) -> Result<()> {
    check_hosts(routes, origins)?;
    check_credentials(routes, origins)
}

/// Every enabled app's fixed credentials: key -> (app, value).
pub(crate) fn fixed_credentials(
    bays: &[Vec<AppDef>],
) -> Result<BTreeMap<String, (String, String)>> {
    agreed(bays.iter().flatten(), "credentials", |app| &app.credentials)
}

/// Each key's value and the app that set it, from `values` of every app;
/// two apps may share a key only with the same value.
fn agreed<'a>(
    apps: impl IntoIterator<Item = &'a AppDef>,
    field: &str,
    values: impl Fn(&AppDef) -> &BTreeMap<String, String>,
) -> Result<BTreeMap<String, (String, String)>> {
    let mut agreed: BTreeMap<String, (String, String)> = BTreeMap::new();
    for app in apps {
        for (key, value) in values(app) {
            if let Some((other, other_value)) = agreed.get(key)
                && other_value != value
            {
                bail!(
                    "app {}: {field}.{key} differs from app {other}'s",
                    app.name
                );
            }
            agreed.insert(key.clone(), (app.name.clone(), value.clone()));
        }
    }
    Ok(agreed)
}

/// No app may set a credential's placeholder (`env` still may): the
/// placeholders are the tower's, so this is checked once, over every bay.
pub(crate) fn check_placeholders(
    bays: &[Vec<AppDef>],
    placeholders: &BTreeSet<String>,
) -> Result<()> {
    for app in bays.iter().flatten() {
        if let Some(key) =
            app.env.keys().find(|key| placeholders.contains(*key))
        {
            bail!(
                "app {}: env.{key}: reserved for its credential placeholder",
                app.name
            );
        }
    }
    Ok(())
}

pub(crate) fn apply(bay: &mut BaySettings, apps: &[AppDef]) -> Result<()> {
    let env = app_env(apps)?;
    let mut seen = BTreeSet::new();
    bay.packages = apps
        .iter()
        .flat_map(|app| &app.packages)
        .chain(&bay.packages)
        .filter(|package| seen.insert(*package))
        .cloned()
        .collect();
    bay.env = env
        .into_iter()
        .chain(std::mem::take(&mut bay.env))
        .collect();
    bay.run = apps
        .iter()
        .filter_map(|app| Some((app.name.clone(), app.run.clone()?)))
        .chain(std::mem::take(&mut bay.run))
        .collect();
    bay.ports = bay_ports(&bay.name, apps, &bay.host_ports)?;
    bay.apps = apps
        .iter()
        .map(|app| App {
            name: app.name.clone(),
            routes: app.routes.keys().cloned().collect(),
            setup: app.setup.clone(),
        })
        .collect();
    Ok(())
}

fn bay_ports(
    bay: &str,
    apps: &[AppDef],
    host_ports: &BTreeMap<String, u16>,
) -> Result<Vec<PublishedPort>> {
    let mut ports: Vec<PublishedPort> = Vec::new();
    for app in apps {
        for port in &app.ports {
            if let Some(other) = ports.iter().find(|o| o.name == port.name) {
                bail!(
                    "bay {bay}: apps {} and {} both publish a port named {}",
                    other.app.as_deref().unwrap_or_default(),
                    app.name,
                    port.name
                );
            }
            ports.push(PublishedPort {
                bay: Some(bay.to_string()),
                ..port.clone()
            });
        }
    }
    for (name, host) in host_ports {
        let Some(port) = ports.iter_mut().find(|port| port.name == *name)
        else {
            bail!(
                "config.bays.{bay}.ports.{name}: no app of this bay publishes it"
            );
        };
        port.host = *host;
    }
    Ok(ports)
}

/// Two of a bay's apps may not disagree on a variable; apps in different
/// bays may.
fn app_env(apps: &[AppDef]) -> Result<BTreeMap<String, String>> {
    let env = agreed(apps, "env", |app| &app.env)?;
    Ok(env
        .into_iter()
        .map(|(key, (_, value))| (key, value))
        .collect())
}

/// Routes from different sources may not share a host: the vault would
/// pick one of them.
fn check_hosts(
    routes: &BTreeMap<String, Route>,
    origins: &BTreeMap<String, Origin>,
) -> Result<()> {
    let mut by_host: BTreeMap<&str, Vec<(&Origin, &str)>> = BTreeMap::new();
    for (name, route) in routes {
        by_host
            .entry(route.host.as_str())
            .or_default()
            .push((origin(origins, name), name));
    }
    for (host, mut users) in by_host {
        users.sort();
        let (first, first_name) = users[0];
        if let Some((origin, name)) =
            users.iter().find(|(origin, _)| *origin != first)
        {
            bail!("host {host}: {first} {first_name} and {origin} {name}");
        }
    }
    Ok(())
}

/// An app's route may not use a credential another source uses, so an
/// app can't send someone else's credential to its own host.
fn check_credentials(
    routes: &BTreeMap<String, Route>,
    origins: &BTreeMap<String, Origin>,
) -> Result<()> {
    for (name, route) in routes {
        let owner = origin(origins, name);
        if !matches!(owner, Origin::App(..)) {
            continue;
        }
        let credentials = route.auth.credentials();
        for (other_name, other) in routes {
            let other_origin = origin(origins, other_name);
            if other_origin == owner {
                continue;
            }
            let shared = other.auth.credentials();
            if let Some(credential) = credentials.intersection(&shared).next() {
                bail!(
                    "credential {credential}: {owner} {name} and \
                     {other_origin} {other_name}"
                );
            }
        }
    }
    Ok(())
}

fn origin<'a>(origins: &'a BTreeMap<String, Origin>, name: &str) -> &'a Origin {
    origins.get(name).unwrap_or(&Origin::Config)
}

#[cfg(test)]
mod tests {
    use super::BUILTINS;
    use crate::config::{BaySettings, Settings};
    use crate::testing::{
        config_error as error, one_bay, resolved as resolve_text,
    };

    fn bay(settings: &Settings) -> &BaySettings {
        &settings.bays[0]
    }

    /// Package hosts for configs whose apps bring packages.
    const NIX: &str = r#""nix", "github""#;

    #[test]
    fn every_builtin_is_a_valid_app() {
        for (name, _) in BUILTINS {
            // Apps with packages need the nix and GitHub routes too.
            let apps = match name {
                "nix" | "github" => r#""nix", "github""#.to_string(),
                "github-token" => r#""nix", "github-token""#.to_string(),
                _ => format!(r#"{NIX}, "{name}""#),
            };
            let config = one_bay(&format!(r#""apps": [{apps}]"#), "");
            let settings = resolve_text(&config).unwrap();
            assert!(bay(&settings).apps.iter().any(|app| app.name == name));
        }
    }

    #[test]
    fn a_route_only_app_is_just_data() {
        let only = one_bay(
            r#""apps": ["mirror"]"#,
            r#""appDefinitions": {"mirror": {"routes": [{"name": "m",
                "host": "m.example", "auth": {"type": "passthrough"}}]}}"#,
        );
        let settings = resolve_text(&only).unwrap();
        assert_eq!(settings.routes["m"].host, "m.example");
        assert_eq!(bay(&settings).apps[0].routes, ["m"]);
        assert_eq!(bay(&settings).packages, Vec::<String>::new());
    }

    #[test]
    fn apps_must_be_known_once_and_well_named() {
        let known = "claude-code, codex, docker, github, github-token, nix, \
                     node, paperclip, python, rust";
        assert_eq!(
            error(&one_bay(r#""apps": ["hermes"]"#, "")),
            format!("unknown app hermes; known: {known}")
        );
        assert_eq!(
            error(&one_bay(
                r#""apps": ["hermes"]"#,
                r#""appDefinitions": {"web": {}}"#
            )),
            format!("unknown app hermes; known: {known}, web")
        );
        assert_eq!(
            error(&one_bay(r#""apps": ["node", "node"]"#, "")),
            "config.bays.default.apps: node is listed twice"
        );
        assert_eq!(
            error(&one_bay(r#""apps": "web""#, "")),
            "config.bays.default.apps: expected strings"
        );
        assert_eq!(
            error(r#"{"appDefinitions": []}"#),
            "config.appDefinitions: expected an object"
        );
        assert_eq!(
            error(r#"{"appDefinitions": {"Web": {}}}"#),
            "config.appDefinitions.Web: names are lowercase, digits and -"
        );
        // Checked even when not enabled.
        assert_eq!(
            error(r#"{"appDefinitions": {"web": {"files": {}}}}"#),
            "app web: files: unknown setting"
        );
        assert_eq!(
            error(r#"{"appDefinitions": {"web": []}}"#),
            "app web: expected an object"
        );
    }

    #[test]
    fn a_definition_replaces_a_builtin_whole() {
        let settings = resolve_text(&one_bay(
            r#""apps": ["paperclip"]"#,
            r#""appDefinitions": {"paperclip": {
                  "ports": [{"name": "ui", "vm": 3100, "host": 3200,
                             "purpose": "UI", "http": false}]}}"#,
        ))
        .unwrap();
        let bay = bay(&settings);
        assert_eq!(bay.packages, Vec::<String>::new());
        assert!(bay.env.is_empty());
        assert!(bay.run.is_empty());
        let port = &bay.ports[0];
        assert_eq!((port.host, port.vm, port.http), (3200, 3100, false));
    }

    #[test]
    fn definitions_are_checked_key_by_key() {
        let app = |definition: &str| {
            error(&format!(r#"{{"appDefinitions": {{"web": {definition}}}}}"#))
        };
        assert_eq!(
            app(r#"{"setup": {"command": "x"}}"#),
            "app web: setup.check: expected a string"
        );
        assert_eq!(
            app(r#"{"setup": {"command": "x", "check": "y", "tty": true}}"#),
            "app web: setup.tty: unknown setting"
        );
        assert_eq!(app(r#"{"ports": {}}"#), "app web: ports: expected a list");
        assert_eq!(
            app(r#"{"ports": [{"name": "Web", "vm": 1, "purpose": "x"}]}"#),
            "app web: ports.0.name: names are lowercase, digits and -"
        );
        assert_eq!(
            app(r#"{"ports": [{"name": "w", "vm": 70000, "purpose": "x"}]}"#),
            "app web: ports.0.vm: expected a whole number"
        );
        assert_eq!(
            app(r#"{"ports": [{"name": "w", "vm": 1, "purpose": "x",
                               "host": "1"}]}"#),
            "app web: ports.0.host: expected a whole number"
        );
        assert_eq!(
            app(
                r#"{"routes": [{"name": "x", "host": "h", "auth": {"type": "?"}}]}"#
            ),
            r#"app web: routes.x.auth.type: unknown auth type "?""#
        );
        assert_eq!(
            app(r#"{"routes": {"x": {}}}"#),
            "app web: routes: expected a list"
        );
        assert_eq!(app(r#"{"run": 1}"#), "app web: run: expected a string");
    }

    #[test]
    fn fixed_credentials_are_plain_values_two_apps_agree_on() {
        let app = |credentials: &str| {
            error(&format!(
                r#"{{"appDefinitions": {{"web": {{"credentials": {credentials}}}}}}}"#
            ))
        };
        assert!(
            app(r#"{"T": "ghp_abcdefghijklmnopqrstuvwxyz0123456789"}"#)
                .contains("credentials.T: looks like a real secret")
        );
        assert_eq!(
            app(r#"{"user": "x"}"#),
            "app web: credentials.user: expected UPPER_SNAKE_CASE"
        );
        let two = |b: &str| {
            format!(
                r#"{{"bays": [{{"name": "default", "apps": ["a", "b"]}}],
                    "appDefinitions": {{
                      "a": {{"credentials": {{"USER": "one"}}}},
                      "b": {{"credentials": {{"USER": "{b}"}}}}}}}}"#
            )
        };
        assert_eq!(
            error(&two("two")),
            "app b: credentials.USER differs from app a's"
        );
        let same = resolve_text(&two("one")).unwrap();
        assert_eq!(same.app_credentials["USER"], ("b".into(), "one".into()));
    }

    #[test]
    fn reserved_names_are_refused_in_app_env_and_bay_env() {
        let app = |env: &str| {
            error(&format!(
                r#"{{"appDefinitions": {{"web": {{"env": {env}}}}}}}"#
            ))
        };
        assert_eq!(
            app(r#"{"PATH": "/x"}"#),
            "app web: env.PATH: reserved for hangar"
        );
        assert_eq!(
            app(r#"{"https_proxy": "x"}"#),
            "app web: env.https_proxy: reserved for hangar"
        );
        assert_eq!(
            error(&one_bay(r#""env": {"PATH": "/x"}"#, "")),
            "config.bays.default.env.PATH: reserved for hangar"
        );
        assert_eq!(
            error(&one_bay(r#""env": {"HANGAR_X": "x"}"#, "")),
            "config.bays.default.env.HANGAR_X: reserved for hangar"
        );
        // An app can't override a credential placeholder, in any bay; a
        // bay's env can.
        let placeholder = r#""appDefinitions": {"web": {"env": {
                      "CLAUDE_CODE_OAUTH_TOKEN": "x"}}}"#;
        let config = format!(
            r#"{{"bays": [{{"name": "a", "apps": [{NIX}, "claude-code"]}},
                          {{"name": "b", "apps": ["web"]}}], {placeholder}}}"#
        );
        assert_eq!(
            error(&config),
            "app web: env.CLAUDE_CODE_OAUTH_TOKEN: reserved for its \
             credential placeholder"
        );
        let settings = resolve_text(&one_bay(
            &format!(
                r#""apps": [{NIX}, "claude-code"],
                   "env": {{"CLAUDE_CODE_OAUTH_TOKEN": "x"}}"#
            ),
            "",
        ))
        .unwrap();
        assert_eq!(bay(&settings).env["CLAUDE_CODE_OAUTH_TOKEN"], "x");
    }

    const TWO_APPS: &str = r#""appDefinitions": {
        "a": {"packages": ["path:/p1", "path:/p2"],
              "env": {"MODE": "one", "A": "a"},
              "run": "a serve",
              "routes": [{"name": "shared", "host": "s.example",
                          "auth": {"type": "passthrough"}},
                         {"name": "a-api", "host": "a.example",
                          "auth": {"type": "bearer", "token": "A_TOKEN"}}]},
        "b": {"packages": ["path:/p2", "path:/p3"], "env": {"MODE": "one"},
              "routes": [{"name": "shared", "host": "s.example",
                          "auth": {"type": "passthrough"}}]},
        "nixy": {"routes": [{"name": "nix-cache", "host": "cache.nixos.org",
                             "auth": {"type": "passthrough"}}]}}"#;

    #[test]
    fn packages_env_run_and_routes_merge_key_by_key() {
        let settings = resolve_text(&one_bay(
            r#""apps": ["nixy", "a", "b"], "packages": ["path:/p3", "path:/p4"],
               "env": {"A": "mine"}, "run": {"a": "my serve", "x": "x"}"#,
            TWO_APPS,
        ))
        .unwrap();
        let bay = bay(&settings);
        assert_eq!(
            bay.packages,
            ["path:/p1", "path:/p2", "path:/p3", "path:/p4"]
        );
        assert_eq!(bay.env["MODE"], "one");
        assert_eq!(bay.env["A"], "mine");
        assert_eq!(bay.run["a"], "my serve");
        assert_eq!(bay.run["x"], "x");
        assert!(settings.routes.contains_key("a-api"));
        assert_eq!(bay.apps[1].routes, ["a-api", "shared"]);
        assert_eq!(bay.apps[2].routes, ["shared"]);
    }

    #[test]
    fn apps_in_one_bay_may_not_disagree_but_bays_may() {
        let differing = TWO_APPS
            .replace(r#""env": {"MODE": "one"}"#, r#""env": {"MODE": "two"}"#);
        assert_eq!(
            error(&one_bay(r#""apps": ["nixy", "a", "b"]"#, &differing)),
            "app b: env.MODE differs from app a's"
        );
        let apart = format!(
            r#"{{"bays": [{{"name": "x", "apps": ["nixy", "a"]}},
                          {{"name": "y", "apps": ["b"]}}], {differing}}}"#
        );
        let settings = resolve_text(&apart).unwrap();
        assert_eq!(settings.bays[0].env["MODE"], "one");
        assert_eq!(settings.bays[1].env["MODE"], "two");

        let differing = TWO_APPS.replacen(
            r#""host": "s.example""#,
            r#""host": "t.example""#,
            1,
        );
        let error =
            resolve_text(&one_bay(r#""apps": ["nixy", "a", "b"]"#, &differing))
                .unwrap_err();
        assert_eq!(error.message(), "app b: route shared differs from app a's");
        assert_eq!(
            error.hint(),
            Some("enable only one of the two apps (a, b)")
        );
        let both =
            resolve_text(&one_bay(r#""apps": ["github", "github-token"]"#, ""))
                .unwrap_err();
        assert_eq!(
            both.message(),
            "app github-token: route github-api differs from app github's"
        );
    }

    #[test]
    fn a_host_and_a_route_name_belong_to_one_source() {
        let app_and_config = one_bay(
            r#""apps": ["nixy", "a"]"#,
            &format!(
                r#"{TWO_APPS}, "tower": {{"routes": [{{"name": "mine",
                     "host": "a.example", "auth": {{"type": "passthrough"}}}}]}}"#
            ),
        );
        assert_eq!(
            error(&app_and_config),
            "host a.example: app a route a-api and config route mine"
        );
        let same_name = one_bay(
            r#""apps": ["node"]"#,
            r#""tower": {"routes": [{"name": "npm", "host": "npm.example",
                 "auth": {"type": "passthrough"}}]}"#,
        );
        let error = resolve_text(&same_name).unwrap_err();
        assert_eq!(
            error.message(),
            "config.tower.routes.npm: app node route npm has that name"
        );
        assert!(error.hint().unwrap().contains("another name"));
        // Without the app, the name is the user's.
        let mine = one_bay(
            "",
            r#""tower": {"routes": [{"name": "npm", "host": "npm.example",
                 "auth": {"type": "basic", "username": "NPM_USER"}}]}"#,
        );
        assert_eq!(
            resolve_text(&mine).unwrap().routes["npm"].host,
            "npm.example"
        );
    }

    #[test]
    fn only_config_routes_may_set_extra() {
        let app = r#"{"appDefinitions": {"w": {"routes": [{"name": "w-api",
            "host": "w.example", "auth": {"type": "passthrough"},
            "extra": {"substitutions": []}}]}}}"#;
        assert_eq!(
            error(app),
            "app w: routes.w-api.extra: only config.tower.routes may set it"
        );
        let config = r#"{"tower": {"routes": [{"name": "w-api",
            "host": "w.example", "auth": {"type": "passthrough"},
            "extra": {"substitutions": []}}]}}"#;
        assert!(resolve_text(config).is_ok());
    }

    #[test]
    fn an_app_cannot_use_a_credential_another_source_uses() {
        let token_route = r#""tower": {"routes": [{"name": "ghe",
            "host": "ghe.example",
            "auth": {"type": "bearer", "token": "GITHUB_TOKEN"}}]}"#;
        // (a) Next to github-token, the user's route can't send its token
        // elsewhere; without it, it can.
        assert_eq!(
            error(&one_bay(r#""apps": ["github-token"]"#, token_route)),
            "credential GITHUB_TOKEN: app github-token route github-api and \
             config route ghe"
        );
        assert!(resolve_text(&one_bay("", token_route)).is_ok());
        // (b) Nor can another app's route.
        let other_app = r#""appDefinitions": {"w": {"routes": [{"name": "w-api",
              "host": "w.example",
              "auth": {"type": "bearer", "token": "GITHUB_TOKEN"}}]}}"#;
        assert_eq!(
            error(&one_bay(r#""apps": ["github-token", "w"]"#, other_app)),
            "credential GITHUB_TOKEN: app github-token route github-api and \
             app w route w-api"
        );
        let user = one_bay(
            &format!(r#""apps": [{NIX}, "claude-code"]"#),
            r#""tower": {"routes": [{"name": "proxy-ai", "host": "ai.example",
              "auth": {"type": "bearer",
                       "token": "CLAUDE_CODE_OAUTH_TOKEN"}}]}"#,
        );
        assert_eq!(
            error(&user),
            "credential CLAUDE_CODE_OAUTH_TOKEN: app claude-code route \
             anthropic and config route proxy-ai"
        );
    }

    #[test]
    fn ports_are_unique_per_bay_and_can_move() {
        let web = r#""appDefinitions": {
            "web": {"ports": [{"name": "ui", "vm": 3000, "purpose": "UI"}]},
            "admin": {"ports": [{"name": "ui", "vm": 4000, "purpose": "UI"}]}}"#;
        assert_eq!(
            error(&one_bay(r#""apps": ["web", "admin"]"#, web)),
            "bay default: apps web and admin both publish a port named ui"
        );
        let moved = resolve_text(&one_bay(
            r#""apps": ["web"], "ports": {"ui": 3001}"#,
            web,
        ))
        .unwrap();
        assert_eq!(
            (bay(&moved).ports[0].host, bay(&moved).ports[0].vm),
            (3001, 3000)
        );
        assert_eq!(
            error(&one_bay(r#""apps": ["web"], "ports": {"api": 1}"#, web)),
            "config.bays.default.ports.api: no app of this bay publishes it"
        );
    }
}
