//! The shared context of a command: the resolved settings, the state
//! directory, the package cache root and the sandbox backend.
//! Each bay is built from it on demand ([`Hangar::bay`]).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::bay::Bay;
use crate::config::{self, BaySettings, Paths, Settings, env_var, valid_name};
use crate::dirs::{self, Base};
use crate::error::{Context, Error, Result, bail};
use crate::files;
use crate::json;
use crate::lifecycle::Resource;
use crate::proxy::{self, Proxy, Spec};
use crate::sandbox::{Msb, Sandbox, bay_vm};
use crate::state::StateDir;

pub(crate) struct Hangar {
    pub(crate) settings: Settings,
    pub(crate) state: StateDir,
    /// `$XDG_CACHE_HOME/hangar`: every bay's package cache is under it.
    pub(crate) cache_root: PathBuf,
    /// `$XDG_STATE_HOME/hangar`: the log and the lock.
    pub(crate) state_root: PathBuf,
    /// The user's `$HOME`, which the mount rules protect.
    pub(crate) host_home: PathBuf,
    pub(crate) sandbox: Rc<dyn Sandbox>,
    pub(crate) proxies: BTreeMap<String, Box<dyn Proxy>>,
}

impl Hangar {
    pub(crate) fn load(paths: &Paths) -> Result<Self> {
        let path = &paths.config;
        let Ok(text) = fs::read_to_string(path) else {
            return Err(Error::with_hint(
                format!("no config at {}", path.display()),
                "run 'hangar init' or set HANGAR_CONFIG",
            ));
        };
        let invalid = format!("invalid config {}", path.display());
        let config = json::parse(&text).context(&invalid)?;
        let settings =
            config::resolve(config::defaults()?, config).context(&invalid)?;
        let sandbox = sandbox(&settings.sandbox_backend).context(&invalid)?;
        let state = StateDir::new(dirs::hangar_dir(Base::Data, &env_var)?);
        let cache_root = dirs::hangar_dir(Base::Cache, &env_var)?;
        check_disjoint(state.root(), &cache_root)?;
        let host_home = files::host_home()?;
        let proxies =
            build_proxies(&settings, &state, &host_home).context(&invalid)?;
        check_ports(&settings, &proxies).context(&invalid)?;
        Ok(Self {
            settings,
            state,
            cache_root,
            state_root: dirs::hangar_dir(Base::State, &env_var)?,
            host_home,
            sandbox,
            proxies,
        })
    }

    pub(crate) fn bay(&self, name: &str) -> Option<Bay<'_>> {
        self.settings
            .bays
            .iter()
            .find(|bay| bay.name == name)
            .map(|settings| self.build(settings))
    }

    pub(crate) fn bays(&self) -> Vec<Bay<'_>> {
        self.settings
            .bays
            .iter()
            .map(|bay| self.build(bay))
            .collect()
    }

    /// The bay's proxy and its config name; none for a direct bay.
    pub(crate) fn proxy_of(
        &self,
        bay: &BaySettings,
    ) -> Option<(&str, &dyn Proxy)> {
        let (name, proxy) =
            self.proxies.get_key_value(bay.proxy.as_deref()?)?;
        Some((name.as_str(), proxy.as_ref()))
    }

    fn build<'a>(&'a self, settings: &'a BaySettings) -> Bay<'a> {
        let name = settings.name.as_str();
        let dir = self.state.bay(name);
        Bay {
            name,
            vm: bay_vm(name),
            settings,
            dir,
            cache: self.cache_root.join("bays").join(name),
        }
    }

    /// Bays and proxies with a folder in the data dir but no config entry.
    /// The sandbox can't list VMs, so this is the only way to find them;
    /// folders whose names aren't valid names aren't hangar's.
    pub(crate) fn leftovers(&self) -> Vec<(Resource, String)> {
        let bays = folders(&self.state.bays())
            .into_iter()
            .filter(|name| self.bay(name).is_none())
            .map(|name| (Resource::Bay, name));
        let proxies = folders(&self.state.proxies())
            .into_iter()
            .filter(|name| !self.proxies.contains_key(name))
            .map(|name| (Resource::Proxy, name));
        bays.chain(proxies).collect()
    }
}

/// The sorted names of the valid-named folders in `dir`.
fn folders(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| valid_name(name))
        .collect();
    names.sort();
    names
}

pub(crate) fn build_proxies(
    settings: &Settings,
    state: &StateDir,
    home: &Path,
) -> Result<BTreeMap<String, Box<dyn Proxy>>> {
    settings
        .proxies
        .values()
        .map(|entry| {
            let path = format!("proxies.{}", entry.name);
            let spec = Spec {
                name: &entry.name,
                fields: config::Fields::new(&entry.raw, &path)?,
                location: entry.location.clone(),
                home,
                data: state.proxy(&entry.name).data(),
            };
            Ok((entry.name.clone(), proxy::build(&spec)?))
        })
        .collect()
}

/// No two things publish the same host port: managed proxies and bays.
pub(crate) fn check_ports(
    settings: &Settings,
    proxies: &BTreeMap<String, Box<dyn Proxy>>,
) -> Result<()> {
    let managed = proxies.iter().filter_map(|(name, proxy)| {
        let (host, _) = proxy.managed()?.vm().publish;
        Some((host, format!("proxy {name}")))
    });
    let bays = settings.bays.iter().flat_map(|bay| {
        bay.ports
            .iter()
            .map(|port| (port.host, format!("{}/{}", bay.name, port.name)))
    });
    let mut owners = BTreeMap::new();
    for (port, owner) in managed.chain(bays) {
        if let Some(first) = owners.insert(port, owner.clone()) {
            bail!("port {port}: {first} and {owner}");
        }
    }
    Ok(())
}

/// Each bay's home is only safe to mount because the data dir and the
/// cache root never overlap: the mount rules exempt a bay's own folder in
/// one root and refuse everything else in both.
fn check_disjoint(data: &Path, cache_root: &Path) -> Result<()> {
    if data.starts_with(cache_root) || cache_root.starts_with(data) {
        return Err(Error::with_hint(
            format!(
                "the data dir {} overlaps the package cache {}",
                data.display(),
                cache_root.display()
            ),
            "set XDG_DATA_HOME and XDG_CACHE_HOME apart",
        ));
    }
    Ok(())
}

fn sandbox(backend: &str) -> Result<Rc<dyn Sandbox>> {
    match backend {
        "msb" => Ok(Rc::new(Msb)),
        other => Err(Error::new(format!(
            "sandbox.backend: unknown backend {other:?}; known: msb"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::{build_proxies, check_disjoint, check_ports, sandbox};
    use crate::lifecycle::Resource;
    use crate::sandbox::fake::FakeSandbox;
    use crate::testing::{hangar_with, scratch_dir, settings};
    use std::fs;
    use std::path::Path;
    use std::rc::Rc;

    #[test]
    fn state_and_the_package_caches_never_overlap() {
        let check = |state: &str, cache: &str| {
            check_disjoint(Path::new(state), Path::new(cache))
                .map_err(|e| e.to_string())
        };
        assert!(check("/h/.local/share/hangar", "/h/.cache/hangar").is_ok());
        assert!(
            check("/h/.local/share/hangar", "/h/.local/share/hangar-cache")
                .is_ok()
        );
        let error =
            check("/h/.cache/hangar/state", "/h/.cache/hangar").unwrap_err();
        assert_eq!(
            error,
            "the data dir /h/.cache/hangar/state overlaps the package cache \
             /h/.cache/hangar: set XDG_DATA_HOME and XDG_CACHE_HOME apart"
        );
        assert!(check("/h/data", "/h/data/cache").is_err());
        assert!(check("/h/same", "/h/same").is_err());
    }

    #[test]
    fn only_known_backends_load() {
        assert!(sandbox("msb").is_ok());
        let error = sandbox("docker").err().unwrap().to_string();
        assert_eq!(
            error,
            r#"sandbox.backend: unknown backend "docker"; known: msb"#
        );
    }

    const MANAGED_AT_3100: &str = r#"{"proxies": {"p":
        {"kind": "fake", "run": "managed", "managed": true, "port": 3100}},
        "bays": [{"name": "work", "proxy": "p", "apps": ["paperclip"]}]}"#;

    #[test]
    fn published_ports_never_collide() {
        let settings = settings(MANAGED_AT_3100);
        let state = scratch_dir("hangar-ports");
        let proxies = build_proxies(
            &settings,
            &crate::state::StateDir::new(state),
            Path::new("/home/u"),
        )
        .unwrap();
        let error = check_ports(&settings, &proxies).unwrap_err();
        assert_eq!(error.to_string(), "port 3100: proxy p and work/paperclip");

        let moved =
            MANAGED_AT_3100.replace(r#""port": 3100"#, r#""port": 3101"#);
        let settings = crate::testing::settings(&moved);
        let proxies = build_proxies(
            &settings,
            &crate::state::StateDir::new(scratch_dir("hangar-ports-ok")),
            Path::new("/home/u"),
        )
        .unwrap();
        assert!(check_ports(&settings, &proxies).is_ok());
    }

    #[test]
    fn leftovers_cover_bays_and_proxies() {
        let state = scratch_dir("hangar-leftovers");
        let hangar = hangar_with(
            MANAGED_AT_3100,
            &state,
            Rc::new(FakeSandbox::default()),
        );
        for folder in ["bays/old", "bays/work", "proxies/gone", "proxies/p"] {
            fs::create_dir_all(state.join(folder)).unwrap();
        }
        fs::create_dir_all(state.join("bays/Not_A_Name")).unwrap();
        assert_eq!(
            hangar.leftovers(),
            [
                (Resource::Bay, "old".to_string()),
                (Resource::Proxy, "gone".to_string())
            ]
        );
    }

    #[test]
    fn a_bay_finds_its_proxy_by_config_name() {
        let state = scratch_dir("hangar-proxy-of");
        let hangar = hangar_with(
            MANAGED_AT_3100,
            &state,
            Rc::new(FakeSandbox::default()),
        );
        let (name, proxy) = hangar.proxy_of(&hangar.settings.bays[0]).unwrap();
        assert_eq!((name, proxy.kind()), ("p", "fake"));
        let direct = hangar_with(
            crate::testing::DIRECT,
            &state,
            Rc::new(FakeSandbox::default()),
        );
        assert!(direct.proxy_of(&direct.settings.bays[0]).is_none());
    }
}
