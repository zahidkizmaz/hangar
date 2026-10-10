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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MountSpec {
    pub(crate) host: String,
    pub(crate) writable: bool,
}

/// A checked mount: its source resolved, so symlinks can't hide where it
/// points.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub(crate) vm: String,
    pub(crate) host: PathBuf,
    pub(crate) writable: bool,
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
                writable: true,
            };
            all.insert(vm.to_string(), spec);
        }
    }
    all
}

/// `mounts` targets don't nest or cover the home mount, and no `files`
/// target is inside, above or at one. Files may land in the home mount: they're copied into its host
/// folder.
pub(crate) fn check_targets<'a>(
    mounts: &BTreeMap<String, MountSpec>,
    home: bool,
    files: impl IntoIterator<Item = &'a String>,
) -> Result<()> {
    if let Some(vm) = mounts
        .keys()
        .find(|vm| home && Path::new(VM_HOME).starts_with(vm))
    {
        return Err(Error::with_hint(
            format!(
                "mounts.{vm} covers {VM_HOME}, where the bay's home is mounted"
            ),
            "set home = false to mount your own",
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
    for file in files {
        if let Some(mount) = mount_at(file, mounts) {
            return Err(Error::with_hint(
                format!("files.{file} overlaps the mount at {mount}"),
                "a VM path is either copied or mounted, not both",
            ));
        }
    }
    Ok(())
}

/// What the rules check a source against, all resolved: the user's home,
/// the two roots (`stateDir`, the package cache root) and this bay's own
/// home and cache.
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
    let checked = mounts
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
        let host = fs::canonicalize(&configured)
            .context(format!("{context}: {}", configured.display()))?;
        if !host.is_dir() {
            return Err(refuse(&context, &host, "isn't a directory"));
        }
        host
    };
    check_source(&host, spec.writable, roots)
        .map_err(|why| refuse(&context, &host, why))?;
    let resolved = Resolved {
        vm: vm.to_string(),
        host,
        writable: spec.writable,
    };
    Ok((resolved, missing))
}

/// Where a missing path would end up: its nearest existing ancestor with
/// symlinks resolved, plus the missing components.
fn would_be(path: &Path) -> Result<PathBuf> {
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
fn create(path: &Path) -> std::io::Result<()> {
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

/// An existing bay's recorded mounts, against today's rules: a root may
/// have moved since the VM was created.
pub(crate) fn recheck(lines: &[String], roots: &Roots) -> Result<()> {
    for line in lines {
        let mut fields = line.split('\t');
        let (Some(vm), Some(host), Some(mode)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let host = Path::new(host);
        check_source(host, mode == "rw", roots).map_err(|why| {
            refuse(&format!("the bay's mount at {vm}"), host, why)
        })?;
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

fn line(vm: &str, host: &str, writable: bool) -> String {
    let mode = if writable { "rw" } else { "ro" };
    format!("{vm}\t{host}\t{mode}")
}

/// What the bay is created with, one line per mount with its resolved
/// host: recorded and compared on later `up`s, since mounts only change
/// with a new VM.
pub(crate) fn lines(resolved: &[Resolved]) -> Vec<String> {
    resolved
        .iter()
        .map(|mount| {
            line(&mount.vm, &mount.host.display().to_string(), mount.writable)
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
    with_hangar(mounts, bay_home, bay_cache)
        .iter()
        .map(|(vm, spec)| {
            let host = expanded(spec, home);
            let wanted = line(vm, &resolved_host(&host), spec.writable);
            let applied = recorded.map(|lines| lines.contains(&wanted));
            MountStatus {
                vm: vm.clone(),
                host,
                writable: spec.writable,
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
        MountKind, MountSpec, MountStatus, check_source, check_targets, lines,
        overlaps, recheck, status, with_hangar,
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
                        writable: *writable,
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
                "/home/you/.local/share/hangar/vault",
                true,
                "hangar's own state",
            ),
            ("/home/you/.ssh", false, "credentials or keys"),
            ("/home/you/work/.aws", false, "credentials or keys"),
            ("/home/you/.config/gh", false, "credentials or keys"),
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
        let none: [String; 0] = [];
        assert!(check_targets(&two, false, &none).is_ok());
        let nested = mounts(&[
            ("/home/pilot/a", "~/a", false),
            ("/home/pilot/a/b", "~/b", false),
        ]);
        let error = check_targets(&nested, false, &none)
            .unwrap_err()
            .to_string();
        assert!(error.contains("overlap"), "{error}");
        for file in ["/home/pilot/a/x", "/home/pilot/a", "/home/pilot"] {
            let error = check_targets(&two, false, &[file.to_string()])
                .unwrap_err()
                .to_string();
            assert!(error.contains("either copied or mounted"), "{error}");
        }
        assert!(
            check_targets(&two, false, &["/home/pilot/c".to_string()]).is_ok()
        );
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
            }]
        );
        for (host, why) in [
            ("~/file", "isn't a directory"),
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
            },
            Resolved {
                vm: "/home/pilot/skills".into(),
                host: "/srv/skills".into(),
                writable: false,
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
    fn recorded_mounts_are_checked_again_against_todays_roots() {
        let roots = bay_roots(STATE, CACHE);
        let fine = [
            format!("/home/pilot\t{STATE}/bays/default/home\trw"),
            "/srv\t/srv/skills\tro".to_string(),
            "unreadable".to_string(),
        ];
        assert!(recheck(&fine, &roots).is_ok());
        // Allowed when created, but the state root has moved onto it.
        let moved = bay_roots("/home/you/work", CACHE);
        let error = recheck(&["/w\t/home/you/work/x\trw".into()], &moved)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "the bay's mount at /w: /home/you/work/x holds hangar's own \
             state: see docs/configuration.md, \"Mounting folders\""
        );
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
                writable: true,
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
        let none: [String; 0] = [];
        let below =
            mounts(&[("/home/pilot/.claude/skills", "~/skills", false)]);
        assert!(check_targets(&below, true, &none).is_ok());
        for vm in ["/home/pilot", "/home", "/"] {
            let error =
                check_targets(&mounts(&[(vm, "~/x", true)]), true, &none)
                    .unwrap_err()
                    .to_string();
            assert!(error.contains("bay's home is mounted"), "{vm}: {error}");
            assert!(
                check_targets(&mounts(&[(vm, "~/x", true)]), false, &none)
                    .is_ok()
            );
        }
        // Copied files may land in the home mount: only mounts conflict.
        let files = ["/home/pilot/.claude/CLAUDE.md".to_string()];
        assert!(check_targets(&BTreeMap::new(), true, &files).is_ok());
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
                ("/home/you/.local/share/hangar/vault", "hangar's own state"),
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
                writable: true,
            }
        );
        let shown =
            status(&BTreeMap::new(), None, Some(&cache), Path::new(HOME), None);
        assert!(shown.iter().all(|m| m.kind == MountKind::Cache));
    }
}
