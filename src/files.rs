//! `files`: config files copied from the host into a bay. Secrets never
//! go in: a refusal stops the whole step before anything is copied.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use crate::bay::{self, Bay, PILOT, VM_HOME};
use crate::config::env_var;
use crate::error::{Context, Error, Result, bail};
use crate::hangar::Hangar;
use crate::mounts::{self, Roots, mount_at};
use crate::sandbox::Sandbox;
use crate::secret::find_secret;
use crate::state::{fnv, read_hashes, write_hashes};
use log::{debug, info};

const MAX_FILE: u64 = 1024 * 1024;
const MAX_TOTAL: u64 = 10 * 1024 * 1024;
const MAX_FILES: usize = 1000;

const HINT: &str = "files never copies secrets; store them with \
                    'hangar credential set NAME' (see docs/configuration.md, \
                    \"Your config files in the VM\")";

#[derive(Debug, PartialEq)]
struct FileCopy {
    vm: String,
    executable: bool,
    contents: Vec<u8>,
}

/// VM path -> hash of what hangar last copied there.
type Record = BTreeMap<String, String>;

/// A VM path from config: absolute or `~/…`, always in pilot's home and
/// never escaping it with `..`. Pilot owns what lands there, so nothing
/// root runs or reads can be a target.
pub(crate) fn vm_path(raw: &str) -> Result<String> {
    let path = match raw.strip_prefix("~/") {
        Some(rest) => format!("{VM_HOME}/{rest}"),
        None => raw.to_string(),
    };
    let path = path.trim_end_matches('/');
    if !path.starts_with('/') {
        bail!("expected an absolute path or ~/…");
    }
    if path.contains(['\n', '\t', '\0']) {
        bail!("tabs, newlines and NUL aren't allowed");
    }
    let parsed = Path::new(path);
    if parsed.components().any(|c| c == Component::ParentDir) {
        bail!("'..' isn't allowed");
    }
    if !parsed.starts_with(VM_HOME) {
        bail!("must be in ~ ({VM_HOME}), the bay user's home");
    }
    Ok(path.to_string())
}

/// What a copy changed in the VM: paths only, never contents.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Copied {
    pub(crate) copied: Vec<String>,
    pub(crate) removed: Vec<String>,
}

pub(crate) enum Request<'a> {
    Declared,
    /// `hangar copy SRC [DEST]`: never recorded as managed. DEST defaults
    /// to SRC's place under the VM user's home.
    AdHoc {
        src: &'a str,
        dest: Option<&'a str>,
    },
}

pub(crate) fn copy_declared(hangar: &Hangar, bay: &Bay) -> Result<()> {
    copy(hangar, bay, &Request::Declared).map(drop)
}

/// The one way files reach the VM: every caller (`up`, `copy`, `restart`)
/// goes through the safety check in [`plan`], the pure [`plan_copy`] and
/// [`apply_copy`].
pub(crate) fn copy(
    hangar: &Hangar,
    bay: &Bay,
    request: &Request,
) -> Result<Copied> {
    let record_path = bay.dir.files();
    let recorded = read_hashes(&record_path);
    let declared = &bay.settings.files;
    if matches!(request, Request::Declared)
        && declared.is_empty()
        && recorded.is_empty()
    {
        return Ok(Copied::default());
    }
    let home = &hangar.host_home;
    let (entries, managed) = match request {
        Request::Declared => (declared.clone(), true),
        Request::AdHoc { src, dest } => {
            let (dest, src) = ad_hoc_entry(src, *dest, home)?;
            if let Some(mount) = mount_at(&dest, &bay.settings.mounts) {
                return Err(Error::with_hint(
                    format!("{dest} overlaps the mount at {mount}"),
                    "a mounted folder is live already; change it on the host",
                ));
            }
            (BTreeMap::from([(dest, src)]), false)
        }
    };
    let copies = plan(&entries, home, &bay::roots(hangar, bay)?)?;
    let plan = plan_copy(&copies, recorded, managed);
    let sandbox = hangar.sandbox.as_ref();
    let vm = bay.vm.as_str();
    apply_copy(
        plan,
        &copies,
        vm,
        |copy| write(sandbox, vm, copy),
        |paths| remove(sandbox, vm, paths),
        |record| write_hashes(&record_path, record),
    )
}

fn ad_hoc_entry(
    src: &str,
    dest: Option<&str>,
    home: &Path,
) -> Result<(String, String)> {
    let host = host_path(src, home)?;
    let dest = if let Some(dest) = dest {
        vm_path(dest).context(format!("DEST {dest}"))?
    } else {
        let Ok(relative) = host.strip_prefix(home) else {
            return Err(Error::with_hint(
                format!("{} is outside your home", host.display()),
                "give a DEST: hangar copy SRC DEST",
            ));
        };
        vm_path(&format!("~/{}", relative.display()))?
    };
    Ok((dest, host.display().to_string()))
}

/// The record's text: part of a `run` entry's fingerprint, so a
/// changed config file shows up as a changed input.
pub(crate) fn record_text(bay: &Bay) -> String {
    fs::read_to_string(bay.dir.files()).unwrap_or_default()
}

pub(crate) fn host_home() -> Result<PathBuf> {
    env_var("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

#[derive(Debug, PartialEq)]
struct CopyPlan {
    write: Vec<usize>,
    delete: Vec<String>,
    /// The record before any write; managed writes are added as they land.
    base: Record,
    managed: bool,
}

/// Declared (`managed`): write what changed, delete managed files whose
/// entry is gone. Ad hoc: write everything, delete nothing, and forget the
/// written paths' hashes so a declared entry for the same path wins again.
fn plan_copy(copies: &[FileCopy], recorded: Record, managed: bool) -> CopyPlan {
    let wanted = |vm: &str| copies.iter().any(|copy| copy.vm == vm);
    let (base, delete): (Record, Record) = if managed {
        recorded.into_iter().partition(|(vm, _)| wanted(vm))
    } else {
        let base = recorded.into_iter().filter(|(vm, _)| !wanted(vm)).collect();
        (base, Record::new())
    };
    let write = copies
        .iter()
        .enumerate()
        .filter(|(_, copy)| !managed || base.get(&copy.vm) != Some(&hash(copy)))
        .map(|(index, _)| index)
        .collect();
    CopyPlan {
        write,
        delete: delete.into_keys().collect(),
        base,
        managed,
    }
}

/// Deletes, then writes, saving the record after each managed write so a
/// failure can't lose track of the ones before it.
fn apply_copy(
    plan: CopyPlan,
    copies: &[FileCopy],
    vm: &str,
    mut write: impl FnMut(&FileCopy) -> Result<()>,
    remove: impl FnOnce(&[String]) -> Result<()>,
    mut save: impl FnMut(&Record) -> Result<()>,
) -> Result<Copied> {
    if !plan.delete.is_empty() {
        remove(&plan.delete)?;
    }
    let mut record = plan.base;
    save(&record)?;
    if !plan.write.is_empty() {
        info!("copying {} file(s) into {vm}", plan.write.len());
    }
    let mut written = Vec::new();
    for &index in &plan.write {
        let file = &copies[index];
        write(file)?;
        if plan.managed {
            record.insert(file.vm.clone(), hash(file));
            save(&record)?;
        }
        written.push(file.vm.clone());
    }
    Ok(Copied {
        copied: written,
        removed: plan.delete,
    })
}

fn remove(sandbox: &dyn Sandbox, vm: &str, paths: &[String]) -> Result<()> {
    info!("removing {}", paths.join(" "));
    let mut remove = vec!["rm", "-f", "--"];
    remove.extend(paths.iter().map(String::as_str));
    sandbox.exec(vm, Some(PILOT), &remove, None).map(drop)
}

fn plan(
    files: &BTreeMap<String, String>,
    home: &Path,
    roots: &Roots,
) -> Result<Vec<FileCopy>> {
    let mut copies = Vec::new();
    let mut total = 0;
    for (vm, host) in files {
        let host = host_path(host, home).context(format!("files.{vm}"))?;
        // Resolved, so a symlinked parent can't hide hangar's folders; a
        // missing source fails in `collect`.
        if let Ok(resolved) = fs::canonicalize(&host) {
            mounts::check_hangar_dirs(&resolved, roots)
                .map_err(|why| refuse(&resolved, why))?;
        }
        collect(&host, vm, &mut copies, &mut total)?;
    }
    Ok(copies)
}

/// A host path from config: absolute, `~` or `~/…` (the host `$HOME`).
pub(crate) fn host_path(raw: &str, home: &Path) -> Result<PathBuf> {
    let path = match raw.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if raw == "~" => home.to_path_buf(),
        None => PathBuf::from(raw),
    };
    if !path.is_absolute() {
        bail!("{raw}: expected an absolute path or ~/…");
    }
    Ok(path)
}

fn collect(
    host: &Path,
    vm: &str,
    copies: &mut Vec<FileCopy>,
    total: &mut u64,
) -> Result<()> {
    let meta = fs::symlink_metadata(host).context(host.display())?;
    let kind = meta.file_type();
    if kind.is_symlink() {
        return Err(refuse(host, "is a symlink; point at its target instead"));
    }
    if is_credentials_file(host) || is_credentials_file(Path::new(vm)) {
        return Err(refuse(host, "looks like a credentials file"));
    }
    if kind.is_dir() {
        let mut names = fs::read_dir(host)
            .and_then(Iterator::collect::<std::io::Result<Vec<_>>>)
            .context(host.display())?
            .into_iter()
            .map(|entry| entry.file_name())
            .collect::<Vec<_>>();
        names.sort();
        for name in names {
            let child = host.join(&name);
            let Some(name) =
                name.to_str().filter(|n| !n.contains(['\n', '\t']))
            else {
                return Err(refuse(&child, "has an unsupported file name"));
            };
            collect(&child, &format!("{vm}/{name}"), copies, total)?;
        }
        return Ok(());
    }
    if !kind.is_file() {
        return Err(refuse(host, "isn't a regular file"));
    }
    if meta.len() > MAX_FILE {
        let size = meta.len();
        return Err(refuse(
            host,
            format!("is {size} bytes; the limit is 1 MiB"),
        ));
    }
    *total += meta.len();
    if *total > MAX_TOTAL {
        return Err(refuse(host, "takes files past 10 MiB in total"));
    }
    if copies.len() == MAX_FILES {
        return Err(refuse(host, "takes files past 1000 files"));
    }
    let contents = fs::read(host).context(host.display())?;
    if let Some(what) = find_secret(&String::from_utf8_lossy(&contents)) {
        return Err(refuse(
            host,
            format!("contains what looks like a credential ({what})"),
        ));
    }
    copies.push(FileCopy {
        vm: vm.to_string(),
        executable: meta.permissions().mode() & 0o111 != 0,
        contents,
    });
    Ok(())
}

pub(crate) fn is_credentials_file(path: &Path) -> bool {
    const DIRS: [&str; 4] = [".ssh", ".gnupg", ".aws", ".kube"];
    const NAMES: [&str; 8] = [
        ".credentials.json",
        "credentials.json",
        ".env",
        ".netrc",
        ".git-credentials",
        "hosts.yml",
        ".npmrc",
        ".pypirc",
    ];
    const PREFIXES: [&str; 4] = [".env.", "id_rsa", "id_ed25519", "id_ecdsa"];
    const SUFFIXES: [&str; 5] = [".pem", ".key", ".p12", ".pfx", ".kdbx"];
    let parts: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    if parts.iter().any(|part| DIRS.contains(part))
        || parts.windows(2).any(|pair| {
            pair == [".config", "gh"] || pair == [".docker", "config.json"]
        })
    {
        return true;
    }
    parts.last().is_some_and(|name| {
        NAMES.contains(name)
            || PREFIXES.iter().any(|prefix| name.starts_with(prefix))
            || SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
    })
}

fn refuse(path: &Path, why: impl fmt::Display) -> Error {
    Error::with_hint(format!("files: {}: {why}", path.display()), HINT)
}

/// Writes atomically; the contents travel on stdin, never in argv.
const WRITE: &str = r#"mkdir -p "$(dirname "$1")" &&
cat >"$1.hangar-new" && chmod "$2" "$1.hangar-new" && mv -f "$1.hangar-new" "$1""#;

fn write(sandbox: &dyn Sandbox, vm: &str, copy: &FileCopy) -> Result<()> {
    debug!("copying {}", copy.vm);
    let mode = if copy.executable { "755" } else { "644" };
    sandbox
        .exec(
            vm,
            Some(PILOT),
            &["sh", "-c", WRITE, "hangar-file", &copy.vm, mode],
            Some(&copy.contents),
        )
        .map(drop)
}

/// Over the mode and contents.
fn hash(copy: &FileCopy) -> String {
    fnv(std::iter::once(u8::from(copy.executable))
        .chain(copy.contents.iter().copied()))
}

#[cfg(test)]
mod tests {
    use super::{
        Copied, CopyPlan, FileCopy, Record, apply_copy, hash,
        is_credentials_file, plan, plan_copy, vm_path,
    };
    use crate::error::Result;
    use crate::testing::{roots, scratch_dir};
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn copy(vm: &str, contents: &str) -> FileCopy {
        FileCopy {
            vm: vm.into(),
            executable: false,
            contents: contents.as_bytes().to_vec(),
        }
    }

    #[test]
    fn vm_paths_stay_in_pilots_home() {
        assert_eq!(
            vm_path("~/.claude/CLAUDE.md").unwrap(),
            "/home/pilot/.claude/CLAUDE.md"
        );
        assert_eq!(vm_path("/home/pilot/app/").unwrap(), "/home/pilot/app");
        assert_eq!(vm_path("~/").unwrap(), "/home/pilot");
        for (bad, why) in [
            ("relative/x", "absolute path"),
            ("~/../etc/passwd", "'..'"),
            ("/home/pilot/a\tb", "tabs"),
            ("/", "absolute path"),
            ("~x", "absolute path"),
        ] {
            let error = vm_path(bad).unwrap_err().to_string();
            assert!(error.contains(why), "{bad}: {error}");
        }
        for bad in [
            "/etc/app.conf",
            "/etc/profile.d/x.sh",
            "/usr/bin/x",
            "/bin",
            "/var/lib/x",
            "/run/hangar/ca.pem",
            "/nix/store/x",
            "/root/.claude/CLAUDE.md",
            "/home",
            "/home/pilotx/a",
        ] {
            let error = vm_path(bad).unwrap_err().to_string();
            assert!(
                error.contains("must be in ~ (/home/pilot)"),
                "{bad}: {error}"
            );
        }
    }

    #[test]
    fn credentials_files_are_recognised_by_name_and_place() {
        for path in [
            "/h/.claude/.credentials.json",
            "/h/.aws/credentials.json",
            "/h/project/.env",
            "/h/project/.env.local",
            "/h/server.pem",
            "/h/tls.key",
            "/h/cert.p12",
            "/h/cert.pfx",
            "/h/.ssh/id_ed25519",
            "/h/id_rsa.pub",
            "/h/vault.kdbx",
            "/h/.netrc",
            "/h/.git-credentials",
            "/h/.config/gh/hosts.yml",
            "/h/.npmrc",
            "/h/.pypirc",
            "/h/.docker/config.json",
            "/h/.ssh/config",
            "/h/.gnupg/gpg.conf",
            "/h/.kube/config",
            "/h/.config/gh/config.yml",
        ] {
            assert!(is_credentials_file(Path::new(path)), "{path}");
        }
        for path in [
            "/h/.claude/CLAUDE.md",
            "/h/.claude/settings.json",
            "/h/.config/app/config.json",
            "/h/keys.md",
            "/h/environment.md",
        ] {
            assert!(!is_credentials_file(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn plan_walks_directories_and_keeps_the_exec_bit() {
        let home = scratch_dir("files-plan");
        let claude = home.join("dotfiles/claude");
        fs::create_dir_all(claude.join("agents")).unwrap();
        fs::write(claude.join("CLAUDE.md"), "be terse").unwrap();
        fs::write(claude.join("agents/b.md"), "b").unwrap();
        fs::write(claude.join("agents/a.sh"), "#!/bin/sh").unwrap();
        fs::set_permissions(
            claude.join("agents/a.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let files = BTreeMap::from([
            (
                "/home/pilot/.claude/CLAUDE.md".into(),
                "~/dotfiles/claude/CLAUDE.md".into(),
            ),
            (
                "/home/pilot/.claude/agents".into(),
                "~/dotfiles/claude/agents".into(),
            ),
        ]);

        let copies =
            plan(&files, &home, &roots(&home, "state", "cache")).unwrap();
        let summary: Vec<(&str, bool)> = copies
            .iter()
            .map(|c| (c.vm.as_str(), c.executable))
            .collect();
        assert_eq!(
            summary,
            [
                ("/home/pilot/.claude/CLAUDE.md", false),
                ("/home/pilot/.claude/agents/a.sh", true),
                ("/home/pilot/.claude/agents/b.md", false),
            ]
        );
        assert_eq!(copies[0].contents, b"be terse");
    }

    fn refusal(home: &Path, vm: &str, host: &str) -> String {
        let files = BTreeMap::from([(vm.into(), host.into())]);
        plan(&files, home, &roots(home, "state", "cache"))
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn plan_refuses_secrets_links_and_oversized_input_before_copying() {
        let home = scratch_dir("files-refuse");
        fs::create_dir_all(home.join("state")).unwrap();
        fs::create_dir_all(home.join("cfg/.aws")).unwrap();
        fs::write(home.join("cfg/.aws/config"), "region = x").unwrap();
        fs::write(home.join("token.md"), format!("ghp_{}", "x".repeat(36)))
            .unwrap();
        fs::write(home.join("big"), vec![b'a'; 1024 * 1024 + 1]).unwrap();
        std::os::unix::fs::symlink(home.join("token.md"), home.join("link"))
            .unwrap();
        for (dir, file) in
            [("state/bays/other/home", "x"), ("cache/bays/other", "x")]
        {
            fs::create_dir_all(home.join(dir)).unwrap();
            fs::write(home.join(dir).join(file), "x").unwrap();
        }
        std::os::unix::fs::symlink(home.join("state"), home.join("alias"))
            .unwrap();
        for (host, why) in [
            ("~/cfg", "looks like a credentials file"),
            ("~/token.md", "looks like a credential (ghp_)"),
            ("~/link", "is a symlink"),
            ("~/big", "the limit is 1 MiB"),
            ("~/state", "hangar's own state"),
            ("~", "hangar's own state"),
            ("~/state/bays/other/home/x", "hangar's own state"),
            ("~/cache/bays/other/x", "hangar's package caches"),
            ("~/alias/bays/other/home/x", "hangar's own state"),
            ("relative", "expected an absolute path"),
        ] {
            let error = refusal(&home, "/home/pilot/x", host);
            assert!(error.contains(why), "{host}: {error}");
        }
        let error = refusal(
            &home,
            "/home/pilot/.claude/.credentials.json",
            "~/token.md",
        );
        assert!(error.contains("credentials file"), "{error}");
        let error = refusal(&home, "/home/pilot/x", "~/token.md");
        assert!(error.contains("hangar credential set"), "{error}");
    }

    #[test]
    fn plan_limits_the_total_size_and_the_file_count() {
        let home = scratch_dir("files-limits");
        let many = home.join("many");
        fs::create_dir_all(&many).unwrap();
        for i in 0..=1000 {
            fs::write(many.join(format!("{i:04}")), "x").unwrap();
        }
        let error = refusal(&home, "/home/pilot/many", "~/many");
        assert!(error.contains("past 1000 files"), "{error}");

        let big = home.join("big");
        fs::create_dir_all(&big).unwrap();
        for i in 0..11 {
            fs::write(big.join(format!("{i:02}")), vec![b'a'; 1024 * 1024])
                .unwrap();
        }
        let error = refusal(&home, "/home/pilot/big", "~/big");
        assert!(error.contains("past 10 MiB"), "{error}");
    }

    #[test]
    fn declared_copies_write_what_changed_and_delete_what_left() {
        let a = copy("/home/pilot/a", "same");
        let b = copy("/home/pilot/b", "new");
        let recorded = Record::from([
            ("/home/pilot/a".into(), hash(&a)),
            ("/home/pilot/gone".into(), "1".into()),
        ]);
        let plan = plan_copy(&[a, b], recorded, true);
        assert_eq!(plan.write, [1]);
        assert_eq!(plan.delete, ["/home/pilot/gone"]);
        assert_eq!(plan.base.keys().collect::<Vec<_>>(), ["/home/pilot/a"]);
    }

    #[test]
    fn ad_hoc_copies_write_everything_and_forget_the_hashes_they_shadow() {
        let a = copy("/home/pilot/a", "same");
        let recorded = Record::from([
            ("/home/pilot/a".into(), hash(&a)),
            ("/home/pilot/other".into(), "1".into()),
        ]);
        let plan = plan_copy(&[a], recorded, false);
        assert_eq!(plan.write, [0]);
        assert_eq!(plan.delete, Vec::<String>::new());
        assert_eq!(plan.base.keys().collect::<Vec<_>>(), ["/home/pilot/other"]);
    }

    fn run_plan(
        plan: CopyPlan,
        copies: &[FileCopy],
        fail_at: Option<&str>,
    ) -> (Result<Copied>, Vec<String>, Vec<String>, Record) {
        let mut written = Vec::new();
        let mut removed = Vec::new();
        let mut last = Record::new();
        let result = apply_copy(
            plan,
            copies,
            "hangar-bay-default",
            |copy| {
                if Some(copy.vm.as_str()) == fail_at {
                    crate::error::bail!("disk full");
                }
                written.push(copy.vm.clone());
                Ok(())
            },
            |paths| {
                removed.extend(paths.iter().cloned());
                Ok(())
            },
            |record| {
                last = record.clone();
                Ok(())
            },
        );
        (result, written, removed, last)
    }

    #[test]
    fn applying_a_plan_reports_paths_and_records_managed_writes() {
        let copies = [copy("/home/pilot/a", "1"), copy("/home/pilot/b", "2")];
        let recorded = Record::from([("/home/pilot/gone".into(), "1".into())]);
        let plan = plan_copy(&copies, recorded, true);
        let (result, written, removed, last) = run_plan(plan, &copies, None);
        assert_eq!(
            result.unwrap(),
            Copied {
                copied: vec!["/home/pilot/a".into(), "/home/pilot/b".into()],
                removed: vec!["/home/pilot/gone".into()],
            }
        );
        assert_eq!(written, ["/home/pilot/a", "/home/pilot/b"]);
        assert_eq!(removed, ["/home/pilot/gone"]);
        assert_eq!(
            last.keys().collect::<Vec<_>>(),
            ["/home/pilot/a", "/home/pilot/b"]
        );

        // Ad hoc: written, but never recorded.
        let plan = plan_copy(&copies, Record::new(), false);
        let (_, written, _, last) = run_plan(plan, &copies, None);
        assert_eq!(written, ["/home/pilot/a", "/home/pilot/b"]);
        assert_eq!(last, Record::new());
    }

    #[test]
    fn a_failed_copy_keeps_the_ones_before_it_recorded() {
        let copies = [copy("/home/pilot/a", "1"), copy("/home/pilot/b", "2")];
        let plan = plan_copy(&copies, Record::new(), true);
        let (result, _, _, last) =
            run_plan(plan, &copies, Some("/home/pilot/b"));
        assert!(result.unwrap_err().to_string().contains("disk full"));
        assert_eq!(last.keys().collect::<Vec<_>>(), ["/home/pilot/a"]);
    }

    #[test]
    fn the_hash_sees_contents_and_the_exec_bit() {
        let plain = copy("/home/pilot/a", "x");
        let executable = FileCopy {
            executable: true,
            ..copy("/home/pilot/a", "x")
        };
        assert_ne!(hash(&plain), hash(&copy("/home/pilot/a", "y")));
        assert_ne!(hash(&plain), hash(&executable));
        assert_eq!(hash(&plain), hash(&copy("/elsewhere", "x")));
    }
}
