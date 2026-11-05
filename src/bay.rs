//! A bay: a locked-down VM that AI coding agents run in. Its only way
//! out is the tower's proxy.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use crate::config::BaySettings;
use crate::error::{Context, Error, Result, bail};
use crate::hangar::Hangar;
use crate::mounts::Roots;
use crate::sandbox::Sandbox;
use crate::state::{BayDir, fnv};
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

pub(crate) fn roots(hangar: &Hangar, bay: &Bay) -> Result<Roots> {
    Roots::new(
        &hangar.host_home,
        hangar.state.root(),
        &hangar.cache_root,
        &bay.dir.home(),
        &bay.cache,
    )
}

/// systemd's state once it can tell.
const BOOTED: &str = "/run/current-system/sw/bin/systemctl is-system-running --wait \
     2>/dev/null || :";
const WAIT_PAUSE: Duration = Duration::from_millis(500);
const WAIT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, PartialEq, Eq)]
enum Probe {
    Done,
    /// Not yet, with what it last said.
    Pending(String),
    /// Waiting won't help.
    Failed(String),
}

/// Probes every `pause` until done, failed or `timeout` passed; the last
/// answer.
fn wait(
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
                "in 'hangar shell -b {}', run 'systemctl --user reset-failed \
                 docker && systemctl --user restart docker'",
                bay.name
            ),
        )),
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

/// The image's CAs and the tower's (`nix/bay/configuration.nix`).
const CA_BUNDLE: VmFile = VmFile {
    name: "hangar-ca",
    path: "/var/lib/hangar/ca-bundle.crt",
    owner: "root:root",
    mode: "0644",
    fill: "cat /etc/hangar/system-ca.crt /run/hangar/ca.pem",
};

/// Replaced whole, so nothing reads half a file, and only when it changed:
/// the bay restarts its daemons whenever `PROXY_ENV_FILE` is replaced.
fn replace_script(fill: &str) -> String {
    format!(
        r#"umask 077 && {fill} >"$1.tmp" && chown "$2" "$1.tmp" &&
chmod "$3" "$1.tmp" &&
if cmp -s "$1.tmp" "$1"; then rm "$1.tmp"; else mv "$1.tmp" "$1"; fi"#
    )
}

fn replace(
    sandbox: &dyn Sandbox,
    vm: &str,
    file: &VmFile,
    stdin: Option<&[u8]>,
) -> Result<()> {
    let script = replace_script(file.fill);
    let command = [
        "/bin/sh", "-c", &script, file.name, file.path, file.owner, file.mode,
    ];
    sandbox.exec(vm, Some(ROOT), &command, stdin).map(drop)
}

/// The proxy for every client, and the CA bundle under every name a tool
/// looks for. The CA's hash makes a new CA a new file too.
fn proxy_env(url: &str, ca_pem: &[u8]) -> String {
    let mut env = BTreeMap::new();
    for name in ["HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"] {
        env.insert(name.to_string(), url.to_string());
    }
    for name in ["NO_PROXY", "no_proxy"] {
        env.insert(name.to_string(), "localhost,127.0.0.1".to_string());
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
    format!(
        "# tower CA {}\n{}",
        fnv(ca_pem.iter().copied()),
        render_env(&env)
    )
}

pub(crate) fn write_env(hangar: &Hangar, bay: &Bay) -> Result<()> {
    let rendered = render_env(&vm_env(bay.settings));
    replace(
        hangar.sandbox.as_ref(),
        &bay.vm,
        &BAY_ENV,
        Some(rendered.as_bytes()),
    )
}

fn vm_env(bay: &BaySettings) -> BTreeMap<String, String> {
    bay.env.clone()
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

/// Starts the unit `$1` unless it runs, after stopping it for `restart`;
/// prints `started`, or the running unit's description. `--collect`
/// unloads it once it exits, failed or not, so its name is free again.
/// The login shell brings the proxy env, `bay.env` and the profile.
const START_RUN: &str = r#"case $(systemctl --user is-active "$1") in
active | activating)
  [ "$4" = restart ] || exec systemctl --user show -P Description "$1"
  systemctl --user stop "$1" ;;
esac
systemd-run --user --quiet --collect --unit="$1" --description="$3" \
  --property=TimeoutStopSec=10s /bin/sh -lc "$2" && echo started"#;

/// A `run` entry's transient unit in pilot's user manager: stopping it
/// stops every process the entry started, and its output is its journal.
pub(crate) fn run_unit(name: &str) -> String {
    format!("hangar-run-{name}.service")
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
        warn!("{name} needs setup: run 'hangar setup {name}'");
    }
    Ok(())
}

/// `up` never restarts anything: it starts what isn't running and warns
/// about a running entry started with other inputs.
pub(crate) fn start_runs(hangar: &Hangar, bay: &Bay) -> Result<()> {
    let needing_setup = needing_setup(hangar, bay)?;
    for (name, command) in &bay.settings.run {
        if needing_setup.contains(name) {
            continue;
        }
        let wanted = description(bay, name, command);
        match launch(hangar, bay, name, command, &wanted, false)?.as_str() {
            "started" => info!("started {name}: hangar logs {name}"),
            running if running != wanted => warn!(
                "{name}'s inputs changed; run 'hangar restart {name}' to apply"
            ),
            _ => {}
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
    for name in &names {
        let command = &run[name];
        info!("restarting {name}");
        let wanted = description(bay, name, command);
        launch(hangar, bay, name, command, &wanted, true)?;
    }
    Ok(names)
}

/// The unit's description records what the entry was started with: a
/// hash of its command, the env file and the copied config files.
fn description(bay: &Bay, name: &str, command: &str) -> String {
    let env = render_env(&vm_env(bay.settings));
    let inputs = [command, &env, &files::record_text(bay)].join("\0");
    format!("hangar run {name} {}", fnv(inputs.into_bytes()))
}

/// The one place an entry is (re)started: what `START_RUN` printed.
fn launch(
    hangar: &Hangar,
    bay: &Bay,
    name: &str,
    command: &str,
    description: &str,
    restart: bool,
) -> Result<String> {
    let script = format!("{USER_MANAGER}{START_RUN}");
    let unit = run_unit(name);
    let mode = if restart { "restart" } else { "start" };
    let output = hangar.sandbox.exec(
        &bay.vm,
        Some(PILOT),
        &[
            "sh",
            "-c",
            &script,
            "hangar-run",
            &unit,
            command,
            description,
            mode,
        ],
        None,
    )?;
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
        Probe, WAIT_TIMEOUT, boot_state, ensure_image, proxy_env,
        replace_script, start_runs, wait_booted, wait_for_docker, write_env,
    };
    use crate::error::Error;
    use crate::sandbox::BoxState;
    use crate::sandbox::fake::FakeSandbox;
    use crate::testing::{hangar_with, scratch_dir};
    use std::fs;
    use std::rc::Rc;
    use std::time::Duration;

    const BAY_VM: &str = "hangar-bay-default";

    fn fake(vms: &[(&str, BoxState)]) -> Rc<FakeSandbox> {
        Rc::new(FakeSandbox::with(vms))
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
            let hangar = hangar_with("{}", &state, sandbox.clone());
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
        let config = r#"{"bays": [{"name": "default",
            "image": "hangar-bay:dev", "imageLoader": "true"}]}"#;
        let sandbox = fake(&[]);
        let hangar = hangar_with(config, &state, sandbox.clone());
        let bay = hangar.bay("default").unwrap();
        ensure_image(&hangar, &bay, "hangar-bay:dev").unwrap();
        ensure_image(&hangar, &bay, "hangar-bay:dev").unwrap();
        assert_eq!(sandbox.changes(), ["load hangar-bay:dev"]);
    }

    fn replace_call(name: &str, path: &str, owner: &str, mode: &str) -> String {
        let fill = if name == "hangar-ca" {
            "cat /etc/hangar/system-ca.crt /run/hangar/ca.pem"
        } else {
            "cat"
        };
        let script = replace_script(fill);
        format!(
            "exec root@{BAY_VM} /bin/sh -c {script} {name} {path} {owner} {mode}"
        )
    }

    #[test]
    fn the_proxy_env_names_the_proxy_the_bundle_and_the_ca() {
        let env = proxy_env("http://a'b@host:1", b"CA-1");
        assert_eq!(
            env,
            format!(
                "# tower CA {}\n\
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
                 no_proxy='localhost,127.0.0.1'\n",
                crate::state::fnv(*b"CA-1")
            )
        );
        // A new CA alone is a new file, which restarts the daemons.
        assert_ne!(env, proxy_env("http://a'b@host:1", b"CA-2"));
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
            let status = std::process::Command::new("/bin/sh")
                .args(["-c", &script, "sh"])
                .arg(&file)
                .args([&owner, "0640", content])
                .status()
                .unwrap();
            assert!(status.success());
        };
        let inode = || fs::metadata(&file).unwrap().ino();
        replace("A=1\n");
        let first = inode();
        assert_eq!(fs::read_to_string(&file).unwrap(), "A=1\n");
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o640
        );
        replace("A=1\n");
        assert_eq!(inode(), first);
        replace("A=2\n");
        assert_ne!(inode(), first);
        assert_eq!(fs::read_to_string(&file).unwrap(), "A=2\n");
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            1,
            "a tmp file was left behind"
        );
    }

    #[test]
    fn docker_is_waited_for_until_it_answers() {
        let sandbox = fake(&[(BAY_VM, BoxState::Running)]);
        sandbox.reply_once("hangar-docker", None);
        let state = scratch_dir("bay-docker");
        let hangar = hangar_with("{}", &state, sandbox.clone());
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
                "in 'hangar shell -b default', run 'systemctl --user \
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
        assert_eq!(started(&sandbox), ["hangar-run-db.service db serve"]);

        let sandbox = Rc::new(FakeSandbox::default());
        sandbox.reply("hangar-setup-check", "done");
        let hangar = hangar_with(config, &state, sandbox.clone());
        start_runs(&hangar, &hangar.bay("default").unwrap()).unwrap();
        assert_eq!(
            started(&sandbox),
            [
                "hangar-run-db.service db serve",
                "hangar-run-web.service web serve --dev"
            ]
        );
    }

    #[test]
    fn the_env_file_is_replaced_whole_by_root() {
        let state = scratch_dir("bay-env-atomic");
        let sandbox = Rc::new(FakeSandbox::default());
        let hangar = hangar_with("{}", &state, sandbox.clone());
        write_env(&hangar, &hangar.bay("default").unwrap()).unwrap();
        assert_eq!(
            sandbox.changes(),
            [replace_call(
                "hangar-env",
                "/etc/hangar/bay.env",
                "root:root",
                "0644"
            )]
        );
    }
}
