//! `mounts`: host directories mounted into a bay when it's created. The
//! rules keep writable mounts away from host config and every bay away
//! from hangar's own folders but its own home and cache.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::bay::{VM_CACHE, VM_HOME};
use crate::error::{Context, Error, Result};
use crate::files::{host_path, is_credentials_file};

const HINT: &str = "see docs/configuration.md, \"Mounting folders\"";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Ro,
    Rw,
    Copy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MountSpec {
    pub(crate) host: String,
    pub(crate) mode: Mode,
}

/// A checked mount: its source resolved, so symlinks can't hide where it
/// points; `file` for a single-file mount.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub(crate) vm: String,
    pub(crate) host: PathBuf,
    pub(crate) writable: bool,
    pub(crate) file: bool,
}

/// The mounts the VM is created with: `ro` and `rw`.
pub(crate) fn live(
    mounts: &BTreeMap<String, MountSpec>,
) -> BTreeMap<String, MountSpec> {
    mounts
        .iter()
        .filter(|(_, spec)| spec.mode != Mode::Copy)
        .map(|(vm, spec)| (vm.clone(), spec.clone()))
        .collect()
}

/// The `copy` mounts: VM path -> host path as configured.
pub(crate) fn copies(
    mounts: &BTreeMap<String, MountSpec>,
) -> BTreeMap<String, String> {
    mounts
        .iter()
        .filter(|(_, spec)| spec.mode == Mode::Copy)
        .map(|(vm, spec)| (vm.clone(), spec.host.clone()))
        .collect()
}

fn overlaps(a: &str, b: &str) -> bool {
    Path::new(a).starts_with(b) || Path::new(b).starts_with(a)
}

/// The mount a VM path collides with, if any: a VM path is either copied
/// or mounted, never both.
pub(crate) fn mount_at<'a>(
    vm: &str,
    mounts: &'a BTreeMap<String, MountSpec>,
) -> Option<&'a str> {
    mounts
        .keys()
        .map(String::as_str)
        .find(|mount| overlaps(vm, mount))
}

/// `mounts` plus hangar's own mounts (`home` at the VM home,
/// `cache` at the package cache), when on: what the bay is created
/// with.
pub(crate) fn with_hangar(
    mounts: &BTreeMap<String, MountSpec>,
    home: Option<&String>,
    cache: Option<&String>,
) -> BTreeMap<String, MountSpec> {
    let mut all = mounts.clone();
    for (vm, host) in [(VM_HOME, home), (VM_CACHE, cache)] {
        if let Some(host) = host {
            let spec = MountSpec {
                host: host.clone(),
                mode: Mode::Rw,
            };
            all.insert(vm.to_string(), spec);
        }
    }
    all
}

/// `mounts` targets don't nest or cover the home mount.
pub(crate) fn check_targets(
    mounts: &BTreeMap<String, MountSpec>,
) -> Result<()> {
    if let Some(vm) =
        mounts.keys().find(|vm| Path::new(VM_HOME).starts_with(vm))
    {
        return Err(Error::with_hint(
            format!(
                "mounts.{vm} covers {VM_HOME}, where the bay's home is mounted"
            ),
            format!("mount it at a path below {VM_HOME}"),
        ));
    }
    let targets: Vec<&String> = mounts.keys().collect();
    for (i, a) in targets.iter().enumerate() {
        if let Some(b) = targets[i + 1..].iter().find(|b| overlaps(a, b)) {
            return Err(Error::with_hint(
                format!("mounts: {a} and {b} overlap"),
                HINT,
            ));
        }
    }
    Ok(())
}

/// What the rules check a source against, all resolved: the user's home,
/// hangar's data and cache dirs, and this bay's own home and cache.
#[derive(Debug)]
pub(crate) struct Roots {
    pub(crate) home: PathBuf,
    pub(crate) state: PathBuf,
    pub(crate) cache: PathBuf,
    pub(crate) bay_home: PathBuf,
    pub(crate) bay_cache: PathBuf,
}

impl Roots {
    /// Resolves every path, but of the bay's own folders only their
    /// parents: a folder that is itself a symlink then points elsewhere and
    /// loses the exception.
    pub(crate) fn new(
        home: &Path,
        state: &Path,
        cache: &Path,
        bay_home: &Path,
        bay_cache: &Path,
    ) -> Result<Self> {
        let folder = |path: &Path| -> Result<PathBuf> {
            let (Some(parent), Some(name)) = (path.parent(), path.file_name())
            else {
                return Err(Error::new(format!(
                    "{}: not a folder",
                    path.display()
                )));
            };
            Ok(would_be(parent).context(parent.display())?.join(name))
        };
        Ok(Self {
            home: fs::canonicalize(home).context(home.display())?,
            state: fs::canonicalize(state).context(state.display())?,
            cache: would_be(cache).context(cache.display())?,
            bay_home: folder(bay_home)?,
            bay_cache: folder(bay_cache)?,
        })
    }
}

/// Every mount, checked against the rules; the first refusal stops `up`
/// before anything is created. A missing source is checked on the path it
/// would have, then created (0700).
pub(crate) fn resolve(
    mounts: &BTreeMap<String, MountSpec>,
    roots: &Roots,
) -> Result<Vec<Resolved>> {
    let checked = live(mounts)
        .iter()
        .map(|(vm, spec)| check(vm, spec, roots))
        .collect::<Result<Vec<_>>>()?;
    checked
        .into_iter()
        .map(|(resolved, missing)| {
            if missing {
                create(&resolved.host)
                    .context(format!("creating {}", resolved.host.display()))?;
                log::info!("created {} for a mount", resolved.host.display());
            }
            Ok(resolved)
        })
        .collect()
}

fn check(
    vm: &str,
    spec: &MountSpec,
    roots: &Roots,
) -> Result<(Resolved, bool)> {
    let context = format!("mounts.{vm}");
    let configured = host_path(&spec.host, &roots.home).context(&context)?;
    let missing = fs::symlink_metadata(&configured).is_err();
    let host = if missing {
        would_be(&configured)
            .context(format!("{context}: {}", configured.display()))?
    } else {
        fs::canonicalize(&configured)
            .context(format!("{context}: {}", configured.display()))?
    };
    let file = host.is_file();
    if !missing && !file && !host.is_dir() {
        return Err(refuse(
            &context,
            &host,
            "isn't a regular file or directory",
        ));
    }
    let writable = spec.mode == Mode::Rw;
    check_source(&host, writable, roots)
        .map_err(|why| refuse(&context, &host, why))?;
    // A stow link's own name counts too: `~/.bashrc` is host config
    // wherever it points. Hangar's own folders are exempt, as in
    // `check_source`.
    let own =
        host.starts_with(&roots.bay_home) || host.starts_with(&roots.bay_cache);
    if !own {
        check_source(&configured, writable, roots)
            .map_err(|why| refuse(&context, &configured, why))?;
    }
    if file && is_credentials_file(Path::new(vm)) {
        return Err(refuse(
            &context,
            Path::new(vm),
            "holds credentials or keys",
        ));
    }
    let resolved = Resolved {
        vm: vm.to_string(),
        host,
        writable: spec.mode == Mode::Rw,
        file,
    };
    Ok((resolved, missing))
}

/// Where a missing path would end up: its nearest existing ancestor with
/// symlinks resolved, plus the missing components.
pub(crate) fn would_be(path: &Path) -> Result<PathBuf> {
    use std::path::Component;
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(Error::new("can't contain .."));
    }
    let existing = path
        .ancestors()
        .find(|ancestor| fs::symlink_metadata(ancestor).is_ok())
        .context("no existing ancestor")?;
    let host = fs::canonicalize(existing)
        .context(format!("resolving {}", existing.display()))?;
    if !host.is_dir() {
        return Err(Error::new(format!(
            "{} isn't a directory",
            host.display()
        )));
    }
    match path.strip_prefix(existing) {
        // `join("")` would add a trailing `/` to the recorded mount.
        Ok(rest) if !rest.as_os_str().is_empty() => Ok(host.join(rest)),
        _ => Ok(host),
    }
}

/// Creates the missing components of a checked path, each 0700; existing
/// ancestors are left as they are.
pub(crate) fn create(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

/// Why a resolved source can't be used, if it can't, for what only hangar
/// may touch: the two roots and other bays' folders. Copies check this
/// too (`files::plan`).
pub(crate) fn check_hangar_dirs(
    host: &Path,
    roots: &Roots,
) -> std::result::Result<(), &'static str> {
    let Roots {
        state,
        cache,
        bay_home,
        bay_cache,
        ..
    } = roots;
    if state.starts_with(host)
        || (host.starts_with(state) && !host.starts_with(bay_home))
    {
        return Err("holds hangar's own state");
    }
    if cache.starts_with(host)
        || (host.starts_with(cache) && !host.starts_with(bay_cache))
    {
        return Err("holds hangar's package caches");
    }
    Ok(())
}

fn check_source(
    host: &Path,
    writable: bool,
    roots: &Roots,
) -> std::result::Result<(), &'static str> {
    let home = &roots.home;
    if home.starts_with(host) {
        return Err("is your home or above it");
    }
    check_hangar_dirs(host, roots)?;
    if is_credentials_file(host) {
        return Err("holds credentials or keys");
    }
    let own =
        host.starts_with(&roots.bay_home) || host.starts_with(&roots.bay_cache);
    if !writable || own {
        return Ok(());
    }
    let Ok(inside) = host.strip_prefix(home) else {
        return Err("is outside your home; writable mounts must be in it");
    };
    let first = inside
        .components()
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .unwrap_or_default();
    if first.starts_with('.') || first == "Library" {
        return Err("is where host programs keep config; a writable \
                    mount there lets an agent change what they run");
    }
    Ok(())
}

fn refuse(context: &str, host: &Path, why: &str) -> Error {
    Error::with_hint(format!("{context}: {} {why}", host.display()), HINT)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountKind {
    User,
    Home,
    Cache,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MountStatus {
    pub(crate) vm: String,
    pub(crate) host: String,
    pub(crate) writable: bool,
    pub(crate) kind: MountKind,
    /// The bay was created with it; `None` when that's unknown.
    pub(crate) applied: Option<bool>,
}

fn expanded(spec: &MountSpec, home: &Path) -> String {
    host_path(&spec.host, home)
        .map_or_else(|_| spec.host.clone(), |p| p.display().to_string())
}

/// Where an expanded host path resolves to, as the record has it; the
/// path itself when it can't be resolved.
fn resolved_host(host: &str) -> String {
    let path = Path::new(host);
    fs::canonicalize(path)
        .ok()
        .or_else(|| would_be(path).ok())
        .map_or_else(|| host.to_string(), |p| p.display().to_string())
}

pub(crate) fn line(vm: &str, host: &str, writable: bool) -> String {
    record_line(vm, host, writable, false)
}

fn record_line(vm: &str, host: &str, writable: bool, file: bool) -> String {
    let mode = if writable { "rw" } else { "ro" };
    let kind = if file { "file-" } else { "" };
    format!("{vm}\t{host}\t{kind}{mode}")
}

/// What the bay is created with, one line per mount with its resolved
/// host: recorded and compared on later `up`s, since mounts only change
/// with a new VM.
pub(crate) fn lines(resolved: &[Resolved]) -> Vec<String> {
    resolved
        .iter()
        .map(|mount| {
            let host = mount.host.display().to_string();
            record_line(&mount.vm, &host, mount.writable, mount.file)
        })
        .collect()
}

/// Each configured mount (hangar's own included), and whether the VM has
/// it: unknown without a `recorded` list.
pub(crate) fn status(
    mounts: &BTreeMap<String, MountSpec>,
    bay_home: Option<&String>,
    bay_cache: Option<&String>,
    home: &Path,
    recorded: Option<&[String]>,
) -> Vec<MountStatus> {
    live(&with_hangar(mounts, bay_home, bay_cache))
        .iter()
        .map(|(vm, spec)| {
            let host = expanded(spec, home);
            let host_now = resolved_host(&host);
            let wanted = record_line(
                vm,
                &host_now,
                spec.mode == Mode::Rw,
                Path::new(&host_now).is_file(),
            );
            let applied = recorded.map(|lines| lines.contains(&wanted));
            MountStatus {
                vm: vm.clone(),
                host,
                writable: spec.mode == Mode::Rw,
                kind: if bay_home.is_some() && vm == VM_HOME {
                    MountKind::Home
                } else if bay_cache.is_some() && vm == VM_CACHE {
                    MountKind::Cache
                } else {
                    MountKind::User
                },
                applied,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        Mode, MountKind, MountSpec, MountStatus, check_source, check_targets,
        lines, overlaps, status, with_hangar,
    };
    use super::{Resolved, Roots, resolve};
    use crate::testing::{roots, scratch_dir};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    const HOME: &str = "/home/you";
    const STATE: &str = "/home/you/.local/share/hangar";
    const CACHE: &str = "/home/you/.cache/hangar";

    /// The roots as bay `default` sees them, with `state` and `cache`.
    fn bay_roots(state: &str, cache: &str) -> Roots {
        Roots {
            home: HOME.into(),
            state: state.into(),
            cache: cache.into(),
            bay_home: Path::new(state).join("bays/default/home"),
            bay_cache: Path::new(cache).join("bays/default"),
        }
    }

    fn source(host: &str, writable: bool) -> Result<(), &'static str> {
        check_source(Path::new(host), writable, &bay_roots(STATE, CACHE))
    }

    fn mounts(entries: &[(&str, &str, bool)]) -> BTreeMap<String, MountSpec> {
        entries
            .iter()
            .map(|(vm, host, writable)| {
                (
                    (*vm).to_string(),
                    MountSpec {
                        host: (*host).to_string(),
                        mode: if *writable { Mode::Rw } else { Mode::Ro },
                    },
                )
            })
            .collect()
    }

    #[test]
    fn sources_follow_the_rules_for_read_only_and_writable_mounts() {
        for (host, writable) in [
            ("/home/you/work/shared", true),
            ("/home/you/work/shared", false),
            ("/home/you/.claude/skills", false),
            ("/srv/shared", false),
        ] {
            assert_eq!(source(host, writable), Ok(()), "{host} {writable}");
        }
        for (host, writable, why) in [
            ("/", false, "your home or above"),
            ("/home", false, "your home or above"),
            ("/home/you", false, "your home or above"),
            ("/home/you/.local/share/hangar", false, "hangar's own state"),
            (
                "/home/you/.local/share/hangar/proxies/p",
                true,
                "hangar's own state",
            ),
            ("/home/you/.ssh", false, "credentials or keys"),
            ("/home/you/work/.aws", false, "credentials or keys"),
            ("/home/you/.config/gh", false, "credentials or keys"),
            ("/home/you/.secrets", false, "credentials or keys"),
            ("/home/you/.claude", true, "keep config"),
            ("/home/you/.claude/agents", true, "keep config"),
            ("/home/you/.config", true, "keep config"),
            ("/home/you/Library/Application Support", true, "keep config"),
            ("/srv/shared", true, "outside your home"),
        ] {
            let error = source(host, writable).unwrap_err();
            assert!(error.contains(why), "{host} {writable}: {error}");
        }
    }

    #[test]
    fn targets_never_overlap_each_other_or_copied_files() {
        assert!(overlaps(
            "/home/pilot/.paperclip",
            "/home/pilot/.paperclip/db"
        ));
        assert!(overlaps("/home/pilot/a", "/home/pilot/a"));
        assert!(!overlaps("/home/pilot/a", "/home/pilot/ab"));
        let two = mounts(&[
            ("/home/pilot/a", "~/a", false),
            ("/home/pilot/b", "~/b", true),
        ]);
        assert!(check_targets(&two).is_ok());
        let nested = mounts(&[
            ("/home/pilot/a", "~/a", false),
            ("/home/pilot/a/b", "~/b", false),
        ]);
        let error = check_targets(&nested).unwrap_err().to_string();
        assert!(error.contains("overlap"), "{error}");
    }

    #[test]
    fn resolve_follows_symlinks_and_needs_directories() {
        let home = fs::canonicalize(scratch_dir("mount-resolve")).unwrap();
        let roots = roots(&home, "state", ".cache/hangar");
        fs::create_dir_all(home.join("data")).unwrap();
        fs::create_dir_all(home.join(".config")).unwrap();
        fs::write(home.join("file"), "x").unwrap();
        std::os::unix::fs::symlink(home.join("data"), home.join("link"))
            .unwrap();
        std::os::unix::fs::symlink(home.join(".config"), home.join("cfg"))
            .unwrap();
        let resolved =
            resolve(&mounts(&[("/home/pilot/d", "~/link", true)]), &roots)
                .unwrap();
        assert_eq!(
            resolved,
            [Resolved {
                vm: "/home/pilot/d".into(),
                host: home.join("data"),
                writable: true,
                file: false,
            }]
        );
        for (host, why) in [
            ("~/file/below", "isn't a directory"),
            ("~/missing/../x", "can't contain .."),
            ("relative", "expected an absolute path"),
            // A symlink is judged by where it points.
            ("~/cfg", "keep config"),
        ] {
            let error =
                resolve(&mounts(&[("/home/pilot/d", host, true)]), &roots)
                    .unwrap_err()
                    .to_string();
            assert!(error.contains(why), "{host}: {error}");
        }
    }

    fn spec(host: &str, mode: Mode) -> MountSpec {
        MountSpec {
            host: host.into(),
            mode,
        }
    }

    #[test]
    fn stow_symlinked_dir_mounts_its_real_target() {
        let home = fs::canonicalize(scratch_dir("mount-stow")).unwrap();
        let roots = roots(&home, "state", ".cache/hangar");
        fs::create_dir_all(home.join("dotfiles/code")).unwrap();
        std::os::unix::fs::symlink(
            home.join("dotfiles/code"),
            home.join("code"),
        )
        .unwrap();
        let set = BTreeMap::from([(
            "/home/pilot/code".into(),
            spec("~/code", Mode::Ro),
        )]);
        let resolved = resolve(&set, &roots).unwrap();
        assert_eq!(resolved[0].host, home.join("dotfiles/code"));
    }

    #[test]
    fn symlink_into_state_is_refused_after_resolving() {
        let home = fs::canonicalize(scratch_dir("mount-link-state")).unwrap();
        let roots = roots(&home, "state", ".cache/hangar");
        std::os::unix::fs::symlink(home.join("state"), home.join("alias"))
            .unwrap();
        let set = BTreeMap::from([(
            "/home/pilot/a".into(),
            spec("~/alias", Mode::Ro),
        )]);
        let error = resolve(&set, &roots).unwrap_err().to_string();
        assert!(error.contains("hangar's own state"), "{error}");
    }

    #[test]
    fn file_mount_ro_records_file_ro() {
        let home = fs::canonicalize(scratch_dir("mount-file")).unwrap();
        let roots = roots(&home, "state", ".cache/hangar");
        fs::create_dir_all(home.join("dotfiles")).unwrap();
        fs::write(home.join("dotfiles/gitconfig"), "[user]").unwrap();
        std::os::unix::fs::symlink(
            home.join("dotfiles/gitconfig"),
            home.join(".gitconfig"),
        )
        .unwrap();
        let set = BTreeMap::from([(
            "/home/pilot/.gitconfig".into(),
            spec("~/.gitconfig", Mode::Ro),
        )]);
        let resolved = resolve(&set, &roots).unwrap();
        assert!(resolved[0].file);
        assert_eq!(resolved[0].host, home.join("dotfiles/gitconfig"));
        assert_eq!(
            lines(&resolved),
            [format!(
                "/home/pilot/.gitconfig\t{}\tfile-ro",
                home.join("dotfiles/gitconfig").display()
            )]
        );
        let recorded = lines(&resolved);
        let shown = status(&set, None, None, &home, Some(&recorded));
        assert_eq!(shown[0].applied, Some(true));
    }

    fn stowed(home: &Path, link_name: &str, target: &str, dir: bool) {
        let target = home.join(target);
        if dir {
            fs::create_dir_all(&target).unwrap();
        } else {
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(&target, "x").unwrap();
        }
        std::os::unix::fs::symlink(&target, home.join(link_name)).unwrap();
    }

    #[test]
    fn stowed_dot_paths_are_judged_by_their_name_when_writable() {
        let home = fs::canonicalize(scratch_dir("mount-stow-dot")).unwrap();
        let roots = roots(&home, "state", ".cache/hangar");
        stowed(&home, ".bashrc", "dotfiles/bash/.bashrc", false);
        stowed(&home, ".config", "dotfiles/config", true);
        for host in ["~/.bashrc", "~/.config"] {
            let rw = BTreeMap::from([(
                "/home/pilot/x".into(),
                spec(host, Mode::Rw),
            )]);
            let error = resolve(&rw, &roots).unwrap_err().to_string();
            assert!(error.contains("keep config"), "{host}: {error}");
            let ro = BTreeMap::from([(
                "/home/pilot/x".into(),
                spec(host, Mode::Ro),
            )]);
            assert!(resolve(&ro, &roots).is_ok(), "{host}");
        }
    }

    #[test]
    fn stowed_credentials_are_refused_by_their_link_name() {
        let home = fs::canonicalize(scratch_dir("mount-stow-cred")).unwrap();
        let roots = roots(&home, "state", ".cache/hangar");
        stowed(&home, ".git-credentials", "dotfiles/git-creds", false);
        let set = BTreeMap::from([(
            "/home/pilot/x".into(),
            spec("~/.git-credentials", Mode::Ro),
        )]);
        let error = resolve(&set, &roots).unwrap_err().to_string();
        assert!(error.contains("credentials"), "{error}");
        stowed(&home, "innocent", "dotfiles/plain", false);
        let set = BTreeMap::from([(
            "/home/pilot/.netrc".into(),
            spec("~/innocent", Mode::Ro),
        )]);
        let error = resolve(&set, &roots).unwrap_err().to_string();
        assert!(error.contains("credentials"), "{error}");
    }

    #[test]
    fn sockets_are_neither_files_nor_directories() {
        let home = fs::canonicalize(scratch_dir("mount-socket")).unwrap();
        let roots = roots(&home, "state", ".cache/hangar");
        let _listener =
            std::os::unix::net::UnixListener::bind(home.join("sock")).unwrap();
        let set = BTreeMap::from([(
            "/home/pilot/s".into(),
            spec("~/sock", Mode::Ro),
        )]);
        let error = resolve(&set, &roots).unwrap_err().to_string();
        assert!(
            error.contains("isn't a regular file or directory"),
            "{error}"
        );
    }

    #[test]
    fn credentials_file_mount_is_refused() {
        let home = fs::canonicalize(scratch_dir("mount-file-key")).unwrap();
        let roots = roots(&home, "state", ".cache/hangar");
        fs::create_dir_all(home.join("keys")).unwrap();
        fs::write(home.join("keys/id_ed25519"), "key").unwrap();
        let set = BTreeMap::from([(
            "/home/pilot/k".into(),
            spec("~/keys/id_ed25519", Mode::Ro),
        )]);
        let error = resolve(&set, &roots).unwrap_err().to_string();
        assert!(error.contains("credentials"), "{error}");
    }

    #[test]
    fn writable_file_mount_records_file_rw() {
        let home = fs::canonicalize(scratch_dir("mount-file-rw")).unwrap();
        let roots = roots(&home, "state", ".cache/hangar");
        fs::write(home.join("notes"), "x").unwrap();
        let set = BTreeMap::from([(
            "/home/pilot/n".into(),
            spec("~/notes", Mode::Rw),
        )]);
        let resolved = resolve(&set, &roots).unwrap();
        assert!(lines(&resolved)[0].ends_with("\tfile-rw"));
    }

    #[test]
    fn live_and_copies_split_the_modes() {
        let set = BTreeMap::from([
            ("/home/pilot/a".to_string(), spec("~/a", Mode::Ro)),
            ("/home/pilot/b".to_string(), spec("~/b", Mode::Rw)),
            ("/home/pilot/c".to_string(), spec("~/c", Mode::Copy)),
        ]);
        let live: Vec<_> = super::live(&set).into_keys().collect();
        assert_eq!(live, ["/home/pilot/a", "/home/pilot/b"]);
        assert_eq!(
            super::copies(&set),
            BTreeMap::from([("/home/pilot/c".to_string(), "~/c".to_string())])
        );
    }

    #[test]
    fn copy_and_live_mount_cannot_overlap() {
        let set = BTreeMap::from([
            ("/home/pilot/a".to_string(), spec("~/a", Mode::Ro)),
            ("/home/pilot/a/b".to_string(), spec("~/b", Mode::Copy)),
        ]);
        let error = check_targets(&set).unwrap_err().to_string();
        assert!(error.contains("overlap"), "{error}");
        assert_eq!(
            super::mount_at("/home/pilot/a/x", &super::live(&set)),
            Some("/home/pilot/a")
        );
    }

    #[test]
    fn missing_sources_are_checked_then_created_0700() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = fs::canonicalize(scratch_dir("mount-create")).unwrap();
        let roots = roots(&home, ".local/share/hangar", ".cache/hangar");
        fs::create_dir_all(home.join("work")).unwrap();
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::set_permissions(
            home.join("work"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink(home.join(".ssh"), home.join("keys"))
            .unwrap();
        let mode = |p: &str| {
            fs::metadata(home.join(p)).unwrap().permissions().mode() & 0o777
        };

        let resolved = resolve(
            &mounts(&[(
                "/home/pilot/.paperclip",
                "~/work/paperclip/data",
                true,
            )]),
            &roots,
        )
        .unwrap();
        assert_eq!(resolved[0].host, home.join("work/paperclip/data"));
        assert_eq!(mode("work/paperclip"), 0o700);
        assert_eq!(mode("work/paperclip/data"), 0o700);
        // An existing ancestor keeps its own mode.
        assert_eq!(mode("work"), 0o755);

        for (host, writable, why) in [
            // Through a symlink, the would-be path lands in ~/.ssh.
            ("~/keys/new", false, "credentials or keys"),
            ("~/.config/new", true, "keep config"),
            ("~/.local/share/hangar/new", false, "hangar's own state"),
            (
                "~/.local/share/hangar/bays/x/home",
                false,
                "hangar's own state",
            ),
        ] {
            let error =
                resolve(&mounts(&[("/home/pilot/x", host, writable)]), &roots)
                    .unwrap_err()
                    .to_string();
            assert!(error.contains(why), "{host}: {error}");
        }
        assert!(!home.join(".ssh/new").exists());
        assert!(!home.join(".config/new").exists());
        assert!(!home.join(".config").exists());

        // One refusal stops all: the allowed missing source isn't created.
        let error = resolve(
            &mounts(&[
                ("/home/pilot/a", "~/fine", true),
                ("/home/pilot/b", "~/.config/nope", true),
            ]),
            &roots,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("keep config"), "{error}");
        assert!(!home.join("fine").exists());
    }

    #[test]
    fn the_record_holds_the_resolved_host_and_the_mode() {
        let resolved = [
            Resolved {
                vm: "/home/pilot/.paperclip".into(),
                host: "/home/you/work/paperclip".into(),
                writable: true,
                file: false,
            },
            Resolved {
                vm: "/home/pilot/skills".into(),
                host: "/srv/skills".into(),
                writable: false,
                file: false,
            },
        ];
        assert_eq!(
            lines(&resolved),
            [
                "/home/pilot/.paperclip\t/home/you/work/paperclip\trw",
                "/home/pilot/skills\t/srv/skills\tro",
            ]
        );
        assert_eq!(lines(&[]), Vec::<String>::new());
    }

    #[test]
    fn status_marks_what_the_vm_was_created_with() {
        let home = fs::canonicalize(scratch_dir("mount-status")).unwrap();
        fs::create_dir_all(home.join("data")).unwrap();
        std::os::unix::fs::symlink(home.join("data"), home.join("a")).unwrap();
        let set = mounts(&[
            ("/home/pilot/a", "~/a", true),
            ("/home/pilot/b", "~/b", false),
        ]);
        // The record has where `~/a` resolved to, not the link.
        let recorded = [
            format!("/home/pilot/a\t{}\trw", home.join("data").display()),
            format!("/home/pilot/b\t{}\trw", home.join("b").display()),
        ];
        let shown = |recorded| status(&set, None, None, &home, recorded);
        assert_eq!(
            shown(Some(&recorded)),
            [
                MountStatus {
                    vm: "/home/pilot/a".into(),
                    host: home.join("a").display().to_string(),
                    writable: true,
                    kind: MountKind::User,
                    applied: Some(true),
                },
                // Created writable, now configured read-only: not applied.
                MountStatus {
                    vm: "/home/pilot/b".into(),
                    host: home.join("b").display().to_string(),
                    writable: false,
                    kind: MountKind::User,
                    applied: Some(false),
                },
            ]
        );
        assert!(shown(None).iter().all(|m| m.applied.is_none()));
    }

    #[test]
    fn bay_home_mounts_the_vm_home_writable_next_to_user_mounts() {
        let set =
            mounts(&[("/home/pilot/.cache/skills", "/srv/skills", false)]);
        let home = format!("{STATE}/home");
        let all = with_hangar(&set, Some(&home), None);
        assert_eq!(
            all["/home/pilot"],
            MountSpec {
                host: home.clone(),
                mode: Mode::Rw,
            }
        );
        assert_eq!(all.len(), 2);
        assert_eq!(with_hangar(&set, None, None), set);
        let shown = status(&set, Some(&home), None, Path::new(HOME), None);
        assert!(
            shown
                .iter()
                .any(|m| m.vm == "/home/pilot" && m.kind == MountKind::Home)
        );
        assert!(
            shown
                .iter()
                .any(|m| m.vm != "/home/pilot" && m.kind == MountKind::User)
        );
    }

    #[test]
    fn mounts_may_sit_under_the_home_mount_but_not_cover_it() {
        let below =
            mounts(&[("/home/pilot/.claude/skills", "~/skills", false)]);
        assert!(check_targets(&below).is_ok());
        for vm in ["/home/pilot", "/home", "/"] {
            let error =
                check_targets(&mounts(&[(vm, "~/x", true)])).unwrap_err();
            assert!(
                error.to_string().contains("bay's home is mounted"),
                "{vm}: {error}"
            );
            assert_eq!(
                error.hint(),
                Some("mount it at a path below /home/pilot")
            );
        }
    }

    #[test]
    fn only_the_bays_own_folders_are_allowed_in_hangars_roots() {
        for host in [
            "/home/you/.local/share/hangar/bays/default/home",
            "/home/you/.local/share/hangar/bays/default/home/.claude",
            "/home/you/.cache/hangar/bays/default",
            "/home/you/.cache/hangar/bays/default/nix",
        ] {
            assert_eq!(source(host, true), Ok(()), "{host}");
        }
        for writable in [true, false] {
            for (host, why) in [
                ("/home/you/.cache", "package caches"),
                ("/home/you/.cache/hangar", "package caches"),
                ("/home/you/.cache/hangar/bays", "package caches"),
                ("/home/you/.cache/hangar/bays/other", "package caches"),
                ("/home/you/.cache/hangar/bays/default-2", "package caches"),
                ("/home/you/.local/share", "hangar's own state"),
                ("/home/you/.local/share/hangar", "hangar's own state"),
                (
                    "/home/you/.local/share/hangar/proxies/p",
                    "hangar's own state",
                ),
                ("/home/you/.local/share/hangar/guest", "hangar's own state"),
                ("/home/you/.local/share/hangar/bays", "hangar's own state"),
                (
                    "/home/you/.local/share/hangar/bays/default/vm",
                    "hangar's own state",
                ),
                (
                    "/home/you/.local/share/hangar/bays/other/home",
                    "hangar's own state",
                ),
                ("/home/you/.cache/hangar/bays/default/.ssh", "credentials"),
            ] {
                let error = source(host, writable).unwrap_err();
                assert!(error.contains(why), "{host} {writable}: {error}");
            }
        }
        for (host, why) in [
            ("/home/you/.cache/other", "keep config"),
            ("/home/you/.cache/hangar-other", "keep config"),
            ("/home/you/.local/share/hangar-home", "keep config"),
        ] {
            let error = source(host, true).unwrap_err();
            assert!(error.contains(why), "{host}: {error}");
        }
    }

    #[test]
    fn overlapping_roots_fail_safe() {
        let roots = bay_roots(
            "/home/you/.cache/hangar/state",
            "/home/you/.cache/hangar",
        );
        let error = check_source(
            Path::new("/home/you/.cache/hangar/state/bays/default/home"),
            true,
            &roots,
        )
        .unwrap_err();
        assert!(error.contains("package caches"), "{error}");
    }

    #[test]
    fn a_cache_outside_home_is_allowed_only_as_the_bays_own() {
        let roots = bay_roots(STATE, "/var/tmp/xdg-cache/hangar");
        let check = |host: &str| check_source(Path::new(host), true, &roots);
        assert_eq!(check("/var/tmp/xdg-cache/hangar/bays/default"), Ok(()));
        let error = check("/var/tmp/xdg-cache/other").unwrap_err();
        assert!(error.contains("outside your home"), "{error}");
        let error = check("/var/tmp/xdg-cache/hangar").unwrap_err();
        assert!(error.contains("package caches"), "{error}");
    }

    #[test]
    fn a_symlinked_default_folder_loses_the_exception() {
        let home = fs::canonicalize(scratch_dir("mount-own-link")).unwrap();
        let roots = roots(&home, ".local/share/hangar", ".cache/hangar");
        let own = home.join(".cache/hangar/bays/default");
        fs::create_dir_all(home.join(".config")).unwrap();
        fs::create_dir_all(own.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(home.join(".config"), &own).unwrap();
        assert_eq!(roots.bay_cache, own);
        let cache = "~/.cache/hangar/bays/default";
        let error =
            resolve(&mounts(&[("/var/cache/hangar", cache, true)]), &roots)
                .unwrap_err()
                .to_string();
        assert!(error.contains("keep config"), "{error}");

        fs::remove_file(&own).unwrap();
        let resolved =
            resolve(&mounts(&[("/var/cache/hangar", cache, true)]), &roots)
                .unwrap();
        assert_eq!(resolved[0].host, own);
        assert!(own.is_dir());
        let bay_home: PathBuf =
            home.join(".local/share/hangar/bays/default/home");
        let resolved = resolve(
            &mounts(&[("/home/pilot", bay_home.to_str().unwrap(), true)]),
            &roots,
        )
        .unwrap();
        assert_eq!(resolved[0].host, bay_home);
    }

    #[test]
    fn bay_cache_mounts_the_package_cache_writable() {
        let cache = "~/.cache/hangar".to_string();
        let all = with_hangar(&BTreeMap::new(), None, Some(&cache));
        assert_eq!(
            all["/var/cache/hangar"],
            MountSpec {
                host: cache.clone(),
                mode: Mode::Rw,
            }
        );
        let shown =
            status(&BTreeMap::new(), None, Some(&cache), Path::new(HOME), None);
        assert!(shown.iter().all(|m| m.kind == MountKind::Cache));
    }
}
