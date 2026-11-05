//! A bay: a locked-down VM that AI coding agents run in. Its only way
//! out is its proxy, or the open internet without one.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use crate::config::BaySettings;
use crate::error::{Context, Error, Result, bail};
use crate::hangar::Hangar;
use crate::lifecycle::{self, Outcome, Plan, disk_changed};
use crate::mounts::{self, Resolved, Roots};
use crate::proxy::{self, Access};
use crate::sandbox::{
    BoxState, Egress, Mount, MountMode, Sandbox, VmSpec, Volume, data_volume,
};
use crate::secret::Secret;
use crate::state::{BayDir, fnv, write_private};
use crate::vm_record::VmRecord;
use crate::{files, packages, process};
use log::{info, warn};

/// One bay, as its steps see it; built by `Hangar::bay` without I/O.
pub(crate) struct Bay<'a> {
    pub(crate) name: &'a str,
    pub(crate) vm: String,
    pub(crate) settings: &'a BaySettings,
    pub(crate) dir: BayDir,
    pub(crate) cache: PathBuf,
}

impl Bay<'_> {
    /// The bay's own home and package cache folders.
    pub(crate) fn own_mounts(&self) -> (Option<String>, Option<String>) {
        let shown = |path: &Path| Some(path.display().to_string());
        (shown(&self.dir.home()), shown(&self.cache))
    }
}

/// The home of `PILOT`, the bay user; `~` in `files` and `mounts`.
pub(crate) const VM_HOME: &str = "/home/pilot";
/// Who runs the apps, their setup, shells and copied files in a bay; the
/// image provides it (`nix/bay/configuration.nix`).
pub(crate) const PILOT: &str = "pilot";
const PILOT_ID: u32 = 1000;
/// Who runs hangar's own steps in a bay.
pub(crate) const ROOT: &str = "root";
/// The image's systemd, which the VM boots into.
const INIT: &str = "/sbin/init";
/// Pilot's data, Docker's included, on the bay's data volume.
const PILOT_DATA: &str = "/var/lib/pilot";
/// hangar's own records inside the VM, on its own disk next to `/nix`:
/// never in the home, which `home` keeps across new VMs.
pub(crate) const VM_STATE: &str = "/var/lib/hangar";
/// The nix profile `packages` go to, on the VM disk with the store it
/// points into, so a new VM never inherits a dangling one.
pub(crate) const PROFILE: &str = "/nix/var/nix/profiles/hangar";
/// Where `cache` is mounted: a `file://` binary cache of installed
/// packages, so a new VM gets them from the host instead of the internet.
pub(crate) const VM_CACHE: &str = "/var/cache/hangar";

pub(crate) fn roots(hangar: &Hangar, bay: &Bay) -> Result<Roots> {
    Roots::new(
        &hangar.host_home,
        hangar.state.root(),
        &hangar.cache_root,
        &bay.dir.home(),
        &bay.cache,
    )
}

/// Brings the bay in line with its config and says what that changed.
pub(crate) fn up(hangar: &Hangar, bay: &Bay) -> Outcome {
    reconcile(hangar, bay).unwrap_or_else(Outcome::Failed)
}

pub(crate) fn down(hangar: &Hangar, bay: &Bay) -> Result<Outcome> {
    if hangar.sandbox.state(&bay.vm)? != BoxState::Running {
        return Ok(Outcome::Unchanged);
    }
    hangar.sandbox.stop(&bay.vm)?;
    Ok(Outcome::Stopped)
}

fn reconcile(hangar: &Hangar, bay: &Bay) -> Result<Outcome> {
    let sandbox = hangar.sandbox.as_ref();
    let (vm, mut changes) = match step(bay, "vm", || ensure_vm(hangar, bay))? {
        Some(Outcome::Updated(changes)) => (None, changes),
        vm => (vm, Vec::new()),
    };
    step(bay, "boot", || {
        wait_booted(sandbox, bay, WAIT_TIMEOUT, WAIT_PAUSE)
    })?;
    step(bay, "proxy env", || {
        let proxy = hangar.proxy_of(bay.settings);
        let access = proxy::access(proxy, bay.name, sandbox.host_address())?;
        let env = proxy_env(proxy.map(|(_, proxy)| proxy.kind()), &access);
        let ca = access.ca_pem.as_deref().unwrap_or_default();
        changes.extend(apply_proxy(hangar, bay, &env, ca)?.map(String::from));
        wait_for_docker(sandbox, bay, WAIT_TIMEOUT, WAIT_PAUSE)
    })?;
    let added = step(bay, "packages", || {
        packages::reconcile(sandbox, &bay.vm, &bay.settings.packages, true)
    })?;
    changes.extend(listed("packages", &added));
    let copied = step(bay, "copies", || files::copy_declared(hangar, bay))?;
    let copied: Vec<String> = copied
        .copied
        .iter()
        .map(|path| format!("+{path}"))
        .chain(copied.removed.iter().map(|path| format!("-{path}")))
        .collect();
    changes.extend(listed("copies", &copied));
    if step(bay, "env", || write_env(hangar, bay))? {
        changes.push("env".into());
    }
    let needing_setup = step(bay, "setup checks", || check_setup(hangar, bay))?;
    changes.extend(step(bay, "apps", || {
        sync_apps(hangar, bay, &needing_setup)
    })?);
    Ok(vm.unwrap_or(if changes.is_empty() {
        Outcome::Unchanged
    } else {
        Outcome::Updated(changes)
    }))
}

fn step<T>(
    bay: &Bay,
    name: &str,
    run: impl FnOnce() -> Result<T>,
) -> Result<T> {
    lifecycle::step(&format!("bay {}", bay.name), name, run)
}

fn listed(label: &str, items: &[String]) -> Option<String> {
    (!items.is_empty()).then(|| format!("{label} {}", items.join(" ")))
}

fn egress(hangar: &Hangar, bay: &Bay) -> Egress {
    hangar
        .proxy_of(bay.settings)
        .map_or(Egress::Open, |(_, proxy)| proxy.egress())
}

/// Resolved, so symlinks can't hide where a mount points; checked before
/// anything is created.
fn resolve_mounts(hangar: &Hangar, bay: &Bay) -> Result<Vec<Resolved>> {
    let (home, cache) = bay.own_mounts();
    let all = mounts::with_hangar(
        &bay.settings.mounts,
        home.as_ref(),
        cache.as_ref(),
    );
    mounts::resolve(&all, &roots(hangar, bay)?)
}

/// The VM's outcome when it was created, recreated or started; `None`
/// when the running one is used.
fn ensure_vm(hangar: &Hangar, bay: &Bay) -> Result<Option<Outcome>> {
    let resolved = resolve_mounts(hangar, bay)?;
    let image = bay.settings.image_ref();
    let wanted = wanted_record(
        bay,
        &image,
        egress(hangar, bay),
        mounts::lines(&resolved),
    );
    let recorded = VmRecord::load(&bay.dir.vm());
    let sandbox = hangar.sandbox.as_ref();
    match lifecycle::plan(sandbox.state(&bay.vm)?, recorded.as_ref(), &wanted) {
        Plan::Create => {
            create(hangar, bay, &wanted, &resolved)?;
            Ok(Some(Outcome::Created))
        }
        Plan::Recreate(reasons) => {
            let disk = recorded
                .is_some_and(|recorded| disk_changed(&recorded, &wanted));
            recreate(hangar, bay, &wanted, &resolved, disk)?;
            let mut reasons: Vec<String> =
                reasons.into_iter().map(String::from).collect();
            if disk {
                reasons.push("data disk replaced".into());
            }
            Ok(Some(Outcome::Recreated(reasons)))
        }
        Plan::Start => {
            sandbox.start(&bay.vm)?;
            Ok(Some(Outcome::Updated(vec!["started".into()])))
        }
        Plan::Keep => Ok(None),
    }
}

/// Sorted, so the same config always records the same VM.
pub(crate) fn wanted_record(
    bay: &Bay,
    image: &str,
    egress: Egress,
    mut mounts: Vec<String>,
) -> VmRecord {
    let settings = bay.settings;
    let mut ports: Vec<(String, u16, u16)> = settings
        .ports
        .iter()
        .map(|port| (port.name.clone(), port.host, port.vm))
        .collect();
    ports.sort();
    mounts.sort();
    VmRecord {
        image: Some(image.to_string()),
        egress: Some(egress.record()),
        resources: Some(lifecycle::resources(
            settings.cpus,
            &settings.memory,
            Some(&settings.disk),
        )),
        fingerprint: None,
        ports,
        mounts,
    }
}

/// The home, the package cache and (unless `disk` changed) the data
/// volume outlive the VM.
fn recreate(
    hangar: &Hangar,
    bay: &Bay,
    wanted: &VmRecord,
    resolved: &[Resolved],
    disk: bool,
) -> Result<()> {
    let sandbox = hangar.sandbox.as_ref();
    // Trap: remove is a hard kill that loses unsynced writes.
    if sandbox.state(&bay.vm)? == BoxState::Running {
        sandbox.stop(&bay.vm)?;
    }
    remove_if_present(&bay.dir.vm())?;
    sandbox.remove(&bay.vm)?;
    if disk {
        sandbox.remove_volume(&data_volume(bay.name))?;
    }
    create(hangar, bay, wanted, resolved)
}

/// A VM that fails to create is left without a record, so the next `up`
/// recreates it.
fn create(
    hangar: &Hangar,
    bay: &Bay,
    wanted: &VmRecord,
    resolved: &[Resolved],
) -> Result<()> {
    let settings = bay.settings;
    let image = settings.image_ref();
    ensure_image(hangar, bay, &image)?;
    let volume = data_volume(bay.name);
    hangar.sandbox.ensure_volume(&volume, &settings.disk)?;
    // The cache is root's, which installs the packages; the rest pilot's.
    let mounts: Vec<Mount> = resolved
        .iter()
        .map(|mount| Mount {
            host: &mount.host,
            guest: &mount.vm,
            mode: MountMode::writable(mount.writable),
            owner: (mount.vm != VM_CACHE).then_some((PILOT_ID, PILOT_ID)),
            file: mount.file,
        })
        .collect();
    let publish: Vec<(u16, u16)> = wanted
        .ports
        .iter()
        .map(|(_, host, vm)| (*host, *vm))
        .collect();
    hangar.sandbox.create(&VmSpec {
        name: &bay.vm,
        image: &image,
        cpus: settings.cpus,
        memory: &settings.memory,
        disk: Some(&settings.disk),
        entrypoint: None,
        volumes: &[Volume {
            name: &volume,
            guest: PILOT_DATA,
            size: &settings.disk,
        }],
        init: Some(INIT),
        egress: egress(hangar, bay),
        publish: &publish,
        mounts: &mounts,
        env: &[],
    })?;
    // A new VM has none of the files the record says were copied.
    remove_if_present(&bay.dir.copies())?;
    write_private(&bay.dir.vm(), wanted.render().as_bytes())
}

pub(crate) fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != ErrorKind::NotFound => {
            Err(error).context(path.display())
        }
        _ => Ok(()),
    }
}

/// systemd's state once it can tell.
const BOOTED: &str = "/run/current-system/sw/bin/systemctl is-system-running --wait \
     2>/dev/null || :";
const WAIT_PAUSE: Duration = Duration::from_millis(500);
const WAIT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Probe {
    Done,
    /// Not yet, with what it last said.
    Pending(String),
    /// Waiting won't help.
    Failed(String),
}

/// Probes every `pause` until done, failed or `timeout` passed; the last
/// answer.
pub(crate) fn wait(
    timeout: Duration,
    pause: Duration,
    mut probe: impl FnMut() -> Probe,
) -> Probe {
    let deadline = Instant::now() + timeout;
    loop {
        match probe() {
            Probe::Pending(_) if Instant::now() < deadline => {
                thread::sleep(pause);
            }
            last => return last,
        }
    }
}

/// Root's exec fails until activation wrote passwd, and systemd says
/// `offline` until it's up; `degraded` is booted too. Shutting down or
/// stuck in maintenance, it never boots.
fn boot_state(output: Result<Vec<u8>>) -> Probe {
    match output {
        Err(error) => Probe::Pending(error.to_string()),
        Ok(output) => match String::from_utf8_lossy(&output).trim() {
            "running" | "degraded" => Probe::Done,
            other @ ("maintenance" | "stopping") => {
                Probe::Failed(format!("systemd is {other:?}"))
            }
            other => Probe::Pending(format!("systemd is {other:?}")),
        },
    }
}

/// The sandbox starts a VM before its systemd is up.
fn wait_booted(
    sandbox: &dyn Sandbox,
    bay: &Bay,
    timeout: Duration,
    pause: Duration,
) -> Result<()> {
    let booted = ["/bin/sh", "-c", BOOTED, "hangar-booted"];
    let exec = || sandbox.exec(&bay.vm, Some(ROOT), &booted, None);
    match wait(timeout, pause, || boot_state(exec())) {
        Probe::Done => Ok(()),
        Probe::Failed(state) => {
            bail!("{} can't finish booting: {state}", bay.vm)
        }
        Probe::Pending(state) => {
            bail!("{} did not finish booting: {state}", bay.vm)
        }
    }
}

/// Ready once a restart the proxy env triggered is done and dockerd
/// answers: its unit is active before its socket is.
const DOCKER_READY: &str = r#"jobs=$(systemctl list-jobs --no-legend hangar-proxy-env.service)
[ -z "$jobs" ] || { echo "restarting the daemons: $jobs" >&2; exit 1; }
DOCKER_HOST=unix:///run/user/1000/docker.sock exec docker info >/dev/null"#;

fn wait_for_docker(
    sandbox: &dyn Sandbox,
    bay: &Bay,
    timeout: Duration,
    pause: Duration,
) -> Result<()> {
    let ready = ["sh", "-c", DOCKER_READY, "hangar-docker"];
    let probe = || match sandbox.exec(&bay.vm, Some(PILOT), &ready, None) {
        Ok(_) => Probe::Done,
        Err(error) => Probe::Pending(error.to_string()),
    };
    match wait(timeout, pause, probe) {
        Probe::Done => Ok(()),
        Probe::Pending(state) | Probe::Failed(state) => Err(Error::with_hint(
            format!("dockerd in {} did not start: {state}", bay.vm),
            format!(
                "in 'hangar exec {}', run 'systemctl --user reset-failed \
                 docker && systemctl --user restart docker'",
                bay.name
            ),
        )),
    }
}

/// The bay's env, `KEY='value'` lines every login shell sources with
/// `set -a` (`nix/bay/configuration.nix`), like `PROXY_ENV_FILE`.
pub(crate) const ENV_FILE: &str = "/etc/hangar/bay.env";
/// The daemons' proxy and CA variables, a systemd `EnvironmentFile`.
pub(crate) const PROXY_ENV_FILE: &str = "/etc/hangar/proxy.env";

/// A file root keeps in a bay, written from what `fill` prints.
struct VmFile {
    /// The exec's `$0`.
    name: &'static str,
    path: &'static str,
    owner: &'static str,
    mode: &'static str,
    fill: &'static str,
}

const BAY_ENV: VmFile = VmFile {
    name: "hangar-env",
    path: ENV_FILE,
    owner: "root:root",
    mode: "0644",
    fill: "cat",
};

/// Pilot's apps read it too, so pilot's group may.
const PROXY_ENV: VmFile = VmFile {
    name: "hangar-proxy-env",
    path: PROXY_ENV_FILE,
    owner: "root:pilot",
    mode: "0640",
    fill: "cat",
};

/// The image's CAs and the proxy's, which comes on stdin
/// (`nix/bay/configuration.nix`).
const CA_BUNDLE: VmFile = VmFile {
    name: "hangar-ca",
    path: "/var/lib/hangar/ca-bundle.crt",
    owner: "root:root",
    mode: "0644",
    fill: "cat /etc/hangar/system-ca.crt -",
};

/// Replaced whole, so nothing reads half a file, and only when it changed:
/// the bay restarts its daemons whenever `PROXY_ENV_FILE` is replaced.
/// Prints `changed` or `same`.
fn replace_script(fill: &str) -> String {
    format!(
        r#"umask 077 && {fill} >"$1.tmp" && chown "$2" "$1.tmp" &&
chmod "$3" "$1.tmp" &&
if cmp -s "$1.tmp" "$1"; then rm "$1.tmp" && echo same
else mv "$1.tmp" "$1" && echo changed; fi"#
    )
}

/// True when the file changed.
fn replace(
    sandbox: &dyn Sandbox,
    vm: &str,
    file: &VmFile,
    stdin: Option<&[u8]>,
) -> Result<bool> {
    let script = replace_script(file.fill);
    let command = [
        "/bin/sh", "-c", &script, file.name, file.path, file.owner, file.mode,
    ];
    let output = sandbox.exec(vm, Some(ROOT), &command, stdin)?;
    Ok(String::from_utf8_lossy(&output)
        .trim_end()
        .ends_with("changed"))
}

/// The CA bundle first: a new proxy env restarts the daemons that read it.
/// Trap: the guest watches only the proxy env, and a new CA leaves it as
/// it was, so a new CA alone restarts them from here.
fn apply_proxy(
    hangar: &Hangar,
    bay: &Bay,
    env: &Secret,
    ca_pem: &[u8],
) -> Result<impl Iterator<Item = &'static str>> {
    let sandbox = hangar.sandbox.as_ref();
    let ca = replace(sandbox, &bay.vm, &CA_BUNDLE, Some(ca_pem))?;
    let env = env.expose().as_bytes();
    let login = replace(sandbox, &bay.vm, &PROXY_ENV, Some(env))?;
    if ca && !login {
        let restart = ["systemctl", "start", "hangar-proxy-env.service"];
        sandbox.exec(&bay.vm, Some(ROOT), &restart, None)?;
    }
    Ok([(ca, "proxy CA"), (login, "proxy login")]
        .into_iter()
        .filter_map(|(changed, label)| changed.then_some(label)))
}

/// The proxy for every client, and the CA bundle under every name a tool
/// looks for; a direct bay gets only the bundle.
fn proxy_env(kind: Option<&str>, access: &Access) -> Secret {
    let mut env = BTreeMap::new();
    if let Some(url) = &access.url {
        for name in ["HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"] {
            env.insert(name.to_string(), url.expose().to_string());
        }
        for name in ["NO_PROXY", "no_proxy"] {
            env.insert(name.to_string(), "localhost,127.0.0.1".to_string());
        }
    }
    for name in [
        "SSL_CERT_FILE",
        "NIX_SSL_CERT_FILE",
        "CURL_CA_BUNDLE",
        "NODE_EXTRA_CA_CERTS",
        "GIT_SSL_CAINFO",
    ] {
        env.insert(name.to_string(), CA_BUNDLE.path.to_string());
    }
    Secret::new(format!(
        "# proxy {}\n{}",
        kind.unwrap_or("none"),
        render_env(&env)
    ))
}

/// True when the file changed.
pub(crate) fn write_env(hangar: &Hangar, bay: &Bay) -> Result<bool> {
    let rendered = render_env(&bay.settings.env);
    replace(
        hangar.sandbox.as_ref(),
        &bay.vm,
        &BAY_ENV,
        Some(rendered.as_bytes()),
    )
}

fn render_env(env: &BTreeMap<String, String>) -> String {
    env.iter()
        .map(|(key, value)| {
            [key, "='", &value.replace('\'', r"'\''"), "'\n"].concat()
        })
        .collect()
}

/// `msb exec` is no login: nothing points pilot's `systemctl --user` at
/// its user manager.
const USER_MANAGER: &str = "export XDG_RUNTIME_DIR=/run/user/1000\n";

/// Starts the unit `$1` unless it runs with the description `$3` plus the
/// checksum of the proxy env and CA bundle (`$4`, `$5`), restarting it
/// otherwise; prints `running`, `started` or `restarted`. The checksum is
/// taken in the guest so nothing derived from the login reaches the host.
/// `--collect` unloads the unit once it exits, failed or not, so its name
/// is free again. The login shell brings the proxy env, `bay.env` and the
/// profile.
const START_RUN: &str = r#"wanted="$3 $(cat "$4" "$5" | cksum)"
case $(systemctl --user is-active "$1") in
active | activating)
  [ "$(systemctl --user show -P Description "$1")" = "$wanted" ] &&
    exec echo running
  systemctl --user stop "$1" && again=re ;;
esac
systemd-run --user --quiet --collect --unit="$1" --description="$wanted" \
  --property=TimeoutStopSec=10s /bin/sh -lc "$2" && echo "${again}started""#;

const LIST_RUNS: &str =
    "systemctl --user list-units 'hangar-run-*' --plain --no-legend";
const STOP_RUN: &str = r#"systemctl --user stop "$1""#;
const RUN_PREFIX: &str = "hangar-run-";

/// A `run` entry's transient unit in pilot's user manager: stopping it
/// stops every process the entry started, and its output is its journal.
pub(crate) fn run_unit(name: &str) -> String {
    format!("{RUN_PREFIX}{name}.service")
}

/// Runs an app's setup `check` in a login shell (PATH and app env apply);
/// only its exit status counts.
const SETUP_CHECK: &str =
    r#"if sh -lc "$1" >/dev/null 2>&1; then echo done; else echo needed; fi"#;

pub(crate) fn setup_done(
    sandbox: &dyn Sandbox,
    vm: &str,
    check: &str,
) -> Result<bool> {
    let output = sandbox.exec(
        vm,
        Some(PILOT),
        &["sh", "-c", SETUP_CHECK, "hangar-setup-check", check],
        None,
    )?;
    match String::from_utf8_lossy(&output).trim() {
        "done" => Ok(true),
        "needed" => Ok(false),
        other => bail!("setup check: unexpected {other:?}"),
    }
}

/// The enabled apps whose setup check fails, each with a warning. No
/// record is kept: the check is the truth, so a wiped home asks for setup
/// again.
fn check_setup(hangar: &Hangar, bay: &Bay) -> Result<Vec<String>> {
    let names = needing_setup(hangar, bay)?;
    for name in &names {
        warn!("{name} needs setup: run 'hangar setup {} {name}'", bay.name);
    }
    Ok(names)
}

pub(crate) fn needing_setup(hangar: &Hangar, bay: &Bay) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for app in &bay.settings.apps {
        if let Some(setup) = &app.setup
            && !setup_done(hangar.sandbox.as_ref(), &bay.vm, &setup.check)?
        {
            names.push(app.name.clone());
        }
    }
    Ok(names)
}

/// Starts what isn't running, restarts what runs with other inputs and
/// stops what no app lists; apps that need setup are left alone.
fn sync_apps(
    hangar: &Hangar,
    bay: &Bay,
    needing_setup: &[String],
) -> Result<Vec<String>> {
    let mut changes = Vec::new();
    for (name, command) in &bay.settings.run {
        if needing_setup.contains(name) {
            continue;
        }
        let done = launch(hangar, bay, name, command)?;
        if matches!(done.as_str(), "started" | "restarted") {
            changes.push(format!("{done} {name}"));
        }
    }
    for name in unlisted_runs(hangar, bay)? {
        user_units(
            hangar,
            bay,
            STOP_RUN,
            &["hangar-run-stop", &run_unit(&name)],
        )?;
        changes.push(format!("stopped {name}"));
    }
    Ok(changes)
}

fn unlisted_runs(hangar: &Hangar, bay: &Bay) -> Result<Vec<String>> {
    Ok(listed_runs(hangar, bay)?
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| !bay.settings.run.contains_key(name))
        .collect())
}

/// The names of the entries whose unit is running.
pub(crate) fn active_runs(
    hangar: &Hangar,
    bay: &Bay,
) -> Result<HashSet<String>> {
    Ok(listed_runs(hangar, bay)?
        .into_iter()
        .filter(|(_, active)| active == "active")
        .map(|(name, _)| name)
        .collect())
}

/// Every entry's unit systemd knows (a failed one too) with its ACTIVE
/// column.
fn listed_runs(hangar: &Hangar, bay: &Bay) -> Result<Vec<(String, String)>> {
    let units = user_units(hangar, bay, LIST_RUNS, &["hangar-runs"])?;
    Ok(units
        .lines()
        .filter_map(|line| {
            let mut columns = line.split_whitespace();
            let unit = columns.next()?;
            let name =
                unit.strip_prefix(RUN_PREFIX)?.strip_suffix(".service")?;
            Some((name.to_string(), columns.nth(1)?.to_string()))
        })
        .collect())
}

/// The host's half of the unit's description: a hash of the entry's
/// command, `bay.env` and the copied config files, none of them secret.
fn description(bay: &Bay, name: &str, command: &str) -> String {
    let env = render_env(&bay.settings.env);
    let copies = files::record_text(bay);
    let inputs = [command, &env, &copies].join("\0");
    format!("hangar run {name} {}", fnv(inputs.into_bytes()))
}

/// The one place an entry is (re)started: what `START_RUN` printed.
fn launch(
    hangar: &Hangar,
    bay: &Bay,
    name: &str,
    command: &str,
) -> Result<String> {
    let unit = run_unit(name);
    let description = description(bay, name, command);
    let args = [
        "hangar-run",
        &unit,
        command,
        &description,
        PROXY_ENV_FILE,
        CA_BUNDLE.path,
    ];
    user_units(hangar, bay, START_RUN, &args)
}

/// Runs `script` against pilot's user manager; its trimmed stdout.
fn user_units(
    hangar: &Hangar,
    bay: &Bay,
    script: &str,
    args: &[&str],
) -> Result<String> {
    let script = format!("{USER_MANAGER}{script}");
    let mut command = vec!["sh", "-c", &script];
    command.extend(args);
    let output = hangar.sandbox.exec(&bay.vm, Some(PILOT), &command, None)?;
    Ok(String::from_utf8_lossy(&output).trim().to_string())
}

/// A Nix-built image is loaded into the sandbox once per image tag.
fn ensure_image(hangar: &Hangar, bay: &Bay, image: &str) -> Result<()> {
    let Some(loader) = &bay.settings.image_loader else {
        return Ok(());
    };
    if hangar.sandbox.image_present(image) {
        return Ok(());
    }
    info!("loading {image}");
    let mut stream = process::command(loader);
    let mut stream = process::spawn(stream.stdout(Stdio::piped()))?;
    let stdout = stream.stdout.take().context("no image stream")?;
    let imported = hangar.sandbox.load_image(image, stdout.into());
    if !stream.wait().context(loader)?.success() {
        bail!("loading {image} failed: {loader} failed");
    }
    imported
}

#[cfg(test)]
mod tests {
    use super::{
        Probe, START_RUN, WAIT_TIMEOUT, boot_state, check_setup, down,
        ensure_image, proxy_env, replace_script, sync_apps, up, wait_booted,
        wait_for_docker, write_env,
    };
    use crate::error::Error;
    use crate::lifecycle::Outcome;
    use crate::proxy::Access;
    use crate::sandbox::fake::FakeSandbox;
    use crate::sandbox::{BoxState, Egress};
    use crate::secret::Secret;
    use crate::testing::{DIRECT, hangar_with, scratch_dir, settled};
    use std::fs;
    use std::path::Path;
    use std::rc::Rc;
    use std::time::Duration;

    const BAY_VM: &str = "hangar-bay-default";
    const WORK_VM: &str = "hangar-bay-work";
    const PROXIED: &str = r#"{"proxies": {"vault": {"kind": "fake",
        "url": "http://127.0.0.1:17323", "port": 17323,
        "login": "agent:pw-1"}},
        "bays": [{"name": "work", "proxy": "vault", "apps": ["paperclip"]}]}"#;
    const WORK_DIRECT: &str = r#"{"bays": [{"name": "work", "proxy": null}]}"#;
    const LAUNCH: &str = " hangar-run hangar-run-paperclip";

    fn fake(vms: &[(&str, BoxState)]) -> Rc<FakeSandbox> {
        Rc::new(FakeSandbox::with(vms))
    }

    fn up_with(
        config: &str,
        state: &Path,
        sandbox: &Rc<FakeSandbox>,
    ) -> String {
        let hangar = hangar_with(config, state, sandbox.clone());
        format!("{:?}", up(&hangar, &hangar.bay("work").unwrap()))
    }

    fn forget(sandbox: &FakeSandbox) {
        sandbox.calls.borrow_mut().clear();
        sandbox.stdins.borrow_mut().clear();
        sandbox.created.borrow_mut().clear();
    }

    fn stdin_of(sandbox: &FakeSandbox, pattern: &str) -> String {
        let calls = sandbox.calls.borrow();
        let at = calls
            .iter()
            .filter(|call| call.starts_with("exec "))
            .position(|call| call.contains(pattern))
            .unwrap();
        String::from_utf8(sandbox.stdins.borrow()[at].clone()).unwrap()
    }

    /// The calls a fresh `up` of `config` makes in a fresh state.
    fn first_up_calls(config: &str) -> Vec<String> {
        let state = scratch_dir("bay-up-argv");
        let sandbox = settled();
        assert_eq!(up_with(config, &state, &sandbox), "Created");
        sandbox.calls.borrow().clone()
    }

    fn position(changes: &[String], call: &str) -> usize {
        changes.iter().position(|c| c == call).unwrap()
    }

    fn position_of(changes: &[String], pattern: &str) -> usize {
        changes.iter().position(|c| c.contains(pattern)).unwrap()
    }

    const RESTART_DAEMONS: &str =
        "exec root@hangar-bay-work systemctl start hangar-proxy-env.service";

    #[test]
    fn new_bay_is_created_with_proxy_egress_and_reported_created() {
        let state = scratch_dir("bay-up-create");
        let sandbox = settled();
        assert_eq!(up_with(PROXIED, &state, &sandbox), "Created");
        let created = sandbox.created.borrow();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].egress, Egress::HostPort(17323));
        assert_eq!(
            created[0].volumes,
            ["hangar-bay-work-data:/var/lib/pilot:40G"]
        );
        assert_eq!(created[0].entrypoint, None);
        assert!(
            sandbox.changes().contains(
                &"volume create hangar-bay-work-data 40G".to_string()
            )
        );
    }

    #[test]
    fn direct_bay_gets_open_egress_and_no_proxy_vars() {
        let state = scratch_dir("bay-up-direct");
        let sandbox = settled();
        assert_eq!(up_with(WORK_DIRECT, &state, &sandbox), "Created");
        assert_eq!(sandbox.created.borrow()[0].egress, Egress::Open);
        let env = stdin_of(&sandbox, "hangar-proxy-env");
        assert!(!env.contains("HTTPS_PROXY"), "{env}");
        assert!(env.contains("SSL_CERT_FILE"), "{env}");
    }

    #[test]
    fn unchanged_second_up_reports_unchanged_and_execs_no_restart() {
        let state = scratch_dir("bay-up-unchanged");
        let sandbox = settled();
        assert_eq!(up_with(PROXIED, &state, &sandbox), "Created");
        sandbox.reply(LAUNCH, "running");
        forget(&sandbox);
        assert_eq!(up_with(PROXIED, &state, &sandbox), "Unchanged");
        let changes = sandbox.changes();
        assert!(!changes.iter().any(|c| c.starts_with("create ")));
        assert!(!changes.iter().any(|c| c.contains("hangar-run-stop")));
    }

    #[test]
    fn image_change_recreates_and_keeps_home_and_volume() {
        let state = scratch_dir("bay-up-image");
        let sandbox = settled();
        let config = |image: &str| {
            WORK_DIRECT.replace(
                r#""proxy": null"#,
                &format!(r#""proxy": null, "image": "{image}""#),
            )
        };
        assert_eq!(up_with(&config("a:1"), &state, &sandbox), "Created");
        forget(&sandbox);
        assert_eq!(
            up_with(&config("a:2"), &state, &sandbox),
            r#"Recreated(["image"])"#
        );
        let changes = sandbox.changes();
        let stop = position(&changes, "stop hangar-bay-work");
        let remove = position(&changes, "remove hangar-bay-work");
        let create = position(&changes, "create hangar-bay-work");
        assert!(stop < remove && remove < create, "{changes:#?}");
        assert!(!changes.iter().any(|c| c.starts_with("volume rm")));
        assert!(state.join("bays/work/home").is_dir());
    }

    #[test]
    fn a_failed_recreate_leaves_no_record() {
        let state = scratch_dir("bay-up-recreate-fails");
        let sandbox = settled();
        assert_eq!(up_with(WORK_DIRECT, &state, &sandbox), "Created");
        let bigger = WORK_DIRECT
            .replace(r#""proxy": null"#, r#""proxy": null, "cpus": 8"#);
        sandbox.fail_create.set(true);
        assert!(up_with(&bigger, &state, &sandbox).starts_with("Failed("));
        assert!(!state.join("bays/work/vm").exists());
        sandbox.fail_create.set(false);
        assert_eq!(
            up_with(&bigger, &state, &sandbox),
            r#"Recreated(["no record"])"#
        );
    }

    #[test]
    fn disk_change_replaces_the_data_volume() {
        let state = scratch_dir("bay-up-disk");
        let sandbox = settled();
        assert_eq!(up_with(WORK_DIRECT, &state, &sandbox), "Created");
        forget(&sandbox);
        let bigger = WORK_DIRECT
            .replace(r#""proxy": null"#, r#""proxy": null, "disk": "60G""#);
        assert_eq!(
            up_with(&bigger, &state, &sandbox),
            r#"Recreated(["resources", "data disk replaced"])"#
        );
        let changes = sandbox.changes();
        let rm = position(&changes, "volume rm hangar-bay-work-data");
        let create = position(&changes, "create hangar-bay-work");
        assert!(rm < create, "{changes:#?}");
    }

    #[test]
    fn proxy_switch_recreates_with_reason_proxy() {
        let state = scratch_dir("bay-up-proxy-switch");
        let sandbox = settled();
        assert_eq!(up_with(PROXIED, &state, &sandbox), "Created");
        let direct = PROXIED.replace(r#""proxy": "vault""#, r#""proxy": null"#);
        assert_eq!(
            up_with(&direct, &state, &sandbox),
            r#"Recreated(["proxy"])"#
        );
    }

    #[test]
    fn vm_without_record_is_recreated_not_used() {
        let state = scratch_dir("bay-up-no-record");
        let sandbox = settled();
        sandbox.set(WORK_VM, BoxState::Running);
        assert_eq!(
            up_with(WORK_DIRECT, &state, &sandbox),
            r#"Recreated(["no record"])"#
        );
        assert_eq!(sandbox.changes()[0], "stop hangar-bay-work");
    }

    #[test]
    fn a_stopped_bay_is_started_not_recreated() {
        let state = scratch_dir("bay-up-start");
        let sandbox = settled();
        assert_eq!(up_with(WORK_DIRECT, &state, &sandbox), "Created");
        sandbox.set(WORK_VM, BoxState::Stopped);
        forget(&sandbox);
        assert_eq!(
            up_with(WORK_DIRECT, &state, &sandbox),
            r#"Updated(["started"])"#
        );
        assert_eq!(sandbox.changes()[0], "start hangar-bay-work");
    }

    #[test]
    fn package_added_and_removed_in_place() {
        let state = scratch_dir("bay-up-packages");
        let sandbox = settled();
        let config = WORK_DIRECT.replace(
            r#""proxy": null"#,
            r#""proxy": null, "packages": ["nixpkgs#rtk"]"#,
        );
        assert_eq!(up_with(&config, &state, &sandbox), "Created");
        forget(&sandbox);
        sandbox.reply_once("hangar-packages /", Some(r#"{"nixpkgs#jq":"jq"}"#));
        for profile in [
            r#"{"elements":{"jq":{}}}"#,
            r#"{"elements":{}}"#,
            r#"{"elements":{"rtk":{}}}"#,
        ] {
            sandbox.reply_once("hangar-profile", Some(profile));
        }
        assert_eq!(
            up_with(&config, &state, &sandbox),
            r#"Updated(["packages +rtk -jq"])"#
        );
        assert!(!sandbox.changes().iter().any(|c| c.starts_with("create ")));
    }

    #[test]
    fn changed_login_rewrites_proxy_env_and_restarts_apps() {
        let state = scratch_dir("bay-up-login");
        let sandbox = settled();
        assert_eq!(up_with(PROXIED, &state, &sandbox), "Created");
        forget(&sandbox);
        sandbox.reply_once("hangar-proxy-env", Some("changed\n"));
        sandbox.reply_once(LAUNCH, Some("restarted"));
        let renewed = PROXIED.replace("pw-1", "pw-2");
        assert_eq!(
            up_with(&renewed, &state, &sandbox),
            r#"Updated(["proxy login", "restarted paperclip"])"#
        );
        assert!(stdin_of(&sandbox, "hangar-proxy-env").contains("pw-2"));
        assert!(!stdin_of(&sandbox, LAUNCH).contains("pw-2"));
        assert!(!sandbox.changes().iter().any(|c| c == RESTART_DAEMONS));
        assert!(sandbox.changes().iter().all(|call| !call.contains("pw-2")));
    }

    #[test]
    fn nothing_login_derived_reaches_argv() {
        let renewed = PROXIED.replace("pw-1", "pw-2");
        assert_eq!(first_up_calls(PROXIED), first_up_calls(&renewed));
    }

    #[test]
    fn a_new_ca_alone_restarts_the_daemons_and_is_reported_as_proxy_ca() {
        let state = scratch_dir("bay-up-ca");
        let sandbox = settled();
        assert_eq!(up_with(PROXIED, &state, &sandbox), "Created");
        let env = stdin_of(&sandbox, "hangar-proxy-env");
        forget(&sandbox);
        let pem = include_str!("proxy/testdata/ca.pem").replace('\n', "\\n");
        let with_ca =
            PROXIED.replace(r#""port""#, &format!(r#""ca": "{pem}", "port""#));
        sandbox.reply_once(" hangar-ca ", Some("changed\n"));
        sandbox.reply_once(LAUNCH, Some("restarted"));
        assert_eq!(
            up_with(&with_ca, &state, &sandbox),
            r#"Updated(["proxy CA", "restarted paperclip"])"#
        );
        assert!(stdin_of(&sandbox, " hangar-ca ").contains("BEGIN CERT"));
        assert_eq!(stdin_of(&sandbox, "hangar-proxy-env"), env);
        let changes = sandbox.changes();
        let restart = position(&changes, RESTART_DAEMONS);
        assert!(restart > position_of(&changes, "hangar-proxy-env"));
        assert!(restart < position_of(&changes, "hangar-docker"));
    }

    #[test]
    fn removed_app_unit_is_stopped() {
        let state = scratch_dir("bay-up-stray");
        let sandbox = settled();
        assert_eq!(up_with(WORK_DIRECT, &state, &sandbox), "Created");
        forget(&sandbox);
        sandbox.reply(
            "hangar-runs",
            "hangar-run-old.service loaded active running hangar run old 1\n",
        );
        assert_eq!(
            up_with(WORK_DIRECT, &state, &sandbox),
            r#"Updated(["stopped old"])"#
        );
        assert!(
            sandbox
                .changes()
                .iter()
                .any(|c| c.ends_with("hangar-run-stop hangar-run-old.service"))
        );
    }

    #[test]
    fn failing_step_names_the_step() {
        let state = scratch_dir("bay-up-fails");
        let sandbox = settled();
        sandbox.reply_once("hangar-proxy-env", None);
        let hangar = hangar_with(WORK_DIRECT, &state, sandbox.clone());
        let Outcome::Failed(error) = up(&hangar, &hangar.bay("work").unwrap())
        else {
            panic!("expected a failure");
        };
        assert!(
            error.message().starts_with("bay work: proxy env:"),
            "{error}"
        );
    }

    #[test]
    fn proxy_url_only_on_stdin() {
        let state = scratch_dir("bay-up-secret");
        let sandbox = settled();
        assert_eq!(up_with(PROXIED, &state, &sandbox), "Created");
        assert!(sandbox.calls.borrow().iter().all(|c| !c.contains("pw-1")));
        assert!(
            stdin_of(&sandbox, "hangar-proxy-env")
                .contains("HTTPS_PROXY='http://agent:pw-1@host.fake:17323'")
        );
    }

    #[test]
    fn down_stops_only_a_running_bay() {
        let state = scratch_dir("bay-down");
        let sandbox = fake(&[(WORK_VM, BoxState::Running)]);
        let hangar = hangar_with(WORK_DIRECT, &state, sandbox.clone());
        let bay = hangar.bay("work").unwrap();
        assert_eq!(format!("{:?}", down(&hangar, &bay).unwrap()), "Stopped");
        assert_eq!(format!("{:?}", down(&hangar, &bay).unwrap()), "Unchanged");
        assert_eq!(sandbox.changes(), ["stop hangar-bay-work"]);
    }

    #[test]
    fn a_bay_is_booted_once_systemd_runs_even_degraded() {
        let said = |text: &str| boot_state(Ok(text.as_bytes().to_vec()));
        assert_eq!(said("running\n"), Probe::Done);
        assert_eq!(
            said("stopping\n"),
            Probe::Failed(r#"systemd is "stopping""#.into())
        );
        assert_eq!(said("degraded\n"), Probe::Done);
        assert_eq!(
            said("offline\n"),
            Probe::Pending(r#"systemd is "offline""#.into())
        );
        let unknown_root = Error::new("failed to resolve guest uid 0");
        assert_eq!(
            boot_state(Err(unknown_root)),
            Probe::Pending("failed to resolve guest uid 0".into())
        );
    }

    #[test]
    fn a_bay_that_never_finishes_booting_fails_with_its_last_state() {
        let sandbox = fake(&[(BAY_VM, BoxState::Running)]);
        sandbox.reply("hangar-booted", "starting\n");
        let state = scratch_dir("bay-booting");
        let hangar = hangar_with(DIRECT, &state, sandbox.clone());
        let bay = hangar.bay("default").unwrap();
        let error =
            wait_booted(sandbox.as_ref(), &bay, Duration::ZERO, Duration::ZERO)
                .unwrap_err()
                .to_string();
        assert_eq!(
            error,
            "hangar-bay-default did not finish booting: systemd is \
             \"starting\""
        );
        assert_eq!(sandbox.changes().len(), 1);
    }

    #[test]
    fn a_booting_bay_is_retried_until_systemd_runs() {
        let sandbox = fake(&[(BAY_VM, BoxState::Running)]);
        sandbox.reply_once("hangar-booted", None);
        sandbox.reply_once("hangar-booted", Some("offline\n"));
        sandbox.reply_once("hangar-booted", Some("running\n"));
        let state = scratch_dir("bay-boot-retry");
        let hangar = hangar_with(DIRECT, &state, sandbox.clone());
        let bay = hangar.bay("default").unwrap();
        wait_booted(sandbox.as_ref(), &bay, WAIT_TIMEOUT, Duration::ZERO)
            .unwrap();
        assert_eq!(sandbox.changes().len(), 3);
    }

    #[test]
    fn a_bay_that_is_shutting_down_or_in_maintenance_fails_at_once() {
        for said in ["maintenance", "stopping"] {
            let sandbox = fake(&[(BAY_VM, BoxState::Running)]);
            sandbox.reply("hangar-booted", said);
            let state = scratch_dir("bay-boot-fails");
            let hangar = hangar_with(DIRECT, &state, sandbox.clone());
            let bay = hangar.bay("default").unwrap();
            let error = wait_booted(
                sandbox.as_ref(),
                &bay,
                WAIT_TIMEOUT,
                Duration::ZERO,
            )
            .unwrap_err()
            .to_string();
            assert_eq!(
                error,
                format!(
                    "hangar-bay-default can't finish booting: systemd is \
                     {said:?}"
                )
            );
            assert_eq!(sandbox.changes().len(), 1);
        }
    }

    #[test]
    fn a_built_image_is_loaded_once() {
        let state = scratch_dir("bay-image-loader");
        let config = r#"{"bays": [{"name": "default", "proxy": null,
            "image": "hangar-bay:dev", "imageLoader": "true"}]}"#;
        let sandbox = fake(&[]);
        let hangar = hangar_with(config, &state, sandbox.clone());
        let bay = hangar.bay("default").unwrap();
        ensure_image(&hangar, &bay, "hangar-bay:dev").unwrap();
        ensure_image(&hangar, &bay, "hangar-bay:dev").unwrap();
        assert_eq!(sandbox.changes(), ["load hangar-bay:dev"]);
    }

    #[test]
    fn the_proxy_env_names_the_proxy_kind_and_the_bundle_not_the_ca() {
        let access = |ca: &[u8]| Access {
            url: Some(Secret::new("http://a'b@host:1".into())),
            ca_pem: Some(ca.to_vec()),
        };
        let env = proxy_env(Some("fake"), &access(b"CA-1"));
        assert_eq!(
            env.expose(),
            "# proxy fake\n\
                 CURL_CA_BUNDLE='/var/lib/hangar/ca-bundle.crt'\n\
                 GIT_SSL_CAINFO='/var/lib/hangar/ca-bundle.crt'\n\
                 HTTPS_PROXY='http://a'\\''b@host:1'\n\
                 HTTP_PROXY='http://a'\\''b@host:1'\n\
                 NIX_SSL_CERT_FILE='/var/lib/hangar/ca-bundle.crt'\n\
                 NODE_EXTRA_CA_CERTS='/var/lib/hangar/ca-bundle.crt'\n\
                 NO_PROXY='localhost,127.0.0.1'\n\
                 SSL_CERT_FILE='/var/lib/hangar/ca-bundle.crt'\n\
                 http_proxy='http://a'\\''b@host:1'\n\
                 https_proxy='http://a'\\''b@host:1'\n\
                 no_proxy='localhost,127.0.0.1'\n"
        );
        let rotated = proxy_env(Some("fake"), &access(b"CA-2"));
        assert_eq!(env.expose(), rotated.expose());
    }

    #[test]
    fn a_file_is_replaced_whole_and_only_when_it_changed() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = scratch_dir("replace");
        let file = dir.join("proxy.env");
        let meta = fs::metadata(&dir).unwrap();
        let owner = format!("{}:{}", meta.uid(), meta.gid());
        let script = replace_script(r#"printf %s "$4""#);
        let replace = |content: &str| {
            let output = std::process::Command::new("/bin/sh")
                .args(["-c", &script, "sh"])
                .arg(&file)
                .args([&owner, "0640", content])
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap()
        };
        let inode = || fs::metadata(&file).unwrap().ino();
        assert_eq!(replace("A=1\n"), "changed\n");
        let first = inode();
        assert_eq!(fs::read_to_string(&file).unwrap(), "A=1\n");
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(replace("A=1\n"), "same\n");
        assert_eq!(inode(), first);
        assert_eq!(replace("A=2\n"), "changed\n");
        assert_ne!(inode(), first);
        assert_eq!(fs::read_to_string(&file).unwrap(), "A=2\n");
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            1,
            "a tmp file was left behind"
        );
    }

    const FAKE_SYSTEMCTL: &str = r#"#!/bin/sh
d=$(dirname "$0")
case "$2" in
is-active) cat "$d/active" 2>/dev/null || echo inactive ;;
show) cat "$d/description" ;;
stop) echo inactive >"$d/active" ;;
esac"#;
    const FAKE_SYSTEMD_RUN: &str = r#"#!/bin/sh
d=$(dirname "$0")
for arg; do
  case "$arg" in --description=*) echo "${arg#*=}" >"$d/description" ;; esac
done
echo active >"$d/active""#;

    #[test]
    fn a_run_restarts_in_the_guest_when_the_proxy_env_or_ca_change() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch_dir("start-run");
        for (name, script) in [
            ("systemctl", FAKE_SYSTEMCTL),
            ("systemd-run", FAKE_SYSTEMD_RUN),
        ] {
            let path = dir.join(name);
            fs::write(&path, script).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let (env, ca) = (dir.join("proxy.env"), dir.join("ca.crt"));
        fs::write(&env, "login-1").unwrap();
        fs::write(&ca, "ca-1").unwrap();
        let path =
            format!("{}:{}", dir.display(), std::env::var("PATH").unwrap());
        let launch = || {
            let output = std::process::Command::new("/bin/sh")
                .env("PATH", &path)
                .args([
                    "-c",
                    START_RUN,
                    "hangar-run",
                    "u",
                    "cmd",
                    "hangar run x 1",
                ])
                .args([&env, &ca])
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        assert_eq!(launch(), "started");
        assert_eq!(launch(), "running");
        fs::write(&env, "login-2").unwrap();
        assert_eq!(launch(), "restarted");
        assert_eq!(launch(), "running");
        fs::write(&ca, "ca-2").unwrap();
        assert_eq!(launch(), "restarted");
        let description = fs::read_to_string(dir.join("description")).unwrap();
        assert!(description.starts_with("hangar run x 1 "), "{description}");
        assert!(!description.contains("login"), "{description}");
    }

    #[test]
    fn docker_is_waited_for_until_it_answers() {
        let sandbox = fake(&[(BAY_VM, BoxState::Running)]);
        sandbox.reply_once("hangar-docker", None);
        let state = scratch_dir("bay-docker");
        let hangar = hangar_with(DIRECT, &state, sandbox.clone());
        let bay = hangar.bay("default").unwrap();
        wait_for_docker(sandbox.as_ref(), &bay, WAIT_TIMEOUT, Duration::ZERO)
            .unwrap();
        assert_eq!(sandbox.changes().len(), 2);

        sandbox.reply_once("hangar-docker", None);
        let error = wait_for_docker(
            sandbox.as_ref(),
            &bay,
            Duration::ZERO,
            Duration::ZERO,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.starts_with("dockerd in hangar-bay-default did not start: "),
            "{error}"
        );
        assert!(
            error.ends_with(
                "in 'hangar exec default', run 'systemctl --user \
                 reset-failed docker && systemctl --user restart docker'"
            ),
            "{error}"
        );
    }

    fn started(sandbox: &FakeSandbox) -> Vec<String> {
        sandbox
            .changes()
            .iter()
            .filter(|call| call.contains(" hangar-run "))
            .map(|call| {
                let entry = call.rsplit(" hangar-run ").next().unwrap();
                entry.split(" hangar run ").next().unwrap().into()
            })
            .collect()
    }

    #[test]
    fn an_app_that_needs_setup_is_not_started() {
        let state = scratch_dir("setup-gate");
        let config = r#"{"apps": {"web": {
            "setup": {"command": "web init", "check": "test -f ~/.web"},
            "run": "web serve"}},
            "bays": [{"name": "default", "proxy": null, "apps": ["web"]}]}"#;
        for (check, expected) in [("needed", vec![]), ("done", vec!["web"])] {
            let sandbox = Rc::new(FakeSandbox::default());
            sandbox.reply("hangar-setup-check", check);
            sandbox.reply(" hangar-run ", "started");
            let hangar = hangar_with(config, &state, sandbox.clone());
            let bay = hangar.bay("default").unwrap();
            let needing = check_setup(&hangar, &bay).unwrap();
            let changes = sync_apps(&hangar, &bay, &needing).unwrap();
            assert_eq!(changes.len(), expected.len());
            let expected: Vec<String> = expected
                .iter()
                .map(|name| format!("hangar-run-{name}.service {name} serve"))
                .collect();
            assert_eq!(started(&sandbox), expected);
        }
    }

    #[test]
    fn the_env_file_is_replaced_whole_by_root() {
        let state = scratch_dir("bay-env-atomic");
        let sandbox = Rc::new(FakeSandbox::default());
        sandbox.reply("umask 077", "changed\n");
        let hangar = hangar_with(DIRECT, &state, sandbox.clone());
        assert!(write_env(&hangar, &hangar.bay("default").unwrap()).unwrap());
        let script = replace_script("cat");
        assert_eq!(
            sandbox.changes(),
            [format!(
                "exec root@{BAY_VM} /bin/sh -c {script} hangar-env \
                 /etc/hangar/bay.env root:root 0644"
            )]
        );
    }
}
