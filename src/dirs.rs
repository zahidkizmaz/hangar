//! Where hangar keeps things on the host, per the XDG Base Directory
//! spec.

use std::path::{Path, PathBuf};

use crate::config::Var;
use crate::error::{Context, Result};

#[derive(Debug, Clone, Copy)]
pub(crate) enum Base {
    Config,
    Data,
    Cache,
    State,
}

impl Base {
    fn var(self) -> &'static str {
        match self {
            Self::Config => "XDG_CONFIG_HOME",
            Self::Data => "XDG_DATA_HOME",
            Self::Cache => "XDG_CACHE_HOME",
            Self::State => "XDG_STATE_HOME",
        }
    }

    fn fallback(self) -> &'static str {
        match self {
            Self::Config => ".config",
            Self::Data => ".local/share",
            Self::Cache => ".cache",
            Self::State => ".local/state",
        }
    }
}

/// `$XDG_…_HOME/hangar`, or the spec's fallback under `$HOME`; a relative
/// value is ignored, as the spec says.
pub(crate) fn hangar_dir(base: Base, var: Var) -> Result<PathBuf> {
    let root = match var(base.var()).filter(|dir| Path::new(dir).is_absolute())
    {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(var("HOME").context("HOME is not set")?)
            .join(base.fallback()),
    };
    Ok(root.join("hangar"))
}

#[cfg(test)]
mod tests {
    use super::{Base, hangar_dir};
    use crate::testing::vars;
    use std::path::Path;

    #[test]
    fn each_base_follows_its_variable_or_falls_back_under_home() {
        for (base, var, fallback) in [
            (Base::Config, "XDG_CONFIG_HOME", "/h/.config/hangar"),
            (Base::Data, "XDG_DATA_HOME", "/h/.local/share/hangar"),
            (Base::Cache, "XDG_CACHE_HOME", "/h/.cache/hangar"),
            (Base::State, "XDG_STATE_HOME", "/h/.local/state/hangar"),
        ] {
            let home = vars(&[("HOME", "/h")]);
            assert_eq!(hangar_dir(base, &home).unwrap(), Path::new(fallback));
            let set = vars(&[("HOME", "/h"), (var, "/xdg")]);
            assert_eq!(
                hangar_dir(base, &set).unwrap(),
                Path::new("/xdg/hangar")
            );
            let relative = vars(&[("HOME", "/h"), (var, "xdg")]);
            assert_eq!(
                hangar_dir(base, &relative).unwrap(),
                Path::new(fallback)
            );
            let error = hangar_dir(base, &vars(&[])).unwrap_err();
            assert_eq!(error.to_string(), "HOME is not set");
            let without_home = vars(&[(var, "/xdg")]);
            assert!(hangar_dir(base, &without_home).is_ok());
        }
    }
}
