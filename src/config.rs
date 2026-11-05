use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::apps::{self, App};
use crate::dirs::{self, Base};
use crate::error::{Context, Result, bail};
use crate::files;
use crate::mounts::{self, MountSpec};
use crate::sandbox::PublishedPort;
use crate::secret::{SECRET_PREFIXES, find_secret};
use miniserde::json::Object;

use crate::json::{self, Json};

/// The only copy of the defaults. The Nix module only overrides them, and
/// `hangar init` writes just the keys a user has to set.
const DEFAULTS: &str = include_str!("../config/defaults.json");

pub(crate) const INITIAL_CONFIG: &str = r#"{
  "bays": [{ "name": "default", "apps": [], "packages": [] }]
}
"#;

const TOP_LEVEL: [&str; 4] = ["bays", "appDefinitions", "sandbox", "stateDir"];
const BAY_KEYS: [&str; 16] = [
    "name",
    "apps",
    "ports",
    "image",
    "imageRepository",
    "imageLoader",
    "cpus",
    "memory",
    "disk",
    "packages",
    "env",
    "run",
    "files",
    "mounts",
    "home",
    "cache",
];

/// Reads a non-empty environment variable; tests pass their own lookup.
pub(crate) type Var<'a> = &'a dyn Fn(&str) -> Option<String>;

pub(crate) fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[derive(Debug)]
pub(crate) struct Settings {
    pub(crate) sandbox_backend: String,
    pub(crate) bays: Vec<BaySettings>,
    pub(crate) state_dir: Option<String>,
}

#[derive(Debug)]
pub(crate) struct BaySettings {
    pub(crate) name: String,
    pub(crate) apps: Vec<App>,
    pub(crate) ports: Vec<PublishedPort>,
    /// The config's `ports`: app port name -> host port.
    pub(crate) host_ports: BTreeMap<String, u16>,
    pub(crate) image: Option<String>,
    pub(crate) image_repository: String,
    pub(crate) image_loader: Option<String>,
    pub(crate) cpus: u32,
    pub(crate) memory: String,
    pub(crate) disk: String,
    pub(crate) packages: Vec<String>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) run: BTreeMap<String, String>,
    /// Config files copied into the VM on every `up`: VM path (`~`
    /// expanded) -> host path.
    pub(crate) files: BTreeMap<String, String>,
    pub(crate) mounts: BTreeMap<String, MountSpec>,
    pub(crate) home: bool,
    pub(crate) cache: bool,
}

impl BaySettings {
    /// `image` when set; otherwise the image released with this hangar, so
    /// the CLI and the image always match.
    pub(crate) fn image_ref(&self) -> String {
        self.image.clone().unwrap_or_else(|| {
            format!("{}:v{}", self.image_repository, env!("CARGO_PKG_VERSION"))
        })
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct Paths {
    pub(crate) user_config: PathBuf,
    pub(crate) config: PathBuf,
}

impl Paths {
    pub(crate) fn locate(var: Var) -> Result<Self> {
        let user_config = match var("HANGAR_CONFIG") {
            Some(path) => PathBuf::from(path),
            None => dirs::hangar_dir(Base::Config, var)?.join("hangar.json"),
        };
        let config = var("HANGAR_CONFIG")
            .or_else(|| var("HANGAR_NIX_CONFIG"))
            .map_or_else(|| user_config.clone(), PathBuf::from);
        Ok(Self {
            user_config,
            config,
        })
    }
}

pub(crate) fn defaults() -> Result<Json> {
    json::parse(DEFAULTS).context("built-in defaults")
}

/// The config over the defaults: each bay over the defaults' `bay`.
pub(crate) fn resolve(defaults: Json, config: Json) -> Result<Settings> {
    let Json::Object(mut config) = config else {
        bail!("config must be a JSON object");
    };
    Fields::new(&Json::Object(config.clone()), "config")?.only(&TOP_LEVEL)?;
    let Json::Object(mut merged) = defaults else {
        bail!("built-in defaults must be a JSON object");
    };
    let bay_defaults = merged.remove("bay").context("defaults: no bay")?;
    let catalog = apps::Catalog::new(config.remove("appDefinitions"))?;
    let bays = config.remove("bays");
    let mut merged = Json::Object(merged);
    merge(&mut merged, Json::Object(config));
    let top = Fields::new(&merged, "config")?;

    let bays = bay_entries(bays)?
        .into_iter()
        .map(|entry| {
            let mut bay = bay_defaults.clone();
            merge(&mut bay, entry);
            bay
        })
        .collect::<Vec<_>>();
    let enabled = bays
        .iter()
        .map(|bay| {
            let fields = Fields::new(bay, &bay_path(bay))?;
            catalog.enabled(fields.map.get("apps"), &fields.at("apps"))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut settings = Settings {
        sandbox_backend: top.object("sandbox")?.string("backend")?,
        bays: Vec::new(),
        state_dir: top.optional_string("stateDir")?,
    };
    top.object("sandbox")?.only(&["backend"])?;
    for (bay, apps) in bays.iter().zip(&enabled) {
        let mut bay = BaySettings::from_json(bay)?;
        apps::apply(&mut bay, apps)?;
        settings.bays.push(bay);
    }
    Ok(settings)
}

/// `bays` as given; none (absent, `null` or `[]`) is one bay, `default`.
fn bay_entries(bays: Option<Json>) -> Result<Vec<Json>> {
    let items = match bays {
        None | Some(Json::Null) => Vec::new(),
        Some(Json::Array(items)) => items.into_iter().collect(),
        Some(_) => bail!("config.bays: expected a list"),
    };
    if items.is_empty() {
        return Ok(vec![json::object([("name", json::string("default"))])]);
    }
    let mut names = BTreeSet::new();
    for (index, item) in items.iter().enumerate() {
        let entry = Fields::new(item, &format!("config.bays.{index}"))?;
        let name = entry.string("name")?;
        if !valid_bay_name(&name) {
            bail!(
                "config.bays.{index}.name: {name:?}: lowercase letters and \
                 digits, joined by single -, at most 32 characters"
            );
        }
        if !names.insert(name.clone()) {
            bail!("config.bays: two bays named {name}");
        }
    }
    Ok(items)
}

fn bay_path(bay: &Json) -> String {
    match bay {
        Json::Object(bay) if let Some(Json::String(name)) = bay.get("name") => {
            format!("config.bays.{name}")
        }
        _ => "config.bays".to_string(),
    }
}

pub(crate) fn state_dir(settings: &Settings, var: Var) -> Result<PathBuf> {
    if let Some(dir) = var("HANGAR_STATE_DIR") {
        return Ok(dir.into());
    }
    if let Some(dir) = settings.state_dir.as_ref().filter(|d| !d.is_empty()) {
        return Ok(dir.into());
    }
    dirs::hangar_dir(Base::Data, var)
}

impl BaySettings {
    fn from_json(value: &Json) -> Result<Self> {
        let bay = Fields::new(value, &bay_path(value))?;
        bay.only(&BAY_KEYS)?;
        let settings = Self {
            name: bay.string("name")?,
            apps: Vec::new(),
            ports: Vec::new(),
            host_ports: host_ports(&bay)?,
            image: bay.optional_string("image")?,
            image_repository: bay.string("imageRepository")?,
            image_loader: bay.optional_string("imageLoader")?,
            cpus: bay.number("cpus")?,
            memory: bay.string("memory")?,
            disk: bay.string("disk")?,
            packages: bay.strings("packages")?,
            env: checked_vars(&bay, "env")?,
            run: bay_run(&bay)?,
            files: bay_files(&bay)?,
            mounts: bay_mounts(&bay)?,
            home: bay.boolean("home")?,
            cache: bay.boolean("cache")?,
        };
        mounts::check_targets(
            &settings.mounts,
            settings.home,
            settings.files.keys(),
        )?;
        Ok(settings)
    }
}

/// `^[A-Z][A-Z0-9_]*$`.
pub(crate) fn valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars.next().is_some_and(|first| first.is_ascii_uppercase())
        && chars
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Real-looking secrets don't belong in `env`: the VM sees them. A
/// placeholder is one short value, so this is stricter than the scan of
/// `files`: any credential prefix counts, and so does a long token.
fn looks_like_a_secret(value: &str) -> bool {
    SECRET_PREFIXES
        .iter()
        .chain(&["xox", "-----BEGIN"])
        .any(|prefix| value.starts_with(prefix))
        || find_secret(value).is_some()
        || (value.len() >= 40 && !value.contains(' '))
}

/// Variables hangar sets itself in the VM: its PATH and home, the proxy
/// and CA wiring, nix's own settings and `HANGAR_*` (the run marker).
const RESERVED_ENV: [&str; 17] = [
    "PATH",
    "HOME",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "http_proxy",
    "https_proxy",
    "NO_PROXY",
    "no_proxy",
    "SSL_CERT_FILE",
    "NIX_SSL_CERT_FILE",
    "CURL_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
    "GIT_SSL_CAINFO",
    "NIX_CONFIG",
    "NIX_USER_CONF_FILES",
    "NIX_REMOTE",
    "NIX_PATH",
];

fn reserved(key: &str) -> bool {
    RESERVED_ENV.contains(&key) || key.starts_with("HANGAR_")
}

/// Variables under `key`: none of hangar's own, none real-looking.
pub(crate) fn checked_vars(
    fields: &Fields,
    key: &str,
) -> Result<BTreeMap<String, String>> {
    let vars = fields.string_map(key)?;
    let path = fields.at(key);
    for (key, value) in &vars {
        if reserved(key) {
            bail!("{path}.{key}: reserved for hangar");
        }
        if !valid_key(key) {
            bail!("{path}.{key}: expected UPPER_SNAKE_CASE");
        }
        if looks_like_a_secret(value) {
            bail!("{path}.{key}: looks like a real secret");
        }
    }
    Ok(vars)
}

/// A bay name: `[a-z0-9]+(-[a-z0-9]+)*`, at most 32 characters. It ends up
/// in VM and file names and the vault agent `hangar-<name>`, whose rule
/// (3–64 characters, no leading, trailing or doubled `-`) it always meets.
pub(crate) fn valid_bay_name(name: &str) -> bool {
    name.len() <= 32
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn host_ports(bay: &Fields) -> Result<BTreeMap<String, u16>> {
    if !bay.map.contains_key("ports") {
        return Ok(BTreeMap::new());
    }
    let ports = bay.object("ports")?;
    ports
        .map
        .keys()
        .map(|name| Ok((name.clone(), ports.number(name)?)))
        .collect()
}

fn bay_run(bay: &Fields) -> Result<BTreeMap<String, String>> {
    let run = bay.string_map("run")?;
    if let Some(name) = run.keys().find(|name| !valid_name(name)) {
        bail!(
            "{}.{name}: names are lowercase, digits and -",
            bay.at("run")
        );
    }
    Ok(run)
}

fn bay_files(bay: &Fields) -> Result<BTreeMap<String, String>> {
    bay.string_map("files")?
        .into_iter()
        .map(|(vm, host)| {
            let expanded = files::vm_path(&vm)
                .context(format!("{}.{vm}", bay.at("files")))?;
            Ok((expanded, host))
        })
        .collect()
}

fn bay_mounts(bay: &Fields) -> Result<BTreeMap<String, MountSpec>> {
    if !bay.map.contains_key("mounts") {
        return Ok(BTreeMap::new());
    }
    let entries = bay.object("mounts")?;
    entries
        .map
        .keys()
        .map(|vm| {
            let entry = entries.object(vm)?;
            entry.only(&["host", "writable"])?;
            let expanded = files::vm_path(vm).context(entries.at(vm))?;
            let spec = MountSpec {
                host: entry.string("host")?,
                writable: entry.flag_or("writable", false)?,
            };
            Ok((expanded, spec))
        })
        .collect()
}

/// One JSON object being read into a struct; errors name the key's path
/// (relative when `path` is empty: an app's file).
pub(crate) struct Fields<'a> {
    pub(crate) map: &'a Object,
    path: String,
}

impl<'a> Fields<'a> {
    pub(crate) fn new(value: &'a Json, path: &str) -> Result<Self> {
        let Json::Object(map) = value else {
            if path.is_empty() {
                bail!("expected an object");
            }
            bail!("{path}: expected an object");
        };
        Ok(Self {
            map,
            path: path.to_string(),
        })
    }

    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn at(&self, key: &str) -> String {
        if self.path.is_empty() {
            key.to_string()
        } else {
            format!("{}.{key}", self.path)
        }
    }

    pub(crate) fn object(&self, key: &str) -> Result<Fields<'a>> {
        let path = self.at(key);
        let value = self.map.get(key).context(format!("{path}: missing"))?;
        Fields::new(value, &path)
    }

    pub(crate) fn string(&self, key: &str) -> Result<String> {
        self.optional_string(key)?
            .context(format!("{}: expected a string", self.at(key)))
    }

    /// Strict: `null` is refused, never read as absent.
    fn boolean(&self, key: &str) -> Result<bool> {
        match self.map.get(key) {
            Some(Json::Bool(value)) => Ok(*value),
            _ => bail!("{}: expected true or false", self.at(key)),
        }
    }

    pub(crate) fn optional_string(&self, key: &str) -> Result<Option<String>> {
        match self.map.get(key) {
            None | Some(Json::Null) => Ok(None),
            Some(Json::String(text)) => Ok(Some(text.clone())),
            Some(_) => bail!("{}: expected a string", self.at(key)),
        }
    }

    pub(crate) fn number<T: std::str::FromStr>(&self, key: &str) -> Result<T> {
        match self.map.get(key) {
            Some(Json::Number(number)) => number.to_string().parse().ok(),
            _ => None,
        }
        .context(format!("{}: expected a whole number", self.at(key)))
    }

    pub(crate) fn flag_or(&self, key: &str, absent: bool) -> Result<bool> {
        match self.map.get(key) {
            None | Some(Json::Null) => Ok(absent),
            Some(Json::Bool(value)) => Ok(*value),
            Some(_) => bail!("{}: expected true or false", self.at(key)),
        }
    }

    pub(crate) fn strings(&self, key: &str) -> Result<Vec<String>> {
        let not_strings = || format!("{}: expected strings", self.at(key));
        match self.map.get(key) {
            None | Some(Json::Null) => Ok(Vec::new()),
            Some(Json::Array(items)) => items
                .iter()
                .map(|item| match item {
                    Json::String(text) => Ok(text.clone()),
                    _ => bail!("{}", not_strings()),
                })
                .collect(),
            Some(_) => bail!("{}", not_strings()),
        }
    }

    fn string_map(&self, key: &str) -> Result<BTreeMap<String, String>> {
        if !self.map.contains_key(key) {
            return Ok(BTreeMap::new());
        }
        let fields = self.object(key)?;
        fields
            .map
            .keys()
            .map(|name| Ok((name.clone(), fields.string(name)?)))
            .collect()
    }

    pub(crate) fn only(&self, known: &[&str]) -> Result<()> {
        let mut keys = self.map.keys();
        if let Some(key) = keys.find(|key| !known.contains(&key.as_str())) {
            bail!("{}: unknown setting", self.at(key));
        }
        Ok(())
    }
}

/// Recursive object merge; anything else in `overlay` replaces `base`.
fn merge(base: &mut Json, overlay: Json) {
    match (base, overlay) {
        (Json::Object(base), Json::Object(overlay)) => {
            for (key, value) in overlay {
                merge(base.entry(key).or_insert(Json::Null), value);
            }
        }
        (base, overlay) => *base = overlay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testing::{
        config_error as error, one_bay, settings as resolve_with, vars,
    };

    fn names(settings: &Settings) -> Vec<&str> {
        settings.bays.iter().map(|bay| bay.name.as_str()).collect()
    }

    #[test]
    fn keys_follow_the_env_key_rule() {
        for key in ["A", "GITHUB_TOKEN", "K8S_", "X_1"] {
            assert!(valid_key(key), "{key}");
        }
        for key in ["", "_LEADING", "1ST", "lower", "MIXED_case", "A-B"] {
            assert!(!valid_key(key), "{key}");
        }
    }

    #[test]
    fn no_bays_is_one_bay_named_default() {
        for config in ["{}", r#"{"bays": null}"#, r#"{"bays": []}"#] {
            let settings = resolve_with(config);
            assert_eq!(names(&settings), ["default"]);
            assert_eq!(settings.bays[0].cpus, 4);
        }
    }

    #[test]
    fn bays_keep_their_order_and_each_gets_the_defaults() {
        let settings = resolve_with(
            r#"{"bays": [{"name": "work", "cpus": 8}, {"name": "oss"},
                         {"name": "a-1"}]}"#,
        );
        assert_eq!(names(&settings), ["work", "oss", "a-1"]);
        assert_eq!(settings.bays[0].cpus, 8);
        assert_eq!(settings.bays[0].memory, "6G");
        assert_eq!(settings.bays[1].cpus, 4);
    }

    #[test]
    fn bay_names_are_required_unique_and_well_formed() {
        assert_eq!(
            error(r#"{"bays": [{"cpus": 2}]}"#),
            "config.bays.0.name: expected a string"
        );
        assert_eq!(
            error(r#"{"bays": [{"name": "a"}, {"name": "a"}]}"#),
            "config.bays: two bays named a"
        );
        assert_eq!(error(r#"{"bays": {}}"#), "config.bays: expected a list");
        for good in ["a", "a-b-1", &"a".repeat(32)] {
            assert!(valid_bay_name(good), "{good}");
        }
        for bad in ["", "A", "x-", "-x", "a--b", "a_b", &"a".repeat(33)] {
            assert!(!valid_bay_name(bad), "{bad}");
            let config = format!(r#"{{"bays": [{{"name": "{bad}"}}]}}"#);
            assert!(error(&config).contains("lowercase letters and digits"));
        }
    }

    #[test]
    fn misplaced_keys_are_refused() {
        for key in ["bay", "credentialFiles", "tower"] {
            let config = format!(r#"{{"{key}": {{}}}}"#);
            assert_eq!(
                error(&config),
                format!("config.{key}: unknown setting")
            );
        }
    }

    #[test]
    fn bad_bay_values_name_the_bay() {
        assert_eq!(
            error(&one_bay(r#""packages": ["a", 1]"#, "")),
            "config.bays.default.packages: expected strings"
        );
        assert_eq!(
            error(&one_bay(r#""cpus": 8.5"#, "")),
            "config.bays.default.cpus: expected a whole number"
        );
        assert_eq!(
            error(&one_bay(r#""cpu": 8"#, "")),
            "config.bays.default.cpu: unknown setting"
        );
    }

    #[test]
    fn config_lookup_order() {
        let paths = Paths::locate(&vars(&[
            ("HOME", "/h"),
            ("HANGAR_NIX_CONFIG", "/nix/hangar.json"),
        ]))
        .unwrap();
        assert_eq!(paths.config, PathBuf::from("/nix/hangar.json"));
        assert_eq!(
            paths.user_config,
            PathBuf::from("/h/.config/hangar/hangar.json")
        );

        let paths = Paths::locate(&vars(&[
            ("HOME", "/h"),
            ("HANGAR_CONFIG", "/c.json"),
            ("HANGAR_NIX_CONFIG", "/nix/hangar.json"),
        ]))
        .unwrap();
        assert_eq!(paths.config, PathBuf::from("/c.json"));
        assert_eq!(paths.user_config, PathBuf::from("/c.json"));

        let paths =
            Paths::locate(&vars(&[("XDG_CONFIG_HOME", "/xdg")])).unwrap();
        assert_eq!(paths.config, PathBuf::from("/xdg/hangar/hangar.json"));
    }

    #[test]
    fn home_and_cache_are_on_or_off_never_a_path() {
        let bay = |config: &str| resolve_with(config).bays.remove(0);
        let unset = bay("{}");
        assert_eq!((unset.home, unset.cache), (true, true));
        let off = bay(&one_bay(r#""home": false, "cache": false"#, ""));
        assert_eq!((off.home, off.cache), (false, false));
        for value in ["null", r#""~/h""#, "1"] {
            let message = error(&one_bay(&format!(r#""cache": {value}"#), ""));
            assert_eq!(
                message,
                "config.bays.default.cache: expected true or false"
            );
        }
        let mount = one_bay(
            r#""mounts": {"~/data": {"host": "~/d", "writable": null}}"#,
            "",
        );
        assert!(!bay(&mount).mounts["/home/pilot/data"].writable);
    }

    #[test]
    fn state_dir_order() {
        let settings = resolve_with(r#"{"stateDir": "/from/config"}"#);
        let env = vars(&[("HOME", "/h"), ("HANGAR_STATE_DIR", "/from/env")]);
        assert_eq!(
            state_dir(&settings, &env).unwrap(),
            PathBuf::from("/from/env")
        );
        let env = vars(&[("HOME", "/h")]);
        assert_eq!(
            state_dir(&settings, &env).unwrap(),
            PathBuf::from("/from/config")
        );
        let settings = resolve_with("{}");
        assert_eq!(
            state_dir(&settings, &env).unwrap(),
            PathBuf::from("/h/.local/share/hangar")
        );
    }

    #[test]
    fn module_fixture_is_a_valid_config() {
        let settings = resolve_with(include_str!("../tests/module.json"));
        let bay = &settings.bays[0];
        assert_eq!(
            bay.packages,
            [
                "github:numtide/llm-agents.nix#claude-code",
                "github:numtide/llm-agents.nix#codex"
            ]
        );
        assert_eq!(bay.ports[0].name, "web");
        let mount = &bay.mounts["/home/pilot/.app-data"];
        assert_eq!(
            (mount.host.as_str(), mount.writable),
            ("~/work/app-data", true)
        );
    }

    #[test]
    fn mounts_are_validated_when_the_config_loads() {
        for (mounts, why) in [
            (
                r#"{"~/a": {"host": "~/a", "rw": true}}"#,
                "rw: unknown setting",
            ),
            (r#"{"~/a": {"writable": true}}"#, "host: expected a string"),
            (
                r#"{"~/a": {"host": "~/a", "writable": "yes"}}"#,
                "expected true or false",
            ),
            (r#"{"/run/hangar/x": {"host": "~/a"}}"#, "must be in ~"),
            (r#"{"/etc/profile.d": {"host": "~/a"}}"#, "must be in ~"),
            (r#"{"~/../etc": {"host": "~/a"}}"#, "'..' isn't allowed"),
            (
                r#"{"~/a": {"host": "~/a"}, "~/a/b": {"host": "~/b"}}"#,
                "overlap",
            ),
        ] {
            let message =
                error(&one_bay(&format!(r#""mounts": {mounts}"#), ""));
            assert!(message.contains(why), "{mounts}: {message}");
        }
    }

    #[test]
    fn a_bay_image_defaults_to_the_release_matching_this_hangar() {
        let image = |config: &str| resolve_with(config).bays[0].image_ref();
        let version = env!("CARGO_PKG_VERSION");
        assert_eq!(
            image("{}"),
            format!("ghcr.io/zahidkizmaz/hangar-bay:v{version}")
        );
        assert_eq!(
            image(&one_bay(r#""imageRepository": "ghcr.io/fork/agent""#, "")),
            format!("ghcr.io/fork/agent:v{version}")
        );
        assert_eq!(
            image(&one_bay(r#""image": "hangar-bay:dev""#, "")),
            "hangar-bay:dev"
        );
    }

    #[test]
    fn initial_config_only_holds_what_users_set() {
        let config = json::parse(INITIAL_CONFIG).unwrap();
        let Json::Object(top) = &config else {
            panic!("not an object")
        };
        let keys: Vec<&String> = top.keys().collect();
        assert_eq!(keys, ["bays"]);
        let settings = resolve_with(INITIAL_CONFIG);
        assert_eq!(names(&settings), ["default"]);
        assert_eq!(settings.bays[0].packages, Vec::<String>::new());
    }

    #[test]
    fn bay_env_and_run_are_validated() {
        let lower = error(&one_bay(r#""env": {"lower": "x"}"#, ""));
        assert!(lower.contains("UPPER_SNAKE_CASE"), "{lower}");
        for secret in ["ghp_abc", "sk-ant-123", &"a".repeat(40)] {
            let message =
                error(&one_bay(&format!(r#""env": {{"T": "{secret}"}}"#), ""));
            assert!(message.contains("looks like a real secret"), "{message}");
        }
        let ok = resolve_with(&one_bay(
            r#""env": {"T": "placeholder value"}, "run": {"paper-clip2": "x"}"#,
            "",
        ));
        assert_eq!(ok.bays[0].env["T"], "placeholder value");
        assert_eq!(ok.bays[0].run["paper-clip2"], "x");
        let name = error(&one_bay(r#""run": {"Bad Name": "x"}"#, ""));
        assert_eq!(
            name,
            "config.bays.default.run.Bad Name: names are lowercase, digits and -"
        );
    }
}
