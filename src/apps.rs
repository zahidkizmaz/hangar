//! Apps as data: what an app needs in a bay (packages, env, a run entry,
//! ports), from `config/apps/*.json` or `apps`, enabled
//! per bay by its `apps`. No code here knows an app by name.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::{BaySettings, Fields, checked_vars, valid_app_name};
use crate::error::{Context, Result, bail};
use crate::json::{self, Json};
use crate::sandbox::PublishedPort;
use miniserde::json::Object;

const BUILTINS: [(&str, &str); 3] = [
    (
        "claude-code",
        include_str!("../config/apps/claude-code.json"),
    ),
    ("codex", include_str!("../config/apps/codex.json")),
    ("paperclip", include_str!("../config/apps/paperclip.json")),
];

#[derive(Clone)]
pub(crate) struct AppDef {
    name: String,
    packages: Vec<String>,
    env: BTreeMap<String, String>,
    run: Option<String>,
    setup: Option<Setup>,
    ports: Vec<PublishedPort>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Setup {
    pub(crate) command: String,
    pub(crate) check: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct App {
    pub(crate) name: String,
    pub(crate) setup: Option<Setup>,
}

pub(crate) struct Catalog {
    defined: BTreeMap<String, AppDef>,
}

impl Catalog {
    pub(crate) fn new(definitions: Option<Json>) -> Result<Self> {
        let definitions = match definitions {
            None | Some(Json::Null) => Object::new(),
            Some(Json::Object(definitions)) => definitions,
            Some(_) => bail!("apps: expected an object"),
        };
        let mut defined = BTreeMap::new();
        for (name, value) in &definitions {
            if !valid_app_name(name) {
                bail!(
                    "apps.{name}: names are lowercase, \
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
        fields.only(&["packages", "env", "setup", "run", "ports"])?;
        Ok(AppDef {
            name: name.to_string(),
            packages: fields.strings("packages")?,
            env: checked_vars(&fields, "env")?,
            run: fields.optional_string("run")?,
            setup: setup(&fields)?,
            ports: ports(name, &fields)?,
        })
    };
    definition().context(format!("app {name}"))
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
            if !valid_app_name(&name) {
                bail!("{}: names are lowercase, digits and -", port.at("name"));
            }
            let vm = port.number("vm")?;
            let host = match port.map.get("host") {
                None | Some(Json::Null) => vm,
                Some(_) => port.number("host")?,
            };
            Ok(PublishedPort {
                app: app.to_string(),
                name,
                vm,
                host,
                purpose: port.string("purpose")?,
                http: port.flag_or("http", true)?,
            })
        })
        .collect()
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
                    other.app,
                    app.name,
                    port.name
                );
            }
            ports.push(port.clone());
        }
    }
    for (name, host) in host_ports {
        let Some(port) = ports.iter_mut().find(|port| port.name == *name)
        else {
            bail!("bays.{bay}.ports.{name}: no app of this bay publishes it");
        };
        port.host = *host;
    }
    Ok(ports)
}

fn app_env(apps: &[AppDef]) -> Result<BTreeMap<String, String>> {
    let mut env: BTreeMap<String, (&str, &str)> = BTreeMap::new();
    for app in apps {
        for (key, value) in &app.env {
            if let Some((other, other_value)) = env.get(key)
                && other_value != value
            {
                bail!("app {}: env.{key} differs from app {other}'s", app.name);
            }
            env.insert(key.clone(), (&app.name, value));
        }
    }
    Ok(env
        .into_iter()
        .map(|(key, (_, value))| (key, value.to_string()))
        .collect())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::BUILTINS;
    use crate::config::{BaySettings, Settings};
    use crate::testing::{
        config_error as error, one_bay, resolved as resolve_text,
    };

    fn bay(settings: &Settings) -> &BaySettings {
        &settings.bays[0]
    }

    #[test]
    fn every_builtin_is_a_valid_app() {
        for (name, _) in BUILTINS {
            let config = one_bay(&format!(r#""apps": ["{name}"]"#), "");
            let settings = resolve_text(&config).unwrap();
            assert!(bay(&settings).apps.iter().any(|app| app.name == name));
        }
    }

    fn builtin_env(name: &str) -> BTreeMap<String, String> {
        let config = one_bay(&format!(r#""apps": ["{name}"]"#), "");
        bay(&resolve_text(&config).unwrap()).env.clone()
    }

    #[test]
    fn builtin_apps_are_claude_code_codex_paperclip() {
        let names: Vec<&str> = BUILTINS.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["claude-code", "codex", "paperclip"]);
    }

    #[test]
    fn claude_code_env_is_a_placeholder() {
        let env = builtin_env("claude-code");
        assert_eq!(env["CLAUDE_CODE_OAUTH_TOKEN"], "hangar-placeholder");
        assert_eq!(
            builtin_env("codex")["OPENAI_API_KEY"],
            "hangar-placeholder"
        );
    }

    #[test]
    fn routes_in_an_app_are_unknown() {
        assert_eq!(
            error(r#"{"apps": {"x": {"routes": []}}}"#),
            "app x: routes: unknown setting"
        );
        assert_eq!(
            error(r#"{"apps": {"x": {"credentials": {}}}}"#),
            "app x: credentials: unknown setting"
        );
    }

    #[test]
    fn real_looking_env_value_refused() {
        let key = format!("sk-ant-api03-{}", "a1B2".repeat(10));
        assert_eq!(
            error(&format!(
                r#"{{"apps": {{"x": {{"env": {{"K": "{key}"}}}}}}}}"#
            )),
            "app x: env.K: looks like a real secret"
        );
    }

    #[test]
    fn apps_must_be_known_once_and_well_named() {
        let known = "claude-code, codex, paperclip";
        assert_eq!(
            error(&one_bay(r#""apps": ["hermes"]"#, "")),
            format!("unknown app hermes; known: {known}")
        );
        assert_eq!(
            error(&one_bay(r#""apps": ["hermes"]"#, r#""apps": {"web": {}}"#)),
            format!("unknown app hermes; known: {known}, web")
        );
        assert_eq!(
            error(&one_bay(r#""apps": ["codex", "codex"]"#, "")),
            "bays.default.apps: codex is listed twice"
        );
        assert_eq!(
            error(&one_bay(r#""apps": "web""#, "")),
            "bays.default.apps: expected strings"
        );
        assert_eq!(error(r#"{"apps": []}"#), "apps: expected an object");
        assert_eq!(
            error(r#"{"apps": {"Web": {}}}"#),
            "apps.Web: names are lowercase, digits and -"
        );
        assert_eq!(
            error(r#"{"apps": {"web": {"files": {}}}}"#),
            "app web: files: unknown setting"
        );
        assert_eq!(
            error(r#"{"apps": {"web": []}}"#),
            "app web: expected an object"
        );
    }

    #[test]
    fn a_definition_replaces_a_builtin_whole() {
        let settings = resolve_text(&one_bay(
            r#""apps": ["paperclip"]"#,
            r#""apps": {"paperclip": {
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
            error(&format!(r#"{{"apps": {{"web": {definition}}}}}"#))
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
        assert_eq!(app(r#"{"run": 1}"#), "app web: run: expected a string");
    }

    #[test]
    fn reserved_names_are_refused_in_app_env_and_bay_env() {
        let app = |env: &str| {
            error(&format!(r#"{{"apps": {{"web": {{"env": {env}}}}}}}"#))
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
            "bays.default.env.PATH: reserved for hangar"
        );
        assert_eq!(
            error(&one_bay(r#""env": {"HANGAR_X": "x"}"#, "")),
            "bays.default.env.HANGAR_X: reserved for hangar"
        );
    }

    const TWO_APPS: &str = r#""apps": {
        "a": {"packages": ["path:/p1", "path:/p2"],
              "env": {"MODE": "one", "A": "a"},
              "run": "a serve"},
        "b": {"packages": ["path:/p2", "path:/p3"], "env": {"MODE": "one"}}}"#;

    #[test]
    fn packages_and_env_merge_key_by_key() {
        let settings = resolve_text(&one_bay(
            r#""apps": ["a", "b"], "packages": ["path:/p3", "path:/p4"],
               "env": {"A": "mine"}"#,
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
        assert_eq!(bay.run["a"], "a serve");
        assert_eq!(bay.run.len(), 1);
    }

    #[test]
    fn apps_in_one_bay_may_not_disagree_but_bays_may() {
        let differing = TWO_APPS
            .replace(r#""env": {"MODE": "one"}"#, r#""env": {"MODE": "two"}"#);
        assert_eq!(
            error(&one_bay(r#""apps": ["a", "b"]"#, &differing)),
            "app b: env.MODE differs from app a's"
        );
        let apart = format!(
            r#"{{"bays": [{{"name": "x", "proxy": null, "apps": ["a"]}},
                          {{"name": "y", "proxy": null, "apps": ["b"]}}],
                {differing}}}"#
        );
        let settings = resolve_text(&apart).unwrap();
        assert_eq!(settings.bays[0].env["MODE"], "one");
        assert_eq!(settings.bays[1].env["MODE"], "two");
    }

    #[test]
    fn ports_are_unique_per_bay_and_can_move() {
        let web = r#""apps": {
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
            "bays.default.ports.api: no app of this bay publishes it"
        );
    }
}
