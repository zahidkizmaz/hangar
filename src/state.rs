//! The files hangar keeps in `stateDir`, the layout docs/architecture.md
//! describes under "State". The broker backend names its own files.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions, Permissions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::error::{Context, Result};

pub(crate) struct StateDir(PathBuf);

impl StateDir {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self(root)
    }

    pub(crate) fn root(&self) -> &Path {
        &self.0
    }

    /// The credentials hangar set, one per line.
    pub(crate) fn credential_keys(&self) -> PathBuf {
        self.0.join("credential-keys")
    }

    /// What the tower VM was created with (`vm_record`, ports only).
    pub(crate) fn tower_vm(&self) -> PathBuf {
        self.0.join("tower-vm")
    }

    pub(crate) fn bays(&self) -> PathBuf {
        self.0.join("bays")
    }

    pub(crate) fn bay(&self, name: &str) -> BayDir {
        BayDir(self.bays().join(name))
    }

    /// Mounted read-only at `/run/hangar` in every bay.
    pub(crate) fn guest(&self) -> PathBuf {
        self.0.join("guest")
    }

    pub(crate) fn ca(&self) -> PathBuf {
        self.guest().join("ca.pem")
    }
}

pub(crate) struct BayDir(PathBuf);

impl BayDir {
    /// What the bay's VM was created with (`vm_record`).
    pub(crate) fn vm(&self) -> PathBuf {
        self.0.join("vm")
    }

    pub(crate) fn files(&self) -> PathBuf {
        self.0.join("files")
    }

    pub(crate) fn run_fingerprints(&self) -> PathBuf {
        self.0.join("run-fingerprints")
    }

    /// The bay's home, mounted at pilot's home, `/home/pilot` (`home`).
    pub(crate) fn home(&self) -> PathBuf {
        self.0.join("home")
    }
}

/// `hash\tname` lines as name -> hash; a missing file is empty.
pub(crate) fn read_hashes(path: &Path) -> BTreeMap<String, String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(hash, name)| (name.to_string(), hash.to_string()))
        .collect()
}

pub(crate) fn write_hashes(
    path: &Path,
    hashes: &BTreeMap<String, String>,
) -> Result<()> {
    let text = hashes.iter().fold(String::new(), |mut text, (name, hash)| {
        let _ = writeln!(text, "{hash}\t{name}");
        text
    });
    write_private(path, text.as_bytes())
}

/// FNV-1a: enough to see that an input changed (not for security).
pub(crate) fn fnv(bytes: impl IntoIterator<Item = u8>) -> String {
    let hash = bytes
        .into_iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
    format!("{hash:016x}")
}

/// Writes a file only its owner can read: tokens, passwords and records,
/// creating its folder first. `mode` only applies on create, so an
/// existing file is narrowed before the contents go in.
pub(crate) fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).context(dir.display())?;
    }
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| {
            file.set_permissions(Permissions::from_mode(0o600))?;
            file.write_all(contents)
        })
        .context(path.display())
}

#[cfg(test)]
mod tests {
    use super::write_private;
    use crate::testing::scratch_dir;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn private_files_are_owner_only() {
        let file = scratch_dir("private").join("token");
        write_private(&file, b"token").unwrap();
        let mode = fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(fs::read(&file).unwrap(), b"token");

        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&file, b"new").unwrap();
        let mode = fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(fs::read(&file).unwrap(), b"new");
    }
}
