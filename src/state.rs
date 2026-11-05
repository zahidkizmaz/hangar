//! The files hangar keeps in its data dir (`$XDG_DATA_HOME/hangar`): one
//! folder per bay and per proxy.

use std::collections::BTreeMap;
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

    pub(crate) fn bays(&self) -> PathBuf {
        self.0.join("bays")
    }

    pub(crate) fn bay(&self, name: &str) -> BayDir {
        BayDir(self.bays().join(name))
    }

    pub(crate) fn proxies(&self) -> PathBuf {
        self.0.join("proxies")
    }

    pub(crate) fn proxy(&self, name: &str) -> ProxyDir {
        ProxyDir(self.proxies().join(name))
    }
}

pub(crate) struct BayDir(PathBuf);

impl BayDir {
    /// What the bay's VM was created with (`vm_record`).
    pub(crate) fn vm(&self) -> PathBuf {
        self.0.join("vm")
    }

    pub(crate) fn copies(&self) -> PathBuf {
        self.0.join("copies")
    }

    /// The bay's home, mounted at pilot's home, `/home/pilot` (`home`).
    pub(crate) fn home(&self) -> PathBuf {
        self.0.join("home")
    }
}

pub(crate) struct ProxyDir(PathBuf);

impl ProxyDir {
    /// What a managed proxy's VM was created with (`vm_record`).
    pub(crate) fn vm(&self) -> PathBuf {
        self.0.join("vm")
    }

    /// The proxy's own files; a managed proxy mounts it.
    pub(crate) fn data(&self) -> PathBuf {
        self.0.join("data")
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
    let text: String = hashes
        .iter()
        .flat_map(|(name, hash)| [hash, "\t", name, "\n"])
        .collect();
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

/// Writes a file only its owner can read, creating its folder first. `mode` only applies on create, so an
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
