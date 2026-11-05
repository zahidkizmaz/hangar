//! The shared context of a command: the resolved settings, the state
//! directory, the package cache root and the sandbox backend.
//! Each bay is built from it on demand ([`Hangar::bay`]).

use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::bay::Bay;
use crate::config::{
    self, BaySettings, Paths, Settings, env_var, valid_bay_name,
};
use crate::dirs::{self, Base};
use crate::error::{Context, Error, Result};
use crate::files;
use crate::json;
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
        Ok(Self {
            settings,
            state,
            cache_root,
            host_home,
            sandbox,
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

#[cfg(test)]
mod tests {
    use super::{check_disjoint, sandbox};
    use std::path::Path;

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
    }
}
