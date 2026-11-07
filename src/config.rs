use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::apps::{self, App};
use crate::dirs::{self, Base};
use crate::error::{Context, Error, Result, bail};
use crate::files;
use crate::mounts::{self, Mode, MountSpec};
use crate::proxy::{self, Location};
use crate::sandbox::PublishedPort;
use crate::secret::{SECRET_PREFIXES, find_secret};
use miniserde::json::Object;

use crate::json::{self, Json};

const DEFAULTS: &str = include_str!("../config/defaults.json");

pub(crate) const INITIAL_CONFIG: &str = r#"{
  "proxies": {},
  "bays": [{ "name": "default", "proxy": null, "apps": [] }]
}
"#;

const TOP_LEVEL: [&str; 5] = ["proxies", "defaults", "bays", "apps", "sandbox"];
const DEFAULTS_KEYS: [&str; 1] = ["proxy"];
const BAY_KEYS: [&str; 13] = [
    "name",
    "proxy",
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
    "mounts",
];

pub(crate) type Var<'a> = &'a dyn Fn(&str) -> Option<String>;

pub(crate) fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[derive(Debug)]
pub(crate) struct Settings {
    pub(crate) sandbox_backend: String,
    pub(crate) proxies: BTreeMap<String, ProxySettings>,
    pub(crate) bays: Vec<BaySettings>,
}

#[derive(Debug)]
pub(crate) struct ProxySettings {
    pub(crate) name: String,
    pub(crate) location: Location,
    pub(crate) raw: Json,
}

#[derive(Debug)]
pub(crate) struct BaySettings {
    pub(crate) name: String,
    pub(crate) proxy: Option<String>,
    pub(crate) apps: Vec<App>,
    pub(crate) ports: Vec<PublishedPort>,
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
    pub(crate) mounts: BTreeMap<String, MountSpec>,
}

/// Where release images are pushed; `config/defaults.json` says the same.
const RELEASE_REPOSITORY: &str = "ghcr.io/zahidkizmaz/hangar-bay";

impl BaySettings {
    pub(crate) fn image_ref(&self) -> String {
        self.image.clone().unwrap_or_else(|| {
            release_image(
                &self.image_repository,
                env!("CARGO_PKG_VERSION"),
                option_env!("HANGAR_BAY_IMAGE_DIGEST"),
            )
        })
    }
}

/// The image released with this hangar. A release build carries the
/// image's digest, so a re-pushed tag changes nothing; it only fits the
/// repository it was pushed to.
fn release_image(
    repository: &str,
    version: &str,
    digest: Option<&str>,
) -> String {
    let tag = format!("{repository}:v{version}");
    match digest {
        Some(digest) if repository == RELEASE_REPOSITORY => {
            format!("{tag}@{digest}")
        }
        _ => tag,
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

pub(crate) fn resolve(defaults: Json, config: Json) -> Result<Settings> {
    let Json::Object(mut config) = config else {
        bail!("config must be a JSON object");
    };
    Fields::new(&Json::Object(config.clone()), "")?.only(&TOP_LEVEL)?;
    let Json::Object(mut merged) = defaults else {
        bail!("built-in defaults must be a JSON object");
    };
    let bay_defaults = merged.remove("bay").context("defaults: no bay")?;
    let catalog = apps::Catalog::new(config.remove("apps"))?;
    let proxies = proxy_settings(config.remove("proxies"))?;
    let default_proxy = default_proxy(config.remove("defaults"), &proxies)?;
    let bays = config.remove("bays");
    let mut merged = Json::Object(merged);
    merge(&mut merged, Json::Object(config));
    let top = Fields::new(&merged, "")?;

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

    top.object("sandbox")?.only(&["backend"])?;
    let mut settings = Settings {
        sandbox_backend: top.object("sandbox")?.string("backend")?,
        proxies,
        bays: Vec::new(),
    };
    for (bay, apps) in bays.iter().zip(&enabled) {
        Fields::new(bay, &bay_path(bay))?.only(&BAY_KEYS)?;
        let proxy =
            bay_proxy(bay, default_proxy.as_deref(), &settings.proxies)?;
        let mut bay = BaySettings::from_json(bay, proxy)?;
        apps::apply(&mut bay, apps)?;
        settings.bays.push(bay);
    }
    one_namespace(&settings)?;
    Ok(settings)
}

fn proxy_settings(
    proxies: Option<Json>,
) -> Result<BTreeMap<String, ProxySettings>> {
    let entries = match proxies {
        None | Some(Json::Null) => Object::new(),
        Some(Json::Object(entries)) => entries,
        Some(_) => bail!("proxies: expected an object"),
    };
    let mut settings = BTreeMap::new();
    for (name, raw) in entries {
        let path = format!("proxies.{name}");
        if !valid_name(&name) {
            bail!("{path}: {}", NAME_RULE);
        }
        let fields = Fields::new(&raw, &path)?;
        proxy::check_kind(&fields)?;
        let location = proxy::location(&fields)?;
        let entry = ProxySettings {
            name: name.clone(),
            location,
            raw,
        };
        settings.insert(name, entry);
    }
    Ok(settings)
}

fn default_proxy(
    defaults: Option<Json>,
    proxies: &BTreeMap<String, ProxySettings>,
) -> Result<Option<String>> {
    let Some(defaults) = defaults else {
        return Ok(None);
    };
    let fields = Fields::new(&defaults, "defaults")?;
    fields.only(&DEFAULTS_KEYS)?;
    let name = fields.optional_string("proxy")?;
    if let Some(name) = &name {
        known_proxy(&fields.at("proxy"), name, proxies)?;
    }
    Ok(name)
}

fn known_proxy(
    path: &str,
    name: &str,
    proxies: &BTreeMap<String, ProxySettings>,
) -> Result<()> {
    if proxies.contains_key(name) {
        return Ok(());
    }
    let known: Vec<&str> = proxies.keys().map(String::as_str).collect();
    let known = if known.is_empty() {
        "none".to_string()
    } else {
        known.join(", ")
    };
    bail!("{path}: no proxy \"{name}\"; known: {known}")
}

fn bay_proxy(
    bay: &Json,
    default: Option<&str>,
    proxies: &BTreeMap<String, ProxySettings>,
) -> Result<Option<String>> {
    let bay = Fields::new(bay, &bay_path(bay))?;
    let name = match bay.map.get("proxy") {
        Some(Json::Null) => return Ok(None),
        Some(Json::String(name)) => name.as_str(),
        Some(_) => bail!("{}: expected a proxy name or null", bay.at("proxy")),
        None => default.context(format!(
            "{}: no proxy; set \"proxy\", defaults.proxy, or \
             \"proxy\": null for direct internet",
            bay.path()
        ))?,
    };
    known_proxy(&bay.at("proxy"), name, proxies)?;
    Ok(Some(name.to_string()))
}

fn one_namespace(settings: &Settings) -> Result<()> {
    let shared = settings
        .bays
        .iter()
        .find(|bay| settings.proxies.contains_key(&bay.name));
    if let Some(bay) = shared {
        bail!("name \"{}\" is used by a bay and a proxy", bay.name);
    }
    Ok(())
}

fn bay_entries(bays: Option<Json>) -> Result<Vec<Json>> {
    let items = match bays {
        None | Some(Json::Null) => Vec::new(),
        Some(Json::Array(items)) => items.into_iter().collect(),
        Some(_) => bail!("bays: expected a list"),
    };
    if items.is_empty() {
        return Ok(vec![json::object([("name", json::string("default"))])]);
    }
    let mut names = BTreeSet::new();
    for (index, item) in items.iter().enumerate() {
        let entry = Fields::new(item, &format!("bays.{index}"))?;
        let name = entry.string("name")?;
        if !valid_name(&name) {
            bail!("bays.{index}.name: {name:?}: {NAME_RULE}");
        }
        if !names.insert(name.clone()) {
            bail!("bays: two bays named {name}");
        }
    }
    Ok(items)
}

fn bay_path(bay: &Json) -> String {
    match bay {
        Json::Object(bay) if let Some(Json::String(name)) = bay.get("name") => {
            format!("bays.{name}")
        }
        _ => "bays".to_string(),
    }
}

impl BaySettings {
    fn from_json(value: &Json, proxy: Option<String>) -> Result<Self> {
        let bay = Fields::new(value, &bay_path(value))?;
        let settings = Self {
            name: bay.string("name")?,
            proxy,
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
            run: BTreeMap::new(),
            mounts: bay_mounts(&bay)?,
        };
        mounts::check_targets(&settings.mounts)?;
        Ok(settings)
    }
}

fn valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars.next().is_some_and(|first| first.is_ascii_uppercase())
        && chars
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Real-looking secrets don't belong in `env`: the VM sees them. A
/// placeholder is one short value, so this is stricter than the scan of
/// copied files: any secret prefix counts, and so does a long token.
fn looks_like_a_secret(value: &str) -> bool {
    SECRET_PREFIXES
        .iter()
        .chain(&["xox", "-----BEGIN"])
        .any(|prefix| value.starts_with(prefix))
        || find_secret(value).is_some()
        || (value.len() >= 40 && !value.contains(' '))
}

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

pub(crate) fn valid_app_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

const NAME_RULE: &str = "lowercase letters and digits, joined by single -, \
                         at most 32 characters";

pub(crate) fn valid_name(name: &str) -> bool {
    name.len() <= 32
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
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

fn mount_mode(entry: &Fields) -> Result<Mode> {
    match entry.map.get("mode") {
        Some(Json::String(mode)) if mode == "ro" => Ok(Mode::Ro),
        Some(Json::String(mode)) if mode == "rw" => Ok(Mode::Rw),
        Some(Json::String(mode)) if mode == "copy" => Ok(Mode::Copy),
        _ => bail!("{}: expected ro, rw or copy", entry.at("mode")),
    }
}

/// Whatever an agent writes to a writable mount is on this machine, where
/// a git hook, a repo's `core.fsmonitor` or an `.envrc` runs it outside
/// the bay; it can create those anywhere in the folder, at any time. So
/// `rw` is only for a folder the user explicitly gives up.
fn check_unsafe(entry: &Fields, mode: Mode) -> Result<()> {
    let unsafe_ok = entry.flag_or("unsafe", false)?;
    match (mode, unsafe_ok) {
        (Mode::Rw, false) => Err(Error::with_hint(
            format!(
                "{}: a writable mount lets the agent leave files this \
                 machine runs (git hooks and config, .envrc, scripts)",
                entry.at("mode")
            ),
            "use \"ro\" or \"copy\", keep the work in the bay, or accept \
             it with \"unsafe\": true",
        )),
        (Mode::Ro | Mode::Copy, true) => {
            bail!("{}: only a \"rw\" mount can be unsafe", entry.at("unsafe"))
        }
        _ => Ok(()),
    }
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
            entry.only(&["host", "mode", "unsafe"])?;
            let expanded = files::vm_path(vm).context(entries.at(vm))?;
            let mode = mount_mode(&entry)?;
            check_unsafe(&entry, mode)?;
            let spec = MountSpec {
                host: entry.string("host")?,
                mode,
            };
            Ok((expanded, spec))
        })
        .collect()
}

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
        DIRECT, config_error as error, one_bay, settings as resolve_with, vars,
    };

    const FAKE: &str = r#"{"kind": "fake", "run": "managed"}"#;

    fn with_default_proxy(bays: &str) -> String {
        format!(
            r#"{{"proxies": {{"p": {FAKE}}}, "defaults": {{"proxy": "p"}},
                 {bays}}}"#
        )
    }

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
    fn module_fixture_is_a_valid_config() {
        let settings = resolve_with(include_str!("../tests/module.json"));
        let proxies: Vec<&str> =
            settings.proxies.keys().map(String::as_str).collect();
        assert_eq!(proxies, ["vault"]);
        assert!(matches!(
            settings.proxies["vault"].location,
            Location::Managed
        ));
        let [default, oss] = &settings.bays[..] else {
            panic!("two bays expected");
        };
        assert_eq!(default.proxy.as_deref(), Some("vault"));
        assert_eq!(oss.proxy, None);
        assert_eq!(oss.image_repository, "ghcr.io/example/hangar-bay");
        let modes: Vec<Mode> =
            default.mounts.values().map(|mount| mount.mode).collect();
        assert_eq!(modes, [Mode::Copy, Mode::Ro, Mode::Rw]);
        assert_eq!(default.ports[0].host, 3200);
    }

    #[test]
    fn no_bays_is_one_bay_named_default() {
        for bays in [r#""apps": {}"#, r#""bays": null"#, r#""bays": []"#] {
            let settings = resolve_with(&with_default_proxy(bays));
            assert_eq!(names(&settings), ["default"]);
            assert_eq!(settings.bays[0].cpus, 4);
        }
    }

    #[test]
    fn bays_keep_their_order_and_each_gets_the_defaults() {
        let settings = resolve_with(&with_default_proxy(
            r#""bays": [{"name": "work", "cpus": 8}, {"name": "oss"},
                        {"name": "a-1"}]"#,
        ));
        assert_eq!(names(&settings), ["work", "oss", "a-1"]);
        assert_eq!(settings.bays[0].cpus, 8);
        assert_eq!(settings.bays[0].memory, "6G");
        assert_eq!(settings.bays[1].cpus, 4);
    }

    #[test]
    fn bay_names_are_required_unique_and_well_formed() {
        assert_eq!(
            error(r#"{"bays": [{"cpus": 2}]}"#),
            "bays.0.name: expected a string"
        );
        assert_eq!(
            error(r#"{"bays": [{"name": "a"}, {"name": "a"}]}"#),
            "bays: two bays named a"
        );
        assert_eq!(error(r#"{"bays": {}}"#), "bays: expected a list");
        for good in ["a", "a-b-1", &"a".repeat(32)] {
            assert!(valid_name(good), "{good}");
        }
        for bad in ["", "A", "x-", "-x", "a--b", "a_b", &"a".repeat(33)] {
            assert!(!valid_name(bad), "{bad}");
            let config = format!(r#"{{"bays": [{{"name": "{bad}"}}]}}"#);
            assert!(error(&config).contains("lowercase letters and digits"));
        }
    }

    #[test]
    fn misplaced_keys_are_refused() {
        for key in ["bay", "credentialFiles", "tower"] {
            let config = format!(r#"{{"{key}": {{}}}}"#);
            assert_eq!(error(&config), format!("{key}: unknown setting"));
        }
    }

    #[test]
    fn bad_bay_values_name_the_bay() {
        assert_eq!(
            error(&one_bay(r#""packages": ["a", 1]"#, "")),
            "bays.default.packages: expected strings"
        );
        assert_eq!(
            error(&one_bay(r#""cpus": 8.5"#, "")),
            "bays.default.cpus: expected a whole number"
        );
        assert_eq!(
            error(&one_bay(r#""cpu": 8"#, "")),
            "bays.default.cpu: unknown setting"
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
    fn mounts_are_validated_when_the_config_loads() {
        for (mounts, why) in [
            (
                r#"{"~/a": {"host": "~/a", "mode": "ro", "rw": true}}"#,
                "rw: unknown setting",
            ),
            (r#"{"~/a": {"mode": "ro"}}"#, "host: expected a string"),
            (
                r#"{"/run/hangar/x": {"host": "~/a", "mode": "ro"}}"#,
                "must be in ~",
            ),
            (
                r#"{"/etc/profile.d": {"host": "~/a", "mode": "ro"}}"#,
                "must be in ~",
            ),
            (
                r#"{"~/../etc": {"host": "~/a", "mode": "ro"}}"#,
                "'..' isn't allowed",
            ),
            (
                r#"{"~/a": {"host": "~/a", "mode": "ro"},
                    "~/a/b": {"host": "~/b", "mode": "ro"}}"#,
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
            image(DIRECT),
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
        assert_eq!(keys, ["bays", "proxies"]);
        let settings = resolve_with(INITIAL_CONFIG);
        assert_eq!(names(&settings), ["default"]);
        assert_eq!(settings.bays[0].proxy, None);
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
        let ok =
            resolve_with(&one_bay(r#""env": {"T": "placeholder value"}"#, ""));
        assert_eq!(ok.bays[0].env["T"], "placeholder value");
    }

    #[test]
    fn bay_proxy_absent_uses_the_default() {
        let config = with_default_proxy(r#""bays": [{"name": "work"}]"#);
        let settings = resolve_with(&config);
        assert_eq!(settings.bays[0].proxy.as_deref(), Some("p"));
    }

    #[test]
    fn bay_proxy_null_means_direct() {
        let config =
            with_default_proxy(r#""bays": [{"name": "work", "proxy": null}]"#);
        assert_eq!(resolve_with(&config).bays[0].proxy, None);
    }

    #[test]
    fn bay_without_any_proxy_choice_is_an_error() {
        assert_eq!(
            error(r#"{"bays": [{"name": "work"}]}"#),
            "bays.work: no proxy; set \"proxy\", defaults.proxy, or \
             \"proxy\": null for direct internet"
        );
    }

    #[test]
    fn unknown_proxy_name_is_an_error() {
        let config = r#"{"proxies": {"infi": {"kind": "fake", "run": "managed"}},
            "bays": [{"name": "work", "proxy": "nope"}]}"#;
        assert_eq!(
            error(config),
            "bays.work.proxy: no proxy \"nope\"; known: infi"
        );
        let config = r#"{"defaults": {"proxy": "nope"}}"#;
        assert_eq!(
            error(config),
            "defaults.proxy: no proxy \"nope\"; known: none"
        );
        assert_eq!(
            error(&one_bay(r#""proxy": 3"#, "")),
            "bays.default.proxy: expected a proxy name or null"
        );
    }

    #[test]
    fn names_are_one_namespace() {
        let config = format!(
            r#"{{"proxies": {{"work": {FAKE}}},
                 "bays": [{{"name": "work", "proxy": null}}]}}"#
        );
        assert_eq!(
            error(&config),
            "name \"work\" is used by a bay and a proxy"
        );
    }

    #[test]
    fn proxy_names_follow_the_bay_name_rule() {
        let config = format!(r#"{{"proxies": {{"Bad": {FAKE}}}}}"#);
        assert!(error(&config).starts_with("proxies.Bad: lowercase"));
        assert_eq!(error(r#"{"proxies": []}"#), "proxies: expected an object");
    }

    #[test]
    fn mount_mode_must_be_ro_rw_or_copy() {
        let mount = |entry: &str| {
            one_bay(&format!(r#""mounts": {{"~/code": {entry}}}"#), "")
        };
        let expected =
            "bays.default.mounts.~/code.mode: expected ro, rw or copy";
        for entry in [
            r#"{"host": "~/c", "mode": "writable"}"#,
            r#"{"host": "~/c"}"#,
            r#"{"host": "~/c", "mode": 1}"#,
        ] {
            assert_eq!(error(&mount(entry)), expected, "{entry}");
        }
        for (entry, want) in [
            (r#"{"host": "~/c", "mode": "ro"}"#, Mode::Ro),
            (r#"{"host": "~/c", "mode": "rw", "unsafe": true}"#, Mode::Rw),
            (r#"{"host": "~/c", "mode": "copy"}"#, Mode::Copy),
        ] {
            let bay = resolve_with(&mount(entry)).bays.remove(0);
            assert_eq!(bay.mounts["/home/pilot/code"].mode, want);
        }
    }

    #[test]
    fn a_writable_mount_needs_the_risk_accepted() {
        let mount = |entry: &str| {
            one_bay(&format!(r#""mounts": {{"~/code": {entry}}}"#), "")
        };
        let refused = error(&mount(r#"{"host": "~/c", "mode": "rw"}"#));
        assert!(
            refused.starts_with(
                "bays.default.mounts.~/code.mode: a writable mount lets the \
                 agent leave files this machine runs"
            ),
            "{refused}"
        );
        assert_eq!(
            error(&mount(r#"{"host": "~/c", "mode": "ro", "unsafe": true}"#)),
            "bays.default.mounts.~/code.unsafe: only a \"rw\" mount can be \
             unsafe"
        );
    }

    #[test]
    fn a_release_pins_its_own_image_by_digest() {
        use super::{RELEASE_REPOSITORY, release_image};
        let digest = Some("sha256:abc");
        assert_eq!(
            release_image(RELEASE_REPOSITORY, "0.1.0", digest),
            format!("{RELEASE_REPOSITORY}:v0.1.0@sha256:abc")
        );
        assert_eq!(
            release_image(RELEASE_REPOSITORY, "0.1.0", None),
            format!("{RELEASE_REPOSITORY}:v0.1.0")
        );
        assert_eq!(
            release_image("ghcr.io/fork/bay", "0.1.0", digest),
            "ghcr.io/fork/bay:v0.1.0"
        );
        let bay = resolve_with(r#"{"bays": [{"name": "a", "proxy": null}]}"#)
            .bays
            .remove(0);
        assert_eq!(bay.image_repository, RELEASE_REPOSITORY);
    }

    #[test]
    fn old_keys_are_unknown() {
        for (key, config) in [
            ("files", one_bay(r#""files": {}"#, "")),
            ("tower", r#"{"tower": {}}"#.to_string()),
            ("appDefinitions", r#"{"appDefinitions": {}}"#.to_string()),
            ("stateDir", r#"{"stateDir": "/x"}"#.to_string()),
            ("home", one_bay(r#""home": false"#, "")),
            ("cache", one_bay(r#""cache": false"#, "")),
            ("run", one_bay(r#""run": {}"#, "")),
        ] {
            let path = if config.contains("bays") {
                format!("bays.default.{key}")
            } else {
                key.to_string()
            };
            assert_eq!(
                error(&config),
                format!("{path}: unknown setting"),
                "{key}"
            );
        }
    }

    #[test]
    fn an_unregistered_kind_is_an_error() {
        let config =
            r#"{"proxies": {"p": {"kind": "nope", "run": "managed"}}}"#;
        assert!(
            error(config)
                .starts_with("proxies.p.kind: unknown kind \"nope\"; known: "),
            "{}",
            error(config)
        );
    }

    #[test]
    fn apps_key_replaces_app_definitions() {
        let config = one_bay(
            r#""apps": ["paperclip"]"#,
            r#""apps": {"paperclip": {"packages": ["x"]}}"#,
        );
        let bay = resolve_with(&config).bays.remove(0);
        assert_eq!(bay.packages, ["x"]);
    }
}
