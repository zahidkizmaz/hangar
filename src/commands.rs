//! What each command does. Commands return data for `output` to render;
//! `main` only parses arguments and dispatches here.

use std::fs;
use std::io::{self, IsTerminal};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use crate::apps::Setup;
use crate::bay::{self, Bay, PILOT, VM_HOME};
use crate::config::{self, BaySettings, Paths, env_var};
use crate::error::{Context, Error, Result, bail};
use crate::hangar::Hangar;
use crate::keychain::{self, Keychain, OsKeychain};
use crate::overview::{StatusReport, UpReport, VaultLogin};
use crate::sandbox::{BoxState, TOWER_VM, bay_vm};
use crate::{broker, files, output, password, process};
use log::{info, warn};

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

type Step = fn(&Hangar) -> Result<()>;
type BayStep = fn(&Hangar, &Bay) -> Result<()>;

/// In order, once the tower runs. Deny comes before routes and
/// credentials, so a failure in between can't leave the broker forwarding
/// unlisted hosts. Bays' tokens that no configured bay owns go last.
const TOWER_STEPS: [(&str, Step); 4] = [
    ("deny", broker::deny_unlisted),
    ("routes", broker::apply_routes),
    ("credentials", broker::apply_credentials),
    ("tokens", broker::retain_bays),
];

/// In order, per bay: its own preflight, the VM, the broker's access to it
/// (its proxy env), then what runs on top of them.
const BAY_STEPS: [(&str, BayStep); 8] = [
    ("preflight", bay::preflight),
    ("vm", bay::ensure_vm),
    ("start", bay::start),
    ("packages", bay::reconcile_packages),
    ("files", files::copy_declared),
    ("env", bay::write_env),
    ("setup", bay::check_setup),
    ("run", bay::start_runs),
];

/// The tower first, then each named bay (all configured ones without
/// names), in config order. A failing bay doesn't stop the others: it's
/// logged, reported in `failed` and makes `up` exit 1.
pub(crate) fn up(hangar: &Hangar, names: &[String]) -> Result<UpReport> {
    let selected = configured(hangar, names)?;
    // Checked before anything is created: an empty password would start an
    // unencrypted vault.
    let file = hangar.settings.master_password_file.as_deref();
    let password = password::master_password(
        &env_var,
        file,
        &os_keychain()?,
        hangar.broker.has_data().as_deref(),
    )?;
    let state = hangar.state.root();
    fs::create_dir_all(state).context(state.display())?;
    fs::set_permissions(state, fs::Permissions::from_mode(0o700))
        .context(state.display())?;

    broker::preflight(hangar)?;
    broker::start(hangar, &password)?;
    run_tower_steps(hangar)?;
    let mut failed = Vec::new();
    for bay in &selected {
        if let Err(error) = run_bay_steps(hangar, bay) {
            warn!("bay {}: {error}", bay.name);
            failed.push((bay.name.to_string(), error.to_string()));
        }
    }
    let status = StatusReport::collect(hangar);
    for leftover in &status.leftovers {
        let fix = output::leftover_fix(&leftover.name, &leftover.vm);
        warn!("bay {} is not in the config: {fix}", leftover.name);
    }
    for bay in selected
        .iter()
        .filter(|bay| !failed.iter().any(|(name, _)| name == bay.name))
    {
        info!("ready: hangar shell{}", bay::flag(hangar, bay));
    }
    Ok(UpReport { status, failed })
}

/// The configured bays `names` picks (all without names). A leftover's
/// name is refused: `up` never starts one.
fn configured<'a>(
    hangar: &'a Hangar,
    names: &[String],
) -> Result<Vec<Bay<'a>>> {
    if names.is_empty() {
        return Ok(hangar.bays());
    }
    let leftovers = hangar.leftovers();
    let mut bays: Vec<Bay> = Vec::new();
    for name in names {
        let Some(bay) = hangar.bay(name) else {
            if leftovers.contains(name) {
                return Err(Error::with_hint(
                    format!("bay {name} is not in the config"),
                    format!("add it to bays, or hangar destroy {name}"),
                ));
            }
            return Err(unknown_bay(hangar, name));
        };
        bays.push(bay);
    }
    bays.sort_by_key(|bay| {
        hangar
            .settings
            .bays
            .iter()
            .position(|settings| settings.name == bay.name)
    });
    bays.dedup_by(|a, b| a.name == b.name);
    Ok(bays)
}

fn unknown_bay(hangar: &Hangar, name: &str) -> Error {
    let known: Vec<&str> = hangar
        .settings
        .bays
        .iter()
        .map(|bay| bay.name.as_str())
        .collect();
    Error::with_hint(
        format!("no bay named {name}"),
        format!("bays: {}", known.join(", ")),
    )
}

/// The VMs `down` and `destroy` act on: the named bays (configured or
/// leftover), or every bay's and the tower's.
fn vms(hangar: &Hangar, names: &[String]) -> Result<Vec<String>> {
    let leftovers = hangar.leftovers();
    if names.is_empty() {
        let mut vms: Vec<String> =
            hangar.bays().into_iter().map(|bay| bay.vm).collect();
        vms.extend(leftovers.iter().map(|name| bay_vm(name)));
        vms.push(TOWER_VM.to_string());
        return Ok(vms);
    }
    names
        .iter()
        .map(|name| {
            if hangar.bay(name).is_some() || leftovers.contains(name) {
                Ok(bay_vm(name))
            } else {
                Err(unknown_bay(hangar, name))
            }
        })
        .collect()
}

pub(crate) fn pick<'a>(
    hangar: &'a Hangar,
    name: Option<&str>,
) -> Result<Bay<'a>> {
    if let Some(name) = name {
        return hangar.bay(name).ok_or_else(|| unknown_bay(hangar, name));
    }
    let mut bays = hangar.bays();
    if bays.len() == 1 {
        return Ok(bays.remove(0));
    }
    let names: Vec<&str> = bays.iter().map(|bay| bay.name).collect();
    Err(Error::new(format!(
        "several bays: pass --bay ({})",
        names.join(", ")
    )))
}

fn run_tower_steps(hangar: &Hangar) -> Result<()> {
    for (name, step) in TOWER_STEPS {
        log::debug!("tower step: {name}");
        step(hangar)?;
    }
    Ok(())
}

fn run_bay_steps(hangar: &Hangar, bay: &Bay) -> Result<()> {
    for (name, step) in BAY_STEPS {
        log::debug!("bay {} step: {name}", bay.name);
        step(hangar, bay)?;
    }
    Ok(())
}

/// Like `destroy`, a broken sandbox is an error, not "nothing to stop".
pub(crate) fn down(hangar: &Hangar, names: &[String]) -> Result<()> {
    for vm in &vms(hangar, names)? {
        if hangar.sandbox.state(vm)? == BoxState::Running {
            info!("stopping {vm}");
            hangar.sandbox.stop(vm)?;
        }
    }
    Ok(())
}

pub(crate) fn shell(
    hangar: &Hangar,
    bay: Option<&str>,
    args: &[String],
) -> Result<()> {
    let bay = pick(hangar, bay)?;
    let mut shell = pilot_shell(hangar, &bay, args, io::stdin().is_terminal());
    log::debug!("run: {}", process::describe(&shell));
    // Only returns if the shell could not be started.
    let error = shell.exec();
    Err(process::spawn_error(&shell, &error))
}

/// Pilot's shell, in its home: what `shell`, `logs` and `setup` run.
fn pilot_shell(
    hangar: &Hangar,
    bay: &Bay,
    args: &[String],
    tty: bool,
) -> std::process::Command {
    hangar.sandbox.shell(&bay.vm, PILOT, VM_HOME, args, tty)
}

pub(crate) fn logs(
    hangar: &Hangar,
    bay: Option<&str>,
    name: &str,
    follow: bool,
) -> Result<()> {
    let bay = pick(hangar, bay)?;
    if !bay.settings.run.contains_key(name) {
        return Err(Error::with_hint(
            format!("{name} is not a run entry or app"),
            "add it to run or apps in your config, then run 'hangar up'",
        ));
    }
    let unit = bay::run_unit(name);
    let mut journal = [
        "journalctl",
        "--user",
        "-u",
        &unit,
        "-o",
        "cat",
        "--no-pager",
        "-n",
        "100",
    ]
    .map(String::from)
    .to_vec();
    if follow {
        journal.push("-f".into());
    }
    let tty = io::stdin().is_terminal();
    let mut journal = pilot_shell(hangar, &bay, &journal, tty);
    if !process::status(&mut journal)?.success() {
        bail!("can't read {name}'s journal in {}", bay.vm);
    }
    Ok(())
}

pub(crate) fn copy_files(
    hangar: &Hangar,
    bay: Option<&str>,
    src: Option<&str>,
    dest: Option<&str>,
) -> Result<(String, files::Copied)> {
    let bay = pick(hangar, bay)?;
    require_running(hangar, &bay)?;
    let request = match src {
        Some(src) => files::Request::AdHoc { src, dest },
        None => files::Request::Declared,
    };
    Ok((bay.name.to_string(), files::copy(hangar, &bay, &request)?))
}

pub(crate) fn restart(
    hangar: &Hangar,
    bay: Option<&str>,
    names: &[String],
    copy_files: bool,
) -> Result<(String, Vec<String>)> {
    let bay = pick(hangar, bay)?;
    require_running(hangar, &bay)?;
    let restarted = bay::restart(hangar, &bay, names, copy_files)?;
    Ok((bay.name.to_string(), restarted))
}

pub(crate) fn setup(
    hangar: &Hangar,
    bay: Option<&str>,
    name: &str,
    force: bool,
) -> Result<()> {
    let bay = pick(hangar, bay)?;
    let setup = app_setup(bay.settings, name)?;
    require_running(hangar, &bay)?;
    let sandbox = hangar.sandbox.as_ref();
    let again = format!("hangar setup{} {name}", bay::flag(hangar, &bay));
    if !force && bay::setup_done(sandbox, &bay.vm, &setup.check)? {
        info!("{name} is already set up ('{again} --force' reruns it)");
        return Ok(());
    }
    let command = ["sh".to_string(), "-lc".into(), setup.command.clone()];
    let mut shell = pilot_shell(hangar, &bay, &command, true);
    if !process::status(&mut shell)?.success() {
        return Err(Error::with_hint(
            format!("{name}'s setup failed: {}", setup.command),
            format!("run '{again}' to try again"),
        ));
    }
    if !bay::setup_done(sandbox, &bay.vm, &setup.check)? {
        return Err(Error::with_hint(
            format!("{name}'s setup check still fails: {}", setup.check),
            format!("run '{again}' to try again"),
        ));
    }
    let up = if bay::flag(hangar, &bay).is_empty() {
        "hangar up".to_string()
    } else {
        format!("hangar up {}", bay.name)
    };
    info!("{name} is set up: run '{up}' to start {name}");
    Ok(())
}

fn app_setup<'a>(settings: &'a BaySettings, name: &str) -> Result<&'a Setup> {
    let Some(app) = settings.apps.iter().find(|app| app.name == name) else {
        let enabled = settings
            .apps
            .iter()
            .map(|app| app.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let hint = if enabled.is_empty() {
            "no app is enabled: add it to apps in your config".to_string()
        } else {
            format!("enabled apps: {enabled}")
        };
        return Err(Error::with_hint(
            format!("{name} is not an enabled app"),
            hint,
        ));
    };
    app.setup
        .as_ref()
        .ok_or_else(|| Error::new(format!("{name} has no setup")))
}

fn require_running(hangar: &Hangar, bay: &Bay) -> Result<()> {
    if hangar.sandbox.state(&bay.vm)? == BoxState::Running {
        return Ok(());
    }
    Err(Error::with_hint(
        format!("{} isn't running", bay.vm),
        "run 'hangar up' first",
    ))
}

const CLIPBOARDS: [(&str, &[&str]); 3] = [
    ("pbcopy", &[]),
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
];

/// Copies the broker login's password to the clipboard (over stdin) and
/// opens the UI.
pub(crate) fn vault_ui(hangar: &Hangar) -> Result<VaultLogin> {
    let Some(login) = hangar.broker.ui()? else {
        return Err(Error::with_hint(
            "no vault login yet",
            "run 'hangar up' first",
        ));
    };
    let copied = copy(login.password.expose().as_bytes());
    open_in_browser(&login.url);
    Ok(VaultLogin {
        url: login.url,
        login: login.login,
        copied,
        password_file: login.password_file.display().to_string(),
    })
}

/// The first opener that works; none is fine, the caller shows the URL.
pub(crate) fn open_in_browser(url: &str) {
    let _opened = ["open", "xdg-open"].iter().any(|opener| {
        let mut open = process::command(opener);
        open.arg(url);
        process::output(&mut open, None).is_ok_and(|o| o.status.success())
    });
}

fn copy(secret: &[u8]) -> bool {
    CLIPBOARDS.iter().any(|(tool, args)| {
        let mut copy = process::command(tool);
        copy.args(*args);
        process::output(&mut copy, Some(secret))
            .is_ok_and(|output| output.status.success())
    })
}

pub(crate) fn destroy(
    hangar: &Hangar,
    names: &[String],
    wipe: bool,
    yes: bool,
) -> Result<()> {
    let vms = vms(hangar, names)?;
    let folders: Vec<PathBuf> = names
        .iter()
        .map(|name| hangar.state.bays().join(name))
        .collect();
    if wipe && !yes && !confirm(hangar.state.root(), &folders)? {
        bail!("not confirmed: nothing was removed");
    }
    // Missing sandboxes are fine, but a failed removal must keep the state:
    // a leftover VM would otherwise restart against deleted data.
    for vm in &vms {
        if hangar.sandbox.state(vm)? != BoxState::Missing {
            hangar.sandbox.remove(vm)?;
        }
    }
    if !wipe {
        return Ok(());
    }
    if names.is_empty() {
        return wipe_state(hangar.state.root(), &os_keychain()?);
    }
    for folder in folders.iter().filter(|folder| folder.exists()) {
        fs::remove_dir_all(folder).context(folder.display())?;
    }
    Ok(())
}

fn os_keychain() -> Result<OsKeychain> {
    OsKeychain::new(env_var("HANGAR_KEYCHAIN_SERVICE"))
}

/// The keychain item goes too, so the next `up` starts a fresh vault with
/// a fresh password.
fn wipe_state(state: &Path, keychain: &dyn Keychain) -> Result<()> {
    if state.exists() {
        fs::remove_dir_all(state).context(state.display())?;
    }
    keychain.delete()
}

/// The one prompt hangar shows; it goes to stderr like other diagnostics.
/// It lists exactly what goes: the named bays' folders, or all of
/// `stateDir` and the keychain item.
fn confirm(state: &Path, folders: &[PathBuf]) -> Result<bool> {
    if !io::stdin().is_terminal() {
        return Err(Error::with_hint(
            "refusing to delete state without a terminal",
            "pass --yes",
        ));
    }
    if folders.is_empty() {
        eprint!(
            "Delete {} (broker data, tokens, the bays' homes) and the \
             master password in {}? [y/N] ",
            state.display(),
            keychain::STORE
        );
    } else {
        let shown: Vec<String> =
            folders.iter().map(|f| f.display().to_string()).collect();
        eprint!(
            "Delete {} (the bays' records and homes)? [y/N] ",
            shown.join(", ")
        );
    }
    let mut answer = String::new();
    io::stdin().read_line(&mut answer).context("terminal")?;
    Ok(answer.trim() == "y")
}

#[cfg(test)]
mod tests {
    use super::{
        BAY_STEPS, destroy, down, logs, run_tower_steps, setup, vault_ui,
        wipe_state,
    };
    use crate::bay;
    use crate::broker::fake::FakeBroker;
    use crate::keychain::fake::FakeKeychain;
    use crate::sandbox::fake::FakeSandbox;
    use crate::sandbox::{BoxState, TOWER_VM};
    use crate::testing::{hangar_with, hangar_with_broker, scratch_dir};
    use std::fs;
    use std::rc::Rc;

    const BAY_VM: &str = "hangar-bay-default";

    #[test]
    fn down_stops_running_vms_and_destroy_removes_existing_ones() {
        let state = scratch_dir("lifecycle");
        let vms = [(BAY_VM, BoxState::Running), (TOWER_VM, BoxState::Stopped)];
        let sandbox = Rc::new(FakeSandbox::with(&vms));
        let hangar = hangar_with("{}", &state, sandbox.clone());
        down(&hangar, &[]).unwrap();
        destroy(&hangar, &[], false, false).unwrap();
        destroy(&hangar, &[], false, false).unwrap();
        assert_eq!(
            sandbox.changes(),
            [
                "stop hangar-bay-default",
                "remove hangar-bay-default",
                "remove hangar-tower"
            ]
        );
        assert!(state.exists());
    }

    #[test]
    fn logs_read_the_entry_journal_in_the_bay() {
        let state = scratch_dir("logs");
        let sandbox = Rc::new(FakeSandbox::default());
        let config =
            r#"{"bays": [{"name": "default", "run": {"app": "app serve"}}]}"#;
        let hangar = hangar_with(config, &state, sandbox.clone());
        // The fake's shell fails, like a journal pilot can't read.
        let error = logs(&hangar, None, "app", true).unwrap_err().to_string();
        assert_eq!(error, "can't read app's journal in hangar-bay-default");
        assert_eq!(
            sandbox.changes(),
            ["shell pilot@hangar-bay-default journalctl --user -u \
                 hangar-run-app.service -o cat --no-pager -n 100 -f"]
        );
    }

    #[test]
    fn wiping_state_deletes_the_keychain_item() {
        let state = scratch_dir("wipe");
        fs::create_dir_all(state.join("vault/.agent-vault")).unwrap();
        let keychain = FakeKeychain::holding("stored");

        wipe_state(&state, &keychain).unwrap();
        assert!(!state.exists());
        assert!(keychain.item.borrow().is_none());
    }

    fn position<S>(steps: &[(&str, S)], name: &str) -> usize {
        steps.iter().position(|(step, _)| *step == name).unwrap()
    }

    #[test]
    fn the_broker_is_configured_in_order_before_the_bay_gets_access() {
        let state = scratch_dir("broker-order");
        fs::create_dir_all(state.join("guest")).unwrap();
        fs::write(state.join("credential-keys"), "OLD\n").unwrap();
        let token = state.join("token");
        fs::write(&token, "value\n").unwrap();
        let config = format!(
            r#"{{"tower": {{"credentialFiles": {{"GITHUB_TOKEN": "{}"}}}},
                "bays": [{{"name": "default", "apps": ["github-token"]}}]}}"#,
            token.display()
        );
        let broker = FakeBroker::default();
        let calls = broker.calls.clone();
        let sandbox =
            Rc::new(FakeSandbox::with(&[(BAY_VM, BoxState::Running)]));
        let hangar =
            hangar_with_broker(&config, &state, sandbox, Box::new(broker));
        run_tower_steps(&hangar).unwrap();
        bay::start(&hangar, &hangar.bay("default").unwrap()).unwrap();
        let kinds: Vec<String> = calls
            .borrow()
            .iter()
            .map(|call| call.split(' ').next().unwrap().to_string())
            .collect();
        assert_eq!(
            kinds,
            [
                "deny_unlisted",
                "set_routes",
                "delete",
                "put",
                "put",
                "retain",
                "access"
            ]
        );
        assert_eq!(calls.borrow()[5], "retain default");
        assert_eq!(calls.borrow()[6], "access default");
        let services = calls.borrow()[1].clone();
        assert!(
            services.contains("github-api,github-codeload"),
            "{services}"
        );
    }

    #[test]
    fn vault_ui_needs_a_login() {
        let state = scratch_dir("vault-ui");
        let hangar = hangar_with("{}", &state, Rc::new(FakeSandbox::default()));
        let error = vault_ui(&hangar).err().unwrap();
        assert_eq!(error.message(), "no vault login yet");
    }

    #[test]
    fn a_bay_is_checked_and_started_before_anything_runs_in_it() {
        let at = |name| position(&BAY_STEPS, name);
        assert_eq!(at("preflight"), 0);
        assert_eq!(at("vm"), 1);
        assert!(at("start") < at("packages"));
        // Apps started by `run` see their config files and environment.
        assert!(at("packages") < at("files"));
        assert!(at("files") < at("run"));
        // A setup check sees PATH and the app env; a failing one skips run.
        assert!(at("env") < at("setup"));
        assert!(at("setup") < at("run"));
    }

    const WEB_APP: &str = r#"{"bays": [{"name": "default", "apps": ["web"]}],
        "appDefinitions": {"web": {
        "setup": {"command": "web init", "check": "test -f ~/.web"},
        "run": "web serve"}}}"#;

    const CHECK: &str = "exec pilot@hangar-bay-default sh -c if sh -lc \"$1\" >/dev/null 2>&1; \
                         then echo done; else echo needed; fi \
                         hangar-setup-check test -f ~/.web";

    fn setup_fake(check: &str, shell_succeeds: bool) -> Rc<FakeSandbox> {
        let sandbox = FakeSandbox::with(&[(BAY_VM, BoxState::Running)]);
        sandbox.reply("hangar-setup-check", check);
        sandbox.shell_succeeds.set(shell_succeeds);
        Rc::new(sandbox)
    }

    #[test]
    fn setup_runs_the_command_with_a_terminal_then_the_check() {
        let state = scratch_dir("setup");
        let sandbox = setup_fake("done", true);
        let hangar = hangar_with(WEB_APP, &state, sandbox.clone());
        setup(&hangar, None, "web", true).unwrap();
        assert_eq!(
            sandbox.changes(),
            ["shell pilot@hangar-bay-default sh -lc web init", CHECK]
        );
    }

    #[test]
    fn setup_without_force_leaves_a_passing_check_alone() {
        let state = scratch_dir("setup-done");
        let sandbox = setup_fake("done", true);
        let hangar = hangar_with(WEB_APP, &state, sandbox.clone());
        setup(&hangar, None, "web", false).unwrap();
        assert_eq!(sandbox.changes(), [CHECK]);
    }

    #[test]
    fn a_failing_setup_command_or_check_is_an_error() {
        let state = scratch_dir("setup-fails");
        let hangar = hangar_with(WEB_APP, &state, setup_fake("needed", false));
        let error = setup(&hangar, None, "web", false).unwrap_err();
        assert_eq!(error.message(), "web's setup failed: web init");
        assert_eq!(error.hint(), Some("run 'hangar setup web' to try again"));

        let sandbox = setup_fake("needed", true);
        let hangar = hangar_with(WEB_APP, &state, sandbox.clone());
        let error = setup(&hangar, None, "web", false).unwrap_err();
        assert_eq!(
            error.message(),
            "web's setup check still fails: test -f ~/.web"
        );
        assert_eq!(
            sandbox.changes(),
            [
                CHECK,
                "shell pilot@hangar-bay-default sh -lc web init",
                CHECK
            ]
        );

        let hangar = hangar_with(WEB_APP, &state, setup_fake("", true));
        let error = setup(&hangar, None, "web", false).unwrap_err().to_string();
        assert_eq!(error, "setup check: unexpected \"\"");
    }

    #[test]
    fn setup_needs_an_enabled_app_with_setup_and_a_running_vm() {
        let state = scratch_dir("setup-refused");
        let sandbox = setup_fake("done", true);
        let hangar = hangar_with(WEB_APP, &state, sandbox.clone());
        let error = setup(&hangar, None, "db", false).unwrap_err();
        assert_eq!(error.message(), "db is not an enabled app");
        assert_eq!(error.hint(), Some("enabled apps: web"));

        let none = hangar_with("{}", &state, sandbox.clone());
        let error = setup(&none, None, "web", false).unwrap_err();
        assert_eq!(
            error.hint(),
            Some("no app is enabled: add it to apps in your config")
        );

        let config = r#"{"bays": [{"name": "default", "apps": ["web"]}],
                         "appDefinitions": {"web": {}}}"#;
        let plain = hangar_with(config, &state, sandbox.clone());
        let error = setup(&plain, None, "web", false).unwrap_err().to_string();
        assert_eq!(error, "web has no setup");
        assert_eq!(sandbox.changes(), Vec::<String>::new());

        let stopped =
            Rc::new(FakeSandbox::with(&[(BAY_VM, BoxState::Stopped)]));
        let hangar = hangar_with(WEB_APP, &state, stopped.clone());
        let error = setup(&hangar, None, "web", false).unwrap_err();
        assert_eq!(error.message(), "hangar-bay-default isn't running");
        assert_eq!(stopped.changes(), Vec::<String>::new());
    }
}
