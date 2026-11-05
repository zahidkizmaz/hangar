//! What each command does. Commands return data for `output` to render;
//! `main` only parses arguments and dispatches here.

use std::fs;

use crate::config::{self, Paths};
use crate::error::{Context, Error, Result};
use log::info;

pub(crate) fn init(paths: &Paths, force: bool) -> Result<()> {
    if paths.config != paths.user_config {
        return Err(Error::with_hint(
            format!(
                "hangar is configured by the Nix module ({})",
                paths.config.display()
            ),
            "change services.hangar instead, or set HANGAR_CONFIG for a \
             separate config",
        ));
    }
    let path = &paths.user_config;
    if path.exists() && !force {
        return Err(Error::with_hint(
            format!("{} exists", path.display()),
            "use 'hangar init --force' to overwrite",
        ));
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).context(dir.display())?;
    }
    fs::write(path, config::INITIAL_CONFIG).context(path.display())?;
    info!("wrote {}: run 'hangar up'", path.display());
    Ok(())
}
