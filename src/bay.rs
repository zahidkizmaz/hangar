//! A bay: a locked-down VM that AI coding agents run in. Its only way
//! out is the tower's proxy.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use crate::broker;
use crate::config::{BaySettings, Settings, referenced_credentials};
use crate::error::{Context, Error, Result, bail};
use crate::hangar::Hangar;
use crate::mounts::{self, Resolved, Roots};
use crate::overview::RunState;
use crate::sandbox::{BoxState, Egress, Mount, MountMode, Sandbox, VmSpec};
use crate::state::{BayDir, fnv, read_hashes, write_hashes, write_private};
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
    /// The bay's own home and package cache folders, each only when it
    /// mounts it (`home`, `cache`).
    pub(crate) fn own_mounts(&self) -> (Option<String>, Option<String>) {
        let mounted =
            |on: bool, path: &Path| on.then(|| path.display().to_string());
        (
            mounted(self.settings.home, &self.dir.home()),
            mounted(self.settings.cache, &self.cache),
        )
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
/// Where `guest/` (the tower's CA) is mounted, read-only.
pub(crate) const GUEST_MOUNT: &str = "/run/hangar";
/// The image's systemd, which the VM boots into.
const INIT: &str = "/sbin/init";
const HANGAR_START: &str = "/run/current-system/sw/bin/hangar-start";
/// Pilot's data on the VM disk, Docker's included.
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

fn recreate(bay: &str) -> String {
    format!("run 'hangar destroy {bay} && hangar up {bay}'")
}

/// ` --bay NAME` for hints, once there's more than one bay to choose from.
pub(crate) fn flag(hangar: &Hangar, bay: &Bay) -> String {
    if hangar.settings.bays.len() > 1 {
        format!(" --bay {}", bay.name)
    } else {
        String::new()
    }
}

/// The bay's first step: an existing VM must have been created with
/// egress to the tower's proxy port, and with mounts today's rules still
/// allow (a root may have moved since), or the bay isn't touched.
pub(crate) fn preflight(hangar: &Hangar, bay: &Bay) -> Result<()> {
    let state = hangar.sandbox.state(&bay.vm)?;
    let recorded = VmRecord::load(&bay.dir.vm());
    let proxy = broker::proxy_port(hangar.broker.as_ref())?;
    let fix = format!("{} (home is kept)", recreate(bay.name));
    check_egress(state, recorded.as_ref(), proxy)
        .map_err(|message| Error::with_hint(message, &fix))?;
    if let Some(recorded) = recorded.filter(|_| state != BoxState::Missing) {
        mounts::recheck(&recorded.mounts, &roots(hangar, bay)?)
            .map_err(|error| Error::with_hint(error.message(), &fix))?;
    }
    Ok(())
}

pub(crate) fn roots(hangar: &Hangar, bay: &Bay) -> Result<Roots> {
    Roots::new(
        &hangar.host_home,
        hangar.state.root(),
        &hangar.cache_root,
        &bay.dir.home(),
        &bay.cache,
    )
}

/// Egress is fixed when the VM is created, so a VM without a record, or
/// recorded with another port, has to be recreated.
fn check_egress(
    state: BoxState,
    recorded: Option<&VmRecord>,
    proxy_port: u16,
) -> std::result::Result<(), String> {
    if state == BoxState::Missing {
        return Ok(());
    }
    match recorded.and_then(|record| record.egress) {
        Some(port) if port == proxy_port => Ok(()),
        Some(port) => {
            Err(format!("bay was created with egress to port {port}"))
        }
        None => Err("bay has no record of how it was created".into()),
    }
}

/// The image, mounts and ports are fixed when the VM is created too, so an
/// existing VM created with others keeps them: `up` warns instead of
/// recreating it.
fn drift_warnings(
    bay: &str,
    vm: &str,
    recorded: &VmRecord,
    wanted: &VmRecord,
) -> Vec<String> {
    let recreate = recreate(bay);
    let mut warnings = Vec::new();
    if let (Some(old), Some(new)) = (&recorded.image, &wanted.image)
        && old != new
    {
        warnings
            .push(format!("{vm} uses {old}; {recreate} to switch to {new}"));
    }
    if recorded.mounts != wanted.mounts {
        warnings.push(format!(
            "{vm} was created with different mounts; {recreate} to apply"
        ));
    }
    if recorded.ports != wanted.ports {
        warnings.push(format!(
            "{vm} was created with different ports; {recreate} to apply"
        ));
    }
    warnings
}

pub(crate) fn ensure_vm(hangar: &Hangar, bay: &Bay) -> Result<()> {
    // Checked before anything is created; resolved, so symlinks can't
    // hide where a mount points.
    let (home, cache) = bay.own_mounts();
    let all = mounts::with_hangar(
        &bay.settings.mounts,
        home.as_ref(),
        cache.as_ref(),
    );
    let resolved = mounts::resolve(&all, &roots(hangar, bay)?)?;
    reconcile_vm(hangar, bay, &resolved)
}

fn wanted_record(
    bay: &Bay,
    image: &str,
    egress: u16,
    mounts: Vec<String>,
) -> VmRecord {
    let ports = &bay.settings.ports;
    VmRecord {
        image: Some(image.to_string()),
        egress: Some(egress),
        ports: ports
            .iter()
            .map(|port| (port.name.clone(), port.host, port.vm))
            .collect(),
        mounts,
    }
}

fn reconcile_vm(
    hangar: &Hangar,
    bay: &Bay,
    resolved: &[Resolved],
) -> Result<()> {
    let image = bay.settings.image_ref();
    let proxy = broker::proxy_port(hangar.broker.as_ref())?;
    let wanted = wanted_record(bay, &image, proxy, mounts::lines(resolved));
    let state = hangar.sandbox.state(&bay.vm)?;
    if state != BoxState::Missing
        && let Some(recorded) = VmRecord::load(&bay.dir.vm())
    {
        for warning in drift_warnings(bay.name, &bay.vm, &recorded, &wanted) {
            warn!("{warning}");
        }
    }
    match state {
        BoxState::Missing => {
            ensure_image(hangar, bay, &image)?;
            info!("creating {}", bay.vm);
            let created =
                create_vm(hangar, bay, &image, proxy, &wanted, resolved);
            // A VM a failing create still left behind is recorded too, so
            // the next `up` can account for it.
            if created.is_err()
                && hangar.sandbox.state(&bay.vm)? == BoxState::Missing
            {
                return created;
            }
            // A new VM has none of the files the record says were copied.
            let copied = bay.dir.files();
            if copied.exists() {
                fs::remove_file(&copied).context(copied.display())?;
            }
            write_private(&bay.dir.vm(), wanted.render().as_bytes())?;
            created
        }
        BoxState::Stopped => {
            info!("starting {}", bay.vm);
            hangar.sandbox.start(&bay.vm)
        }
        BoxState::Running => Ok(()),
    }
}

/// Once the VM has booted, the tower's CA goes to `guest/`, then
/// `hangar-start` sets up CA trust and the daemons' proxy env inside the
/// VM. The proxy URL can carry a token, so it goes on stdin, never into
/// `guest/` or argv.
pub(crate) fn start(hangar: &Hangar, bay: &Bay) -> Result<()> {
    wait_booted(hangar.sandbox.as_ref(), bay, BOOT_TIMEOUT, BOOT_PAUSE)?;
    let access = broker::access(hangar, bay.name)?;
    let ca = hangar.state.ca();
    fs::write(&ca, &access.ca_pem).context(ca.display())?;
    if access.renewed {
        warn!("{}", renewed_warning(bay.name, &flag(hangar, bay)));
    }
    info!("running hangar-start in {}", bay.vm);
    let url = format!("{}\n", access.proxy_url.expose());
    hangar
        .sandbox
        .exec(&bay.vm, Some(ROOT), &[HANGAR_START], Some(url.as_bytes()))
        .map(drop)
}

/// `hangar-start` restarts the daemons when the proxy URL changes; `up`
/// never restarts the run entries.
fn renewed_warning(bay: &str, flag: &str) -> String {
    format!(
        "bay {bay}: its broker token was renewed; run 'hangar restart{flag}' \
         so its run entries use it"
    )
}

/// systemd's state once it can tell.
const BOOTED: &str = "/run/current-system/sw/bin/systemctl is-system-running --wait \
     2>/dev/null || :";
const BOOT_PAUSE: Duration = Duration::from_millis(500);
const BOOT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, PartialEq, Eq)]
enum Boot {
    Done,
    /// Still booting, with what it last said.
    Pending(String),
    /// Shutting down or stuck in maintenance: waiting won't help.
    Failed(String),
}

/// Root's exec fails until activation wrote passwd, and systemd says
/// `offline` until it's up; `degraded` is booted too.
fn boot_state(output: Result<Vec<u8>>) -> Boot {
    match output {
        Err(error) => Boot::Pending(error.to_string()),
        Ok(output) => match String::from_utf8_lossy(&output).trim() {
            "running" | "degraded" => Boot::Done,
            other @ ("maintenance" | "stopping") => {
                Boot::Failed(format!("systemd is {other:?}"))
            }
            other => Boot::Pending(format!("systemd is {other:?}")),
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
    let deadline = Instant::now() + timeout;
    loop {
        match boot_state(sandbox.exec(&bay.vm, Some(ROOT), &booted, None)) {
            Boot::Done => return Ok(()),
            Boot::Failed(state) => {
                bail!("{} can't finish booting: {state}", bay.vm)
            }
            Boot::Pending(state) if Instant::now() >= deadline => {
                bail!("{} did not finish booting: {state}", bay.vm)
            }
            Boot::Pending(_) => thread::sleep(pause),
        }
    }
}

pub(crate) fn reconcile_packages(hangar: &Hangar, bay: &Bay) -> Result<()> {
    packages::reconcile(
        hangar.sandbox.as_ref(),
        &bay.vm,
        &bay.settings.packages,
        bay.settings.cache,
    )
}

/// The bay's env, `KEY='value'` lines every login shell sources with
/// `set -a` (`nix/bay/configuration.nix`), like `PROXY_ENV_FILE`.
pub(crate) const ENV_FILE: &str = "/etc/hangar/bay.env";
/// The proxy and CA variables `hangar-start` writes (guest/start.sh).
pub(crate) const PROXY_ENV_FILE: &str = "/etc/hangar/proxy.env";
const PLACEHOLDER: &str = "hangar-placeholder";

/// A placeholder per injected credential; `env` wins for a name, for
/// tools that want a specific placeholder format.
fn vm_env(settings: &Settings, bay: &BaySettings) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = referenced_credentials(settings)
        .into_iter()
        .map(|name| (name, PLACEHOLDER.to_string()))
        .collect();
    env.extend(bay.env.clone());
    env
}

pub(crate) fn write_env(hangar: &Hangar, bay: &Bay) -> Result<()> {
    // A login shell never sources half a file.
    let tmp = format!("{ENV_FILE}.tmp");
    let script = format!("cat >{tmp} && mv {tmp} {ENV_FILE}");
    let rendered = render_env(&vm_env(&hangar.settings, bay.settings));
    hangar
        .sandbox
        .exec(
            &bay.vm,
            Some(ROOT),
            &["/bin/sh", "-c", &script, "hangar-env"],
            Some(rendered.as_bytes()),
        )
        .map(drop)
}

fn render_env(env: &BTreeMap<String, String>) -> String {
    env.iter().fold(String::new(), |mut out, (key, value)| {
        let value = value.replace('\'', r"'\''");
        let _ = writeln!(out, "{key}='{value}'");
        out
    })
}

// A run counts as running only if its PID still carries the marker, so a
// PID reused after the entry exited isn't mistaken for it.
const RUNNING: &str = r#"pid_file=/run/hangar-run/$1.pid
running() {
  [ -f "$pid_file" ] || return 1
  tr '\0' '\n' 2>/dev/null <"/proc/$(cat "$pid_file")/environ" |
    grep -qx "HANGAR_RUN=$1"
}
"#;

const STOP_RUN: &str = r#"if running; then
  pid=$(cat "$pid_file")
  kill -TERM -- "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null
  i=0
  while running && [ "$i" -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
  running && { kill -KILL -- "-$pid" 2>/dev/null || kill -KILL "$pid"; }
  rm -f "$pid_file"
fi
"#;

const START_RUN: &str = r#"running && exit 0
HANGAR_RUN=$1 setsid sh -lc "$2" >>"/var/log/hangar/$1.log" 2>&1 </dev/null &
echo $! >"$pid_file"
echo started"#;

const RUN_STATUS: &str =
    r"if running; then echo running; else echo stopped; fi";

/// What `up` does about an entry: `up` never restarts anything.
#[derive(Debug, PartialEq, Eq)]
enum RunDecision {
    /// Just started, or nothing recorded yet: record it.
    Record,
    Warn,
    Nothing,
}

fn run_decision(
    started: bool,
    recorded: Option<&str>,
    current: &str,
) -> RunDecision {
    match recorded {
        _ if started => RunDecision::Record,
        None => RunDecision::Record,
        Some(recorded) if recorded != current => RunDecision::Warn,
        Some(_) => RunDecision::Nothing,
    }
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

/// The enabled apps whose setup check fails. No record is kept: the check
/// is the truth, so a wiped home asks for setup again.
fn needing_setup(hangar: &Hangar, bay: &Bay) -> Result<Vec<String>> {
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

pub(crate) fn check_setup(hangar: &Hangar, bay: &Bay) -> Result<()> {
    for name in needing_setup(hangar, bay)? {
        warn!(
            "{name} needs setup: run 'hangar setup{} {name}'",
            flag(hangar, bay)
        );
    }
    Ok(())
}

pub(crate) fn start_runs(hangar: &Hangar, bay: &Bay) -> Result<()> {
    let needing_setup = needing_setup(hangar, bay)?;
    let mut fingerprints = Fingerprints::load(bay);
    for (name, command) in &bay.settings.run {
        if needing_setup.contains(name) {
            continue;
        }
        let started = launch(hangar, bay, name, command, false)?;
        let current = Fingerprints::current(hangar, bay, command);
        match run_decision(started, fingerprints.get(name), &current) {
            RunDecision::Record => fingerprints.set(name, current)?,
            RunDecision::Warn => warn!(
                "{name}'s inputs changed; run 'hangar restart{} {name}' to \
                 apply",
                flag(hangar, bay)
            ),
            RunDecision::Nothing => {}
        }
    }
    Ok(())
}

pub(crate) fn restart(
    hangar: &Hangar,
    bay: &Bay,
    names: &[String],
    copy_files: bool,
) -> Result<Vec<String>> {
    let run = &bay.settings.run;
    let names: Vec<String> = if names.is_empty() {
        run.keys().cloned().collect()
    } else {
        names.to_vec()
    };
    if let Some(unknown) = names.iter().find(|name| !run.contains_key(*name)) {
        let known = run.keys().cloned().collect::<Vec<_>>().join(", ");
        let hint = if known.is_empty() {
            "run is empty".to_string()
        } else {
            format!("run has: {known}")
        };
        return Err(Error::with_hint(
            format!("{unknown} is not a run entry or app"),
            hint,
        ));
    }
    if copy_files {
        files::copy_declared(hangar, bay)?;
    }
    write_env(hangar, bay)?;
    let mut fingerprints = Fingerprints::load(bay);
    for name in &names {
        let command = &run[name];
        info!("restarting {name}");
        launch(hangar, bay, name, command, true)?;
        fingerprints.set(name, Fingerprints::current(hangar, bay, command))?;
    }
    Ok(names)
}

/// The one place an entry's process is (re)started; `restart` stops it
/// first. True when it was started.
fn launch(
    hangar: &Hangar,
    bay: &Bay,
    name: &str,
    command: &str,
    stop_first: bool,
) -> Result<bool> {
    let stop = if stop_first { STOP_RUN } else { "" };
    let script = format!("{RUNNING}{stop}{START_RUN}");
    let output = hangar.sandbox.exec(
        &bay.vm,
        Some(PILOT),
        &["sh", "-c", &script, "hangar-run", name, command],
        None,
    )?;
    let started = String::from_utf8_lossy(&output).trim() == "started";
    if started && !stop_first {
        info!("started {name}: hangar logs {name}");
    }
    Ok(started)
}

/// What each `run` entry was last started with: a hash of its
/// command, the env file and the copied config files.
struct Fingerprints {
    path: std::path::PathBuf,
    recorded: BTreeMap<String, String>,
}

impl Fingerprints {
    fn load(bay: &Bay) -> Self {
        let path = bay.dir.run_fingerprints();
        let recorded = read_hashes(&path);
        Self { path, recorded }
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.recorded.get(name).map(String::as_str)
    }

    fn current(hangar: &Hangar, bay: &Bay, command: &str) -> String {
        let env = render_env(&vm_env(&hangar.settings, bay.settings));
        let inputs = [command, &env, &files::record_text(bay)].join("\0");
        fnv(inputs.into_bytes())
    }

    fn set(&mut self, name: &str, fingerprint: String) -> Result<()> {
        self.recorded.insert(name.to_string(), fingerprint);
        write_hashes(&self.path, &self.recorded)
    }
}

pub(crate) fn run_state(
    sandbox: &dyn Sandbox,
    vm: &str,
    name: &str,
) -> Result<RunState> {
    let script = format!("{RUNNING}{RUN_STATUS}");
    let output = sandbox.exec(
        vm,
        Some(PILOT),
        &["sh", "-c", &script, "hangar-run-status", name],
        None,
    )?;
    Ok(match String::from_utf8_lossy(&output).trim() {
        "running" => RunState::Running,
        "stopped" => RunState::Stopped,
        other => RunState::Unknown(format!("unexpected {other:?}")),
    })
}

pub(crate) fn log_file(name: &str) -> String {
    format!("/var/log/hangar/{name}.log")
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

fn create_vm(
    hangar: &Hangar,
    bay: &Bay,
    image: &str,
    proxy: u16,
    wanted: &VmRecord,
    user: &[Resolved],
) -> Result<()> {
    let settings = bay.settings;
    let guest = hangar.state.guest();
    fs::create_dir_all(&guest).context(guest.display())?;
    let mut mounts = vec![Mount {
        host: &guest,
        guest: GUEST_MOUNT,
        mode: MountMode::ReadOnly,
        owner: None,
    }];
    // The cache is root's, which installs the packages; the rest pilot's.
    let cache = settings.cache.then_some(VM_CACHE);
    mounts.extend(user.iter().map(|mount| Mount {
        host: &mount.host,
        guest: &mount.vm,
        mode: MountMode::writable(mount.writable),
        owner:
            (Some(mount.vm.as_str()) != cache).then_some((PILOT_ID, PILOT_ID)),
    }));
    let publish: Vec<(u16, u16)> = wanted
        .ports
        .iter()
        .map(|(_, host, vm)| (*host, *vm))
        .collect();
    hangar.sandbox.create(&VmSpec {
        name: &bay.vm,
        image,
        cpus: settings.cpus,
        memory: &settings.memory,
        disk: Some(&settings.disk),
        native_fs: &[PILOT_DATA],
        init: Some(INIT),
        egress: Egress::OnlyHostPort(proxy),
        publish: &publish,
        mounts: &mounts,
        env: &[],
    })
}

#[cfg(test)]
mod tests {
    use super::{
        BOOT_TIMEOUT, BOOTED, Boot, RunDecision, boot_state, check_egress,
        drift_warnings, preflight, reconcile_vm, renewed_warning, run_decision,
        start, start_runs, vm_env, wait_booted, write_env,
    };
    use crate::broker::fake::FakeBroker;
    use crate::error::Error;
    use crate::mounts::Resolved;
    use crate::sandbox::fake::FakeSandbox;
    use crate::sandbox::{BoxState, Egress, Sandbox, TOWER_VM};
    use crate::testing::{
        hangar_with, hangar_with_broker, scratch_dir, settings,
    };
    use crate::vm_record::VmRecord;
    use std::fs;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::time::Duration;

    #[test]
    fn a_run_whose_process_is_gone_is_not_running_and_prints_nothing() {
        let dir = scratch_dir("run-gone");
        let pid_file = dir.join("app.pid");
        fs::write(&pid_file, "999999999").unwrap();
        let script = format!(
            "{}pid_file={}\nrunning",
            super::RUNNING,
            pid_file.display()
        );
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", &script, "sh", "app"])
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    }

    const RECREATE: &str =
        ": run 'hangar destroy default && hangar up default' (home is kept)";
    const BAY_VM: &str = "hangar-bay-default";

    fn record(egress: u16) -> VmRecord {
        VmRecord {
            image: Some("example/agent:1".into()),
            egress: Some(egress),
            ports: vec![("paperclip".into(), 3100, 3100)],
            mounts: vec![
                "/home/pilot\t/home/you/.local/share/hangar/home\trw".into(),
            ],
        }
    }

    #[test]
    fn an_existing_vm_created_with_other_inputs_is_reported_not_recreated() {
        assert_eq!(
            drift_warnings("default", BAY_VM, &record(1), &record(1)),
            Vec::<String>::new()
        );
        let mut wanted = record(1);
        wanted.image = Some("example/agent:2".into());
        wanted.mounts.clear();
        wanted.ports[0].1 = 3200;
        assert_eq!(
            drift_warnings("default", BAY_VM, &record(1), &wanted),
            [
                "hangar-bay-default uses example/agent:1; run 'hangar destroy \
                 default && hangar up default' to switch to example/agent:2",
                "hangar-bay-default was created with different mounts; run \
                 'hangar destroy default && hangar up default' to apply",
                "hangar-bay-default was created with different ports; run \
                 'hangar destroy default && hangar up default' to apply",
            ]
        );
        // An unreadable image line isn't a different image.
        let mut recorded = record(1);
        recorded.image = None;
        assert_eq!(
            drift_warnings("default", BAY_VM, &recorded, &record(1)),
            Vec::<String>::new()
        );
    }

    #[test]
    fn an_existing_vm_must_have_been_created_with_egress_to_the_proxy() {
        let error = |state, recorded: Option<&VmRecord>| {
            check_egress(state, recorded, 14322).unwrap_err()
        };
        for state in [BoxState::Running, BoxState::Stopped] {
            assert!(check_egress(state, Some(&record(14322)), 14322).is_ok());
            assert_eq!(
                error(state, Some(&record(15000))),
                "bay was created with egress to port 15000"
            );
            let unknown = "bay has no record of how it was created";
            assert_eq!(error(state, None), unknown);
            let mut no_egress = record(14322);
            no_egress.egress = None;
            assert_eq!(error(state, Some(&no_egress)), unknown);
        }
        // A missing VM is created with the current port.
        assert!(check_egress(BoxState::Missing, None, 14322).is_ok());
        assert!(check_egress(BoxState::Missing, Some(&record(1)), 2).is_ok());
    }

    fn fake(vms: &[(&str, BoxState)]) -> Rc<FakeSandbox> {
        Rc::new(FakeSandbox::with(vms))
    }

    const CONFIG: &str = r#"{"bays": [{"name": "default",
        "image": "example/agent:1",
        "apps": ["nix", "github", "claude-code", "paperclip"]}]}"#;

    fn write_record(state: &std::path::Path, record: &VmRecord) {
        let file = state.join("bays/default/vm");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, record.render()).unwrap();
    }

    #[test]
    fn refusals_leave_both_vms_untouched() {
        let running =
            [(BAY_VM, BoxState::Running), (TOWER_VM, BoxState::Stopped)];
        let state = scratch_dir("preflight");
        let unmounted = |egress| VmRecord {
            mounts: Vec::new(),
            ..record(egress)
        };
        for recorded in [None, Some(unmounted(15000))] {
            if let Some(recorded) = &recorded {
                write_record(&state, recorded);
            }
            let sandbox = fake(&running);
            let hangar = hangar_with(CONFIG, &state, sandbox.clone());
            assert!(
                preflight(&hangar, &hangar.bay("default").unwrap()).is_err()
            );
            assert_eq!(sandbox.changes(), Vec::<String>::new());
        }
        write_record(&state, &unmounted(14322));
        let hangar = hangar_with(CONFIG, &state, fake(&running));
        assert!(preflight(&hangar, &hangar.bay("default").unwrap()).is_ok());
    }

    #[test]
    fn a_recorded_mount_that_todays_rules_refuse_stops_the_bay() {
        let state = scratch_dir("preflight-mounts");
        fs::create_dir_all(state.join("vault")).unwrap();
        let vault = fs::canonicalize(state.join("vault")).unwrap();
        let recorded = VmRecord {
            mounts: vec![format!("/data\t{}\tro", vault.display())],
            ..record(14322)
        };
        write_record(&state, &recorded);
        let sandbox = fake(&[(BAY_VM, BoxState::Running)]);
        let hangar = hangar_with(CONFIG, &state, sandbox.clone());
        let error = preflight(&hangar, &hangar.bay("default").unwrap())
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            format!(
                "the bay's mount at /data: {} holds hangar's own state{RECREATE}",
                vault.display()
            )
        );
        assert_eq!(sandbox.changes(), Vec::<String>::new());
    }

    #[test]
    fn a_renewed_token_names_what_restarts_the_run_entries() {
        assert_eq!(
            renewed_warning("work", " --bay work"),
            "bay work: its broker token was renewed; run 'hangar restart \
             --bay work' so its run entries use it"
        );
    }

    #[test]
    fn a_bay_is_booted_once_systemd_runs_even_degraded() {
        let said = |text: &str| boot_state(Ok(text.as_bytes().to_vec()));
        assert_eq!(said("running\n"), Boot::Done);
        assert_eq!(
            said("stopping\n"),
            Boot::Failed(r#"systemd is "stopping""#.into())
        );
        assert_eq!(said("degraded\n"), Boot::Done);
        assert_eq!(
            said("offline\n"),
            Boot::Pending(r#"systemd is "offline""#.into())
        );
        let unknown_root = Error::new("failed to resolve guest uid 0");
        assert_eq!(
            boot_state(Err(unknown_root)),
            Boot::Pending("failed to resolve guest uid 0".into())
        );
    }

    #[test]
    fn a_bay_that_never_finishes_booting_fails_with_its_last_state() {
        let sandbox = fake(&[(BAY_VM, BoxState::Running)]);
        sandbox.reply("hangar-booted", "starting\n");
        let state = scratch_dir("bay-booting");
        let hangar = hangar_with("{}", &state, sandbox.clone());
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
        let hangar = hangar_with("{}", &state, sandbox.clone());
        let bay = hangar.bay("default").unwrap();
        wait_booted(sandbox.as_ref(), &bay, BOOT_TIMEOUT, Duration::ZERO)
            .unwrap();
        assert_eq!(sandbox.changes().len(), 3);
    }

    #[test]
    fn a_bay_that_is_shutting_down_or_in_maintenance_fails_at_once() {
        for said in ["maintenance", "stopping"] {
            let sandbox = fake(&[(BAY_VM, BoxState::Running)]);
            sandbox.reply("hangar-booted", said);
            let state = scratch_dir("bay-boot-fails");
            let hangar = hangar_with("{}", &state, sandbox.clone());
            let bay = hangar.bay("default").unwrap();
            let error = wait_booted(
                sandbox.as_ref(),
                &bay,
                BOOT_TIMEOUT,
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
    fn pilot_owns_the_home_and_user_mounts_and_root_the_cache() {
        let state = scratch_dir("bay-owners");
        let sandbox = fake(&[]);
        let hangar = hangar_with(CONFIG, &state, sandbox.clone());
        let bay = hangar.bay("default").unwrap();
        let mount = |vm: &str, host: &str| Resolved {
            vm: vm.into(),
            host: PathBuf::from(host),
            writable: true,
        };
        let mounts = [
            mount("/home/pilot", "/h"),
            mount("/home/pilot/.app", "/a"),
            mount("/var/cache/hangar", "/c"),
        ];
        reconcile_vm(&hangar, &bay, &mounts).unwrap();
        let created = sandbox.created.borrow();
        assert_eq!(
            created[0].mounts[1..],
            [
                "/h:/home/pilot:ReadWrite:1000:1000",
                "/a:/home/pilot/.app:ReadWrite:1000:1000",
                "/c:/var/cache/hangar:ReadWrite",
            ]
        );
    }

    #[test]
    fn a_new_bay_reaches_only_the_proxy_and_is_recorded() {
        let state = scratch_dir("bay-create");
        fs::create_dir_all(state.join("bays/default")).unwrap();
        fs::write(state.join("bays/default/files"), "old").unwrap();
        let sandbox = fake(&[]);
        let hangar = hangar_with(CONFIG, &state, sandbox.clone());
        let bay = hangar.bay("default").unwrap();
        let home = || Resolved {
            vm: "/home/pilot".into(),
            host: PathBuf::from("/home/you/.local/share/hangar/home"),
            writable: true,
        };
        reconcile_vm(&hangar, &bay, &[home()]).unwrap();

        let created = sandbox.created.borrow();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].name, BAY_VM);
        assert_eq!(created[0].init.as_deref(), Some("/sbin/init"));
        assert_eq!(created[0].native_fs, ["/var/lib/pilot"]);
        assert_eq!(created[0].egress, Egress::OnlyHostPort(14322));
        assert_eq!(created[0].publish, [(3100, 3100)]);
        assert_eq!(
            created[0].mounts,
            [
                format!(
                    "{}:/run/hangar:ReadOnly",
                    state.join("guest").display()
                ),
                "/home/you/.local/share/hangar/home:/home/pilot:ReadWrite:\
                 1000:1000"
                    .into(),
            ]
        );
        drop(created);
        assert_eq!(
            VmRecord::load(&state.join("bays/default/vm")),
            Some(record(14322))
        );
        assert!(!state.join("bays/default/files").exists());

        // Existing: kept as it is, or started.
        reconcile_vm(&hangar, &bay, &[home()]).unwrap();
        assert_eq!(sandbox.changes(), ["create hangar-bay-default"]);
        let stopped = fake(&[(BAY_VM, BoxState::Stopped)]);
        let hangar = hangar_with(CONFIG, &state, stopped.clone());
        reconcile_vm(&hangar, &hangar.bay("default").unwrap(), &[home()])
            .unwrap();
        assert_eq!(stopped.changes(), ["start hangar-bay-default"]);
    }

    #[test]
    fn a_vm_that_a_failing_create_left_behind_is_still_recorded() {
        let state = scratch_dir("bay-half-created");
        let sandbox = fake(&[]);
        sandbox.fail_create.set(true);
        let hangar = hangar_with(CONFIG, &state, sandbox.clone());
        let error = reconcile_vm(&hangar, &hangar.bay("default").unwrap(), &[])
            .unwrap_err()
            .to_string();
        assert_eq!(error, "create hangar-bay-default: boot timed out");
        assert!(state.join("bays/default/vm").exists());
        // The next up accounts for it instead of refusing.
        let hangar =
            hangar_with(CONFIG, &state, fake(&[(BAY_VM, BoxState::Running)]));
        assert!(preflight(&hangar, &hangar.bay("default").unwrap()).is_ok());
    }

    #[test]
    fn a_built_image_is_loaded_once_before_the_vm_is_created() {
        let state = scratch_dir("bay-image-loader");
        let config = r#"{"bays": [{"name": "default",
            "image": "hangar-bay:dev", "imageLoader": "true"}]}"#;
        let sandbox = fake(&[]);
        let hangar = hangar_with(config, &state, sandbox.clone());
        let bay = hangar.bay("default").unwrap();
        reconcile_vm(&hangar, &bay, &[]).unwrap();
        sandbox.remove(BAY_VM).unwrap();
        reconcile_vm(&hangar, &bay, &[]).unwrap();
        assert_eq!(
            sandbox.changes(),
            [
                "load hangar-bay:dev",
                "create hangar-bay-default",
                "remove hangar-bay-default",
                "create hangar-bay-default"
            ]
        );
    }

    #[test]
    fn the_bay_starts_with_the_proxy_url_on_stdin() {
        let sandbox = fake(&[(BAY_VM, BoxState::Running)]);
        let state = scratch_dir("bay-start");
        fs::create_dir_all(state.join("guest")).unwrap();
        let hangar = hangar_with("{}", &state, sandbox.clone());
        start(&hangar, &hangar.bay("default").unwrap()).unwrap();
        assert_eq!(
            sandbox.changes(),
            [
                format!("exec root@{BAY_VM} /bin/sh -c {BOOTED} hangar-booted"),
                format!(
                    "exec root@{BAY_VM} /run/current-system/sw/bin/hangar-start"
                ),
            ]
        );
        assert_eq!(
            *sandbox.stdins.borrow(),
            [
                Vec::new(),
                b"http://fake-token:vault@host.fake:14322\n".to_vec()
            ]
        );
        assert_eq!(fs::read(state.join("guest/ca.pem")).unwrap(), b"FAKE-CA\n");

        // A port other than the proxy the VM was created to reach.
        let broker = FakeBroker::default();
        broker.access_port.set(15000);
        let sandbox = fake(&[(BAY_VM, BoxState::Running)]);
        let hangar =
            hangar_with_broker("{}", &state, sandbox.clone(), Box::new(broker));
        let error = start(&hangar, &hangar.bay("default").unwrap())
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "broker gave port 15000 for bay default, but its proxy port is \
             14322"
        );
        let changes = sandbox.changes();
        assert!(!changes.iter().any(|c| c.contains("hangar-start")));
    }

    fn started(sandbox: &FakeSandbox) -> Vec<String> {
        sandbox
            .changes()
            .iter()
            .filter(|call| call.contains(" hangar-run "))
            .map(|call| call.rsplit(" hangar-run ").next().unwrap().into())
            .collect()
    }

    #[test]
    fn up_skips_the_run_of_an_app_that_needs_setup_even_if_overridden() {
        let state = scratch_dir("setup-gate");
        let config = r#"{"appDefinitions": {"web": {
            "setup": {"command": "web init", "check": "test -f ~/.web"},
            "run": "web serve"}},
            "bays": [{"name": "default", "apps": ["web"],
                      "run": {"web": "web serve --dev", "db": "db serve"}}]}"#;
        let sandbox = Rc::new(FakeSandbox::default());
        sandbox.reply("hangar-setup-check", "needed");
        let hangar = hangar_with(config, &state, sandbox.clone());
        start_runs(&hangar, &hangar.bay("default").unwrap()).unwrap();
        assert_eq!(started(&sandbox), ["db db serve"]);

        let sandbox = Rc::new(FakeSandbox::default());
        sandbox.reply("hangar-setup-check", "done");
        let hangar = hangar_with(config, &state, sandbox.clone());
        start_runs(&hangar, &hangar.bay("default").unwrap()).unwrap();
        assert_eq!(started(&sandbox), ["db db serve", "web web serve --dev"]);
    }

    #[test]
    fn up_records_new_runs_and_only_warns_about_changed_ones() {
        assert_eq!(run_decision(true, Some("old"), "new"), RunDecision::Record);
        assert_eq!(run_decision(false, None, "new"), RunDecision::Record);
        assert_eq!(run_decision(false, Some("old"), "new"), RunDecision::Warn);
        assert_eq!(
            run_decision(false, Some("same"), "same"),
            RunDecision::Nothing
        );
    }

    #[test]
    fn placeholders_are_set_and_bay_env_wins() {
        let settings = settings(
            r#"{"tower": {"credentialFiles": {"GITHUB_TOKEN": "/t"}},
                "bays": [{"name": "default", "apps": ["github-token"],
                  "env": {"GITHUB_TOKEN": "gh-placeholder", "MODE": "x"}}]}"#,
        );
        let env = vm_env(&settings, &settings.bays[0]);
        assert_eq!(env["GITHUB_TOKEN"], "gh-placeholder");
        assert_eq!(env["GITHUB_GIT_USER"], "hangar-placeholder");
        assert_eq!(env["MODE"], "x");
    }

    #[test]
    fn the_env_file_is_replaced_whole_never_half_written() {
        let state = scratch_dir("bay-env-atomic");
        let sandbox = Rc::new(FakeSandbox::default());
        let hangar = hangar_with("{}", &state, sandbox.clone());
        write_env(&hangar, &hangar.bay("default").unwrap()).unwrap();
        let calls = sandbox.changes();
        let script = "cat >/etc/hangar/bay.env.tmp && mv \
                      /etc/hangar/bay.env.tmp /etc/hangar/bay.env";
        assert_eq!(
            calls,
            [format!("exec root@{BAY_VM} /bin/sh -c {script} hangar-env")]
        );
    }
}
