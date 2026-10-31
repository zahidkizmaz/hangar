//! The shared context of a command: the resolved settings, the state
//! directory, the package cache root and the sandbox and broker backends.
//! Each bay is built from it on demand ([`Hangar::bay`]).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::bay::Bay;
use crate::broker::{AgentVault, Broker, Proxy};
use crate::config::{
    self, BaySettings, Paths, Settings, Tower, env_var, valid_bay_name,
};
use crate::dirs::{self, Base};
use crate::error::{Context, Error, Result, bail};
use crate::files;
use crate::json::{self, Json};
use crate::overview;
use crate::sandbox::{Msb, Sandbox, bay_vm};
use crate::state::StateDir;

pub(crate) struct Hangar {
    pub(crate) settings: Settings,
    pub(crate) state: StateDir,
    /// `$XDG_CACHE_HOME/hangar`: every bay's package cache is under it.
    pub(crate) cache_root: PathBuf,
    /// The user's `$HOME`, which the mount rules protect.
    pub(crate) host_home: PathBuf,
    pub(crate) sandbox: Rc<dyn Sandbox>,
    /// One broker per tower, by name.
    pub(crate) brokers: BTreeMap<String, Box<dyn Broker>>,
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
        let state = StateDir::new(config::state_dir(&settings, &env_var)?);
        let cache_root = dirs::hangar_dir(Base::Cache, &env_var)?;
        check_disjoint(state.root(), &cache_root)?;
        let host_home = files::host_home()?;
        let mut brokers = BTreeMap::new();
        for (name, tower) in &settings.towers {
            brokers.insert(
                name.clone(),
                broker(tower, sandbox.clone(), &state).context(&invalid)?,
            );
        }
        for (name, tower) in &settings.towers {
            for route in tower.routes.values() {
                brokers[name].validate(route).context(&invalid)?;
            }
        }
        for (scoped, whole) in config::uncovered_path_scopes(&settings) {
            log::warn!(
                "route {scoped} is scoped to a path, but route {whole} lets \
                 bays reach its whole host"
            );
        }
        for key in config::unused_credential_files(&settings) {
            log::warn!("credentialFiles {key} is used by no route");
        }
        overview::check_ports(&overview::published(&brokers, &settings))
            .context(&invalid)?;
        Ok(Self {
            settings,
            state,
            cache_root,
            host_home,
            sandbox,
            brokers,
        })
    }

    /// The broker of the tower named `tower`.
    pub(crate) fn broker(&self, tower: &str) -> Result<&dyn Broker> {
        self.brokers
            .get(tower)
            .map(Box::as_ref)
            .context(format!("no broker for tower {tower}"))
    }

    /// The tower `name` picks; without a name, the only tower, else an
    /// error listing them.
    pub(crate) fn tower(&self, name: Option<&str>) -> Result<&Tower> {
        if let Some(name) = name {
            return self
                .settings
                .towers
                .get(name)
                .context(format!("no tower named {name}"));
        }
        let mut names = self.settings.towers.keys();
        if let (Some(only), None) = (names.next(), names.next()) {
            Ok(&self.settings.towers[only])
        } else {
            let names: Vec<&str> =
                self.settings.towers.keys().map(String::as_str).collect();
            Err(Error::with_hint(
                format!("several towers: pass --tower ({})", names.join(", ")),
                "name one of the towers",
            ))
        }
    }

    /// The tower `name` names and its broker, looked up once.
    pub(crate) fn tower_context(
        &self,
        name: &str,
    ) -> Result<(&Tower, &dyn Broker)> {
        let tower = self
            .settings
            .towers
            .get(name)
            .context(format!("no tower named {name}"))?;
        Ok((tower, self.broker(name)?))
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

    /// Bays with a folder in `stateDir` but no config entry, by name. The
    /// sandbox can't list VMs, so this is the only way to find them;
    /// folders whose names aren't bay names aren't hangar's.
    pub(crate) fn leftovers(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.state.bays()) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| valid_bay_name(name) && self.bay(name).is_none())
            .collect();
        names.sort();
        names
    }
}

/// Each bay's home is only safe to mount because `stateDir` and the cache
/// root never overlap: the mount rules exempt a bay's own folder in one
/// root and refuse everything else in both.
fn check_disjoint(state: &Path, cache_root: &Path) -> Result<()> {
    if state.starts_with(cache_root) || cache_root.starts_with(state) {
        return Err(Error::with_hint(
            format!(
                "stateDir {} overlaps the package cache {}",
                state.display(),
                cache_root.display()
            ),
            "set stateDir (or XDG_DATA_HOME / XDG_CACHE_HOME) apart",
        ));
    }
    Ok(())
}

fn sandbox(backend: &str) -> Result<Rc<dyn Sandbox>> {
    match backend {
        "msb" => Ok(Rc::new(Msb)),
        other => Err(Error::new(format!(
            "config.sandbox.backend: unknown backend {other:?}; known: msb"
        ))),
    }
}

pub(crate) fn broker(
    tower: &Tower,
    sandbox: Rc<dyn Sandbox>,
    state: &StateDir,
) -> Result<Box<dyn Broker>> {
    let path = format!("config.towers.{}", tower.name);
    match tower.backend.as_str() {
        "agent-vault" => {
            reject(&path, tower, false)?;
            Ok(Box::new(AgentVault::new(tower, sandbox, state)?))
        }
        "proxy" => {
            reject(&path, tower, true)?;
            Ok(Box::new(Proxy::new(tower, sandbox.as_ref(), state)?))
        }
        other => Err(Error::new(format!(
            "{path}.backend: unknown backend {other:?}; known: \
             agent-vault, proxy"
        ))),
    }
}

/// A tower's backend reads one object (`agentVault` or `proxy`) and its
/// own routes and credentials; the other backend's would be dead config,
/// refused instead of ignored.
fn reject(path: &str, tower: &Tower, is_proxy: bool) -> Result<()> {
    let has = |name: &str| matches!(&tower.config, Json::Object(map) if map.contains_key(name));
    if is_proxy {
        if has("agentVault") {
            bail!("{path}.agentVault: only for an agent-vault tower");
        }
        if !tower.routes.is_empty() {
            bail!(
                "{path}.routes: a proxy tower manages its own; hangar \
                 can't set them"
            );
        }
        if !tower.credential_files.is_empty() {
            bail!(
                "{path}.credentialFiles: a proxy tower manages its own \
                 credentials"
            );
        }
        if tower.master_password_file.is_some() {
            bail!(
                "{path}.masterPasswordFile: a proxy tower has no local \
                 vault"
            );
        }
    } else if has("proxy") {
        bail!("{path}.proxy: only for a proxy tower");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{broker, check_disjoint, sandbox};
    use crate::config::DEFAULT_TOWER;
    use crate::sandbox::fake::FakeSandbox;
    use crate::state::StateDir;
    use crate::testing::settings;
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
            "stateDir /h/.cache/hangar/state overlaps the package cache \
             /h/.cache/hangar: set stateDir (or XDG_DATA_HOME / XDG_CACHE_HOME) \
             apart"
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
            r#"config.sandbox.backend: unknown backend "docker"; known: msb"#
        );
        let state = StateDir::new(Path::new("/nonexistent").to_path_buf());
        let load = |config: &str| {
            let settings = settings(config);
            broker(
                &settings.towers[DEFAULT_TOWER],
                Rc::new(FakeSandbox::default()),
                &state,
            )
        };
        assert!(load("{}").is_ok());
        let error = load(r#"{"towers": {"default": {"backend": "vault"}}}"#)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            error,
            r#"config.towers.default.backend: unknown backend "vault"; known: agent-vault, proxy"#
        );
    }
}
