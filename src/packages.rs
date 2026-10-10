//! `packages`: hangar's own nix profile in a bay, kept in line with the
//! config. Only what hangar installed is ever removed, and the package
//! cache is never trusted without upstream signatures.

use std::collections::{BTreeMap, BTreeSet};

use crate::bay::{PROFILE, PROXY_ENV_FILE, ROOT, VM_CACHE, VM_STATE};
use crate::error::{Context, Result};
use crate::json;
use crate::sandbox::Sandbox;
use log::{info, warn};

fn tracked_path() -> String {
    format!("{VM_STATE}/packages.json")
}

/// No record yet (a new VM): nothing is ours.
const READ_TRACKED: &str = r#"cat "$1" 2>/dev/null || echo '{}'"#;
const SAVE_TRACKED: &str = r#"mkdir -p "${1%/*}" && cat >"$1""#;
const NIX: &str = "/run/current-system/sw/bin/nix";
/// A new VM has no profile until the first install.
const LIST: &str = r#"[ -e "$1" ] || { echo '{"elements":{}}'; exit; }
exec /run/current-system/sw/bin/nix profile list --json --profile "$1""#;

/// Copies the profile's closure into the cache, signatures included.
const FILL_CACHE: &str = r#"exec /run/current-system/sw/bin/nix copy --to "$1" \
  "$(readlink -f "$2")""#;

/// Installable -> the profile element it became.
type Tracked = BTreeMap<String, String>;

#[derive(Debug, PartialEq)]
struct Plan {
    remove: Vec<String>,
    install: Vec<String>,
    keep: Tracked,
}

#[derive(Clone, Copy)]
struct Vm<'a> {
    sandbox: &'a dyn Sandbox,
    name: &'a str,
}

pub(crate) fn reconcile(
    sandbox: &dyn Sandbox,
    vm: &str,
    desired: &[String],
    cache: bool,
) -> Result<()> {
    let vm = Vm { sandbox, name: vm };
    let record = tracked_path();
    let read = ["/bin/sh", "-c", READ_TRACKED, "hangar-packages", &record];
    let tracked: Tracked = json::from_str(&exec(vm, &read)?)
        .context(format!("{}:{record}", vm.name))?;
    let plan = plan(desired, &tracked, &profile(vm)?);
    if !plan.remove.is_empty() {
        info!("removing {}", plan.remove.join(" "));
        let mut remove = vec![NIX, "profile", "remove", "--profile", PROFILE];
        remove.extend(plan.remove.iter().map(String::as_str));
        exec(vm, &remove)?;
    }
    let installs = !plan.install.is_empty();
    apply(
        plan,
        |installable| install(vm, installable, cache),
        |tracked| save(vm, tracked),
    )?;
    if cache && installs {
        fill_cache(vm);
    }
    Ok(())
}

/// The cache only saves time: a failed copy never fails `up`.
fn fill_cache(vm: Vm) {
    info!("filling the package cache");
    let dest = format!("file://{VM_CACHE}/nix?compression=zstd");
    let fill = ["/bin/sh", "-c", FILL_CACHE, "hangar-cache", &dest, PROFILE];
    if let Err(error) = exec(vm, &fill) {
        warn!("couldn't fill the package cache: {error}");
    }
}

/// Saves the record after every step, so a failed install can't orphan the
/// ones before it.
fn apply(
    plan: Plan,
    mut install: impl FnMut(&str) -> Result<Option<String>>,
    mut save: impl FnMut(&Tracked) -> Result<()>,
) -> Result<()> {
    let mut tracked = plan.keep;
    save(&tracked)?;
    for installable in &plan.install {
        if let Some(element) = install(installable)? {
            tracked.insert(installable.clone(), element);
            save(&tracked)?;
        }
    }
    Ok(())
}

/// The new profile element, or none when it was already installed by hand
/// (then it isn't ours to remove later).
fn install(vm: Vm, installable: &str, cache: bool) -> Result<Option<String>> {
    info!("installing {installable}");
    let before = profile(vm)?;
    let command = install_command(installable, cache);
    exec(vm, &command.iter().map(String::as_str).collect::<Vec<_>>())
        .context(format!("installing {installable}"))?;
    Ok(profile(vm)?.difference(&before).next().cloned())
}

fn save(vm: Vm, tracked: &Tracked) -> Result<()> {
    let stored = json::stringify(tracked);
    let record = tracked_path();
    let save = [
        "/bin/sh",
        "-c",
        SAVE_TRACKED,
        "hangar-packages-save",
        &record,
    ];
    vm.sandbox
        .exec(vm.name, Some(ROOT), &save, Some(stored.as_bytes()))?;
    Ok(())
}

fn plan(
    desired: &[String],
    tracked: &Tracked,
    profile: &BTreeSet<String>,
) -> Plan {
    let mut keep = Tracked::new();
    let mut remove = Vec::new();
    for (installable, element) in tracked {
        if !profile.contains(element) {
            continue;
        }
        if desired.contains(installable) {
            keep.insert(installable.clone(), element.clone());
        } else {
            remove.push(element.clone());
        }
    }
    let mut seen = BTreeSet::new();
    let install = desired
        .iter()
        .filter(|item| !keep.contains_key(*item) && seen.insert(*item))
        .cloned()
        .collect();
    Plan {
        remove,
        install,
        keep,
    }
}

#[derive(miniserde::Deserialize)]
struct Profile {
    elements: BTreeMap<String, json::Json>,
}

fn profile(vm: Vm) -> Result<BTreeSet<String>> {
    let list = exec(vm, &["/bin/sh", "-c", LIST, "hangar-profile", PROFILE])?;
    let profile: Profile = json::from_str(&list).context("nix profile list")?;
    Ok(profile.elements.into_keys().collect())
}

/// Root takes only the proxy env, never a login shell's: pilot can sway
/// what a login shell finds. Unfree is allowed only here: listing a
/// package in `packages` is the user accepting its license.
fn install_command(installable: &str, cache: bool) -> Vec<String> {
    let script = format!(
        "set -a && . {PROXY_ENV_FILE} && set +a && NIXPKGS_ALLOW_UNFREE=1 \
         exec {NIX} profile add --impure --profile \"$@\""
    );
    let mut command: Vec<String> = vec![
        "/bin/sh".into(),
        "-c".into(),
        script,
        "hangar-install".into(),
    ];
    command.push(PROFILE.into());
    // Tried before the internet caches (lower priority wins;
    // cache.nixos.org is 40). Not `trusted`: paths still need a signature.
    if cache {
        command.push("--extra-substituters".into());
        command.push(format!("file://{VM_CACHE}/nix?priority=10"));
    }
    command.push(installable.into());
    command
}

fn exec(vm: Vm, command: &[&str]) -> Result<String> {
    let stdout = vm.sandbox.exec(vm.name, Some(ROOT), command, None)?;
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::{FILL_CACHE, Plan, Tracked, apply, install_command, plan};
    use crate::error::Error;
    use std::collections::BTreeSet;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    fn tracked(pairs: &[(&str, &str)]) -> Tracked {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn installs_missing_removes_unlisted_and_ignores_hand_installed() {
        let desired = strings(&["llm#codex", "nixpkgs#rtk", "nixpkgs#rtk"]);
        let ours = tracked(&[("llm#codex", "codex"), ("llm#claude", "claude")]);
        let profile: BTreeSet<String> =
            strings(&["codex", "claude", "htop"]).into_iter().collect();
        assert_eq!(
            plan(&desired, &ours, &profile),
            Plan {
                remove: strings(&["claude"]),
                install: strings(&["nixpkgs#rtk"]),
                keep: tracked(&[("llm#codex", "codex")]),
            }
        );
    }

    #[test]
    fn nothing_to_do_when_profile_matches() {
        let desired = strings(&["llm#codex"]);
        let ours = tracked(&[("llm#codex", "codex")]);
        let profile = strings(&["codex"]).into_iter().collect();
        let plan = plan(&desired, &ours, &profile);
        assert!(plan.remove.is_empty() && plan.install.is_empty());
    }

    #[test]
    fn elements_removed_by_hand_are_reinstalled() {
        let desired = strings(&["llm#codex"]);
        let ours = tracked(&[("llm#codex", "codex")]);
        let plan = plan(&desired, &ours, &BTreeSet::new());
        assert_eq!(plan.install, strings(&["llm#codex"]));
        assert!(plan.keep.is_empty());
    }

    #[test]
    fn install_takes_only_the_proxy_env_and_absolute_tools() {
        let command = install_command("llm#claude-code", false);
        assert_eq!(
            command[..2],
            ["/bin/sh", "-c"],
            "never a login shell: root mustn't source what pilot can sway"
        );
        assert_eq!(
            command[2],
            "set -a && . /etc/hangar/proxy.env && set +a && \
             NIXPKGS_ALLOW_UNFREE=1 exec /run/current-system/sw/bin/nix \
             profile add --impure --profile \"$@\""
        );
        assert_eq!(
            command[3..],
            [
                "hangar-install",
                "/nix/var/nix/profiles/hangar",
                "llm#claude-code"
            ]
        );
    }

    #[test]
    fn with_the_cache_installs_try_it_first_and_still_need_signatures() {
        let command = install_command("llm#codex", true);
        let at = command
            .iter()
            .position(|arg| arg == "--extra-substituters")
            .unwrap();
        assert_eq!(command[at + 1], "file:///var/cache/hangar/nix?priority=10");
        assert_eq!(command.last().unwrap(), "llm#codex");
        // Nothing makes the writable cache trusted or skips signature checks.
        assert!(command.iter().all(|arg| !arg.contains("trusted")
            && !arg.contains("require-sigs")
            && !arg.contains("no-check-sigs")));
        assert!(!FILL_CACHE.contains("no-check-sigs"));
    }

    #[test]
    fn a_failed_install_keeps_the_record_of_earlier_ones() {
        let plan = Plan {
            remove: Vec::new(),
            install: strings(&["a", "b"]),
            keep: tracked(&[("kept", "kept")]),
        };
        let mut saved = Vec::new();
        let result = apply(
            plan,
            |installable| match installable {
                "a" => Ok(Some("a-element".to_string())),
                _ => Err(Error::new("network down")),
            },
            |record| {
                saved.push(record.clone());
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(
            saved.last(),
            Some(&tracked(&[("kept", "kept"), ("a", "a-element")]))
        );
    }

    #[test]
    fn hand_installed_packages_are_not_recorded() {
        let plan = Plan {
            remove: Vec::new(),
            install: strings(&["a"]),
            keep: Tracked::new(),
        };
        let mut saved = Vec::new();
        apply(
            plan,
            |_| Ok(None),
            |record| {
                saved.push(record.clone());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(saved, [Tracked::new()]);
    }
}
