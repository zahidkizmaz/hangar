//! What each command does. Commands return data for `output` to render;
//! `main` only parses arguments and dispatches here.

use std::fs::{self, File};
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::apps::Setup;
use crate::bay::{self, Bay, PILOT, VM_HOME};
use crate::config::{self, BaySettings, Paths, valid_name};
use crate::error::{Context, Error, Result, bail};
use crate::files::{self, Copied, Request};
use crate::hangar::Hangar;
use crate::lifecycle::{Change, Outcome, Resource};
use crate::prompt::Prompt;
use crate::proxy::{Proxy, Secrets, Slot, managed, proxy_vm};
use crate::sandbox::{BoxState, bay_vm, data_volume};
use crate::status::{self, Detail, Overview};
use crate::{mounts, process};
use log::info;

/// hangar's own log, in `$XDG_STATE_HOME/hangar`.
pub(crate) const LOG_FILE: &str = "hangar.log";
const LOG_LINES: usize = 200;
const FOLLOW_PAUSE: Duration = Duration::from_millis(500);

/// Bays and proxies share one namespace.
pub(crate) enum Target<'a> {
    Bay(Bay<'a>),
    Proxy(&'a str, &'a dyn Proxy),
}

pub(crate) struct UpReport {
    pub(crate) changes: Vec<Change>,
}

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

pub(crate) fn target<'a>(hangar: &'a Hangar, name: &str) -> Result<Target<'a>> {
    if let Some(bay) = hangar.bay(name) {
        return Ok(Target::Bay(bay));
    }
    if let Some((name, proxy)) = hangar.proxies.get_key_value(name) {
        return Ok(Target::Proxy(name, proxy.as_ref()));
    }
    let bays: Vec<&str> = hangar
        .settings
        .bays
        .iter()
        .map(|bay| bay.name.as_str())
        .collect();
    let proxies: Vec<&str> =
        hangar.proxies.keys().map(String::as_str).collect();
    Err(Error::with_hint(
        format!("no bay or proxy {name}"),
        format!("bays: {}; proxies: {}", listed(&bays), listed(&proxies)),
    ))
}

fn listed(names: &[&str]) -> String {
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

/// What `names` pick, each once; every bay and proxy when empty.
#[derive(Default)]
struct Selection<'a> {
    bays: Vec<Bay<'a>>,
    proxies: Vec<(&'a str, &'a dyn Proxy)>,
}

impl<'a> Selection<'a> {
    fn add_proxy(&mut self, name: &'a str, proxy: &'a dyn Proxy) {
        if !self.proxies.iter().any(|(known, _)| *known == name) {
            self.proxies.push((name, proxy));
        }
    }
}

fn select<'a>(hangar: &'a Hangar, names: &[String]) -> Result<Selection<'a>> {
    let mut selection = Selection::default();
    if names.is_empty() {
        selection.bays = hangar.bays();
        for (name, proxy) in &hangar.proxies {
            selection.add_proxy(name, proxy.as_ref());
        }
    }
    for name in names {
        match target(hangar, name)? {
            Target::Bay(bay) => {
                if !selection.bays.iter().any(|known| known.name == bay.name) {
                    selection.bays.push(bay);
                }
            }
            Target::Proxy(name, proxy) => selection.add_proxy(name, proxy),
        }
    }
    Ok(selection)
}

fn change(name: &str, resource: Resource, outcome: Outcome) -> Change {
    Change {
        name: name.to_string(),
        resource,
        outcome,
    }
}

/// Proxies first, each once, then the bays; a bay whose proxy failed is
/// left alone. Tokens missing from the keychain are asked for first.
pub(crate) fn up(
    hangar: &Hangar,
    names: &[String],
    prompt: &dyn Prompt,
) -> Result<UpReport> {
    let mut selection = select(hangar, names)?;
    let used: Vec<_> = selection
        .bays
        .iter()
        .filter_map(|bay| hangar.proxy_of(bay.settings))
        .collect();
    for (name, proxy) in used {
        selection.add_proxy(name, proxy);
    }
    if prompt.terminal() {
        for (name, proxy) in &selection.proxies {
            let secrets = hangar.secrets(name)?;
            let missing = secrets.missing(proxy.secrets())?;
            ask_for(name, &secrets, &missing, prompt)?;
        }
    }
    let mut changes: Vec<Change> = selection
        .proxies
        .iter()
        .map(|(name, proxy)| {
            change(name, Resource::Proxy, managed::up(hangar, name, *proxy))
        })
        .collect();
    for bay in &selection.bays {
        let proxy = hangar.proxy_of(bay.settings).map(|(name, _)| name);
        let outcome = match proxy {
            Some(proxy) if proxy_failed(&changes, proxy) => {
                Outcome::Failed(Error::new(format!("proxy {proxy} failed")))
            }
            _ => bay::up(hangar, bay),
        };
        changes.push(change(bay.name, Resource::Bay, outcome));
    }
    Ok(UpReport { changes })
}

/// Stores each of `slots`, asked for one by one; what was saved.
fn ask_for(
    proxy: &str,
    secrets: &Secrets,
    slots: &[Slot],
    prompt: &dyn Prompt,
) -> Result<Vec<String>> {
    slots
        .iter()
        .map(|slot| {
            let question = format!("{proxy} {} ({})", slot.key, slot.about);
            secrets.store(slot.key, &prompt.ask(&question)?)?;
            info!("proxy {proxy}: {} saved", slot.key);
            Ok(format!("{} saved", slot.key))
        })
        .collect()
}

/// Asks for `token` of proxy `name`, or every one it keeps in the
/// keychain, and stores it.
pub(crate) fn secret_set(
    hangar: &Hangar,
    name: &str,
    token: Option<&str>,
    prompt: &dyn Prompt,
) -> Result<UpReport> {
    let Target::Proxy(name, proxy) = target(hangar, name)? else {
        bail!("{name} is a bay; only proxies have tokens");
    };
    let secrets = hangar.secrets(name)?;
    let mut slots = secrets.settable(proxy.secrets());
    if slots.is_empty() {
        bail!("proxy {name} keeps no tokens in the keychain");
    }
    if let Some(token) = token {
        let keys: Vec<&str> = slots.iter().map(|slot| slot.key).collect();
        slots.retain(|slot| slot.key == token);
        if slots.is_empty() {
            return Err(Error::with_hint(
                format!("proxy {name} keeps no {token} in the keychain"),
                format!("it keeps: {}", keys.join(", ")),
            ));
        }
    }
    let saved = ask_for(name, &secrets, &slots, prompt)?;
    Ok(UpReport {
        changes: vec![change(name, Resource::Proxy, Outcome::Updated(saved))],
    })
}

fn proxy_failed(changes: &[Change], proxy: &str) -> bool {
    changes.iter().any(|change| {
        change.resource == Resource::Proxy
            && change.name == proxy
            && matches!(change.outcome, Outcome::Failed(_))
    })
}

/// The bays first, then the proxies.
pub(crate) fn down(hangar: &Hangar, names: &[String]) -> Result<UpReport> {
    let selection = select(hangar, names)?;
    let bays = selection.bays.iter().map(|bay| {
        let outcome = bay::down(hangar, bay).unwrap_or_else(Outcome::Failed);
        change(bay.name, Resource::Bay, outcome)
    });
    let proxies = selection.proxies.iter().map(|(name, proxy)| {
        let outcome =
            managed::down(hangar, name, *proxy).unwrap_or_else(Outcome::Failed);
        change(name, Resource::Proxy, outcome)
    });
    Ok(UpReport {
        changes: bays.chain(proxies).collect(),
    })
}

pub(crate) fn restart(
    hangar: &Hangar,
    names: &[String],
    prompt: &dyn Prompt,
) -> Result<UpReport> {
    let mut report = down(hangar, names)?;
    report.changes.extend(up(hangar, names, prompt)?.changes);
    Ok(report)
}

/// What `rm` deletes: the VM, and with `--data` the folder, the data
/// volume and a configured proxy's tokens in the keychain.
struct Doomed {
    resource: Resource,
    vm: String,
    folder: PathBuf,
    volume: Option<String>,
    tokens: Option<(Secrets, Vec<Slot>)>,
}

/// Configured or left over: a leftover is only known by its folder.
fn doomed(hangar: &Hangar, name: &str) -> Result<Doomed> {
    let mut tokens = None;
    let resource = match target(hangar, name) {
        Ok(Target::Bay(_)) => Resource::Bay,
        Ok(Target::Proxy(name, proxy)) => {
            let secrets = hangar.secrets(name)?;
            let slots = secrets.settable(proxy.secrets());
            tokens = (!slots.is_empty()).then_some((secrets, slots));
            Resource::Proxy
        }
        Err(error) => hangar
            .leftovers()
            .into_iter()
            .find_map(|(resource, left)| (left == name).then_some(resource))
            .ok_or(error)?,
    };
    Ok(match resource {
        Resource::Bay => Doomed {
            resource,
            vm: bay_vm(name),
            folder: hangar.state.bays().join(name),
            volume: Some(data_volume(name)),
            tokens: None,
        },
        Resource::Proxy => Doomed {
            resource,
            vm: proxy_vm(name),
            folder: hangar.state.proxies().join(name),
            volume: None,
            tokens,
        },
    })
}

/// The package cache is always kept.
pub(crate) fn rm(
    hangar: &Hangar,
    name: &str,
    data: bool,
    yes: bool,
    prompt: &dyn Prompt,
) -> Result<UpReport> {
    let doomed = doomed(hangar, name)?;
    if data && !yes && !confirm(name, &doomed, prompt)? {
        bail!("not confirmed: nothing was removed");
    }
    let outcome = remove(hangar, &doomed, data)
        .map_or_else(Outcome::Failed, |()| Outcome::Removed);
    Ok(UpReport {
        changes: vec![change(name, doomed.resource, outcome)],
    })
}

fn remove(hangar: &Hangar, doomed: &Doomed, data: bool) -> Result<()> {
    let sandbox = hangar.sandbox.as_ref();
    let state = sandbox.state(&doomed.vm)?;
    // Trap: remove is a hard kill that loses unsynced writes.
    if state == BoxState::Running {
        sandbox.stop(&doomed.vm)?;
    }
    if state != BoxState::Missing {
        sandbox.remove(&doomed.vm)?;
    }
    if !data {
        return Ok(());
    }
    if let Some(volume) = &doomed.volume {
        sandbox.remove_volume(volume)?;
    }
    if doomed.folder.exists() {
        fs::remove_dir_all(&doomed.folder).context(doomed.folder.display())?;
    }
    if let Some((secrets, slots)) = &doomed.tokens {
        secrets.delete(slots)?;
    }
    Ok(())
}

fn confirm(name: &str, doomed: &Doomed, prompt: &dyn Prompt) -> Result<bool> {
    if !prompt.terminal() {
        return Err(Error::with_hint(
            format!("refusing to delete {name}'s data without a terminal"),
            "add --yes",
        ));
    }
    let volume = doomed
        .volume
        .as_ref()
        .map(|volume| format!(" and the volume {volume}"))
        .unwrap_or_default();
    let tokens = doomed
        .tokens
        .as_ref()
        .map(|_| format!(" and its tokens in {}", crate::keychain::STORE))
        .unwrap_or_default();
    prompt.confirm(&format!(
        "Delete {}{volume}{tokens}?",
        doomed.folder.display()
    ))
}

pub(crate) fn ps(hangar: &Hangar) -> Overview {
    let overview = status::collect(hangar);
    for event in status::events(&overview) {
        log::warn!("{event}");
    }
    overview
}

pub(crate) fn inspect(hangar: &Hangar, name: &str) -> Result<Detail> {
    Ok(match target(hangar, name)? {
        Target::Bay(bay) => status::bay_detail(hangar, &bay),
        Target::Proxy(name, proxy) => status::proxy_detail(hangar, name, proxy),
    })
}

fn bay_named<'a>(hangar: &'a Hangar, name: &str) -> Result<Bay<'a>> {
    match target(hangar, name)? {
        Target::Bay(bay) => Ok(bay),
        Target::Proxy(..) => bail!("{name} is a proxy, not a bay"),
    }
}

fn require_running(hangar: &Hangar, bay: &Bay) -> Result<()> {
    if hangar.sandbox.state(&bay.vm)? == BoxState::Running {
        return Ok(());
    }
    Err(Error::with_hint(
        format!("bay {} isn't running", bay.name),
        format!("run 'hangar up {}' first", bay.name),
    ))
}

fn running_bay<'a>(hangar: &'a Hangar, name: &str) -> Result<Bay<'a>> {
    let bay = bay_named(hangar, name)?;
    require_running(hangar, &bay)?;
    Ok(bay)
}

/// Pilot's shell, in its home: what `exec`, `logs` and `setup` run.
fn pilot_shell(
    hangar: &Hangar,
    bay: &Bay,
    args: &[String],
    tty: bool,
) -> std::process::Command {
    hangar.sandbox.shell(&bay.vm, PILOT, VM_HOME, args, tty)
}

/// A login shell without `args`.
pub(crate) fn exec(hangar: &Hangar, bay: &str, args: &[String]) -> Result<()> {
    let bay = running_bay(hangar, bay)?;
    let mut shell = pilot_shell(hangar, &bay, args, io::stdin().is_terminal());
    log::debug!("run: {}", process::describe(&shell));
    // Only returns if the shell could not be started.
    let error = shell.exec();
    Err(process::spawn_error(&shell, &error))
}

/// hangar's log without a name; a bay's apps' journal, or one app's; a
/// managed proxy's log.
pub(crate) fn logs(
    hangar: &Hangar,
    name: Option<&str>,
    app: Option<&str>,
    follow: bool,
) -> Result<()> {
    let Some(name) = name else {
        return tail(&hangar.state_root.join(LOG_FILE), follow);
    };
    match target(hangar, name)? {
        Target::Bay(bay) => journal(hangar, &bay, app, follow),
        Target::Proxy(..) if app.is_some() => {
            bail!("proxy {name} has no apps")
        }
        Target::Proxy(name, proxy) => match proxy.managed() {
            Some(managed) => tail(&managed.log_file(), follow),
            None => bail!("proxy {name} runs elsewhere; its log is there"),
        },
    }
}

fn journal(
    hangar: &Hangar,
    bay: &Bay,
    app: Option<&str>,
    follow: bool,
) -> Result<()> {
    let unit = match app {
        None => bay::run_unit("*"),
        Some(app) if bay.settings.run.contains_key(app) => bay::run_unit(app),
        Some(app) if bay.settings.apps.iter().any(|on| on.name == app) => {
            bail!(
                "{app} has no long-running unit in bay {0}, so it has no \
                 log: it only runs setup ('hangar setup {0} {app}')",
                bay.name
            );
        }
        Some(app) => {
            return Err(Error::with_hint(
                format!("{app} is not an app of bay {}", bay.name),
                "add it to apps in your config, then run 'hangar up'",
            ));
        }
    };
    require_running(hangar, bay)?;
    let mut args = ["journalctl", "--user", "-u", &unit, "--no-pager", "-n"]
        .map(String::from)
        .to_vec();
    args.push(LOG_LINES.to_string());
    if follow {
        args.push("-f".into());
    }
    let tty = io::stdin().is_terminal();
    if !process::status(&mut pilot_shell(hangar, bay, &args, tty))?.success() {
        bail!("can't read the journal of bay {}", bay.name);
    }
    Ok(())
}

/// The last lines of `path`, then, with `follow`, what it gains. A
/// proxy's VM writes its log: a symlink planted there is never followed.
fn tail(path: &Path, follow: bool) -> Result<()> {
    let mut file = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .context(path.display())?;
    let mut text = Vec::new();
    file.read_to_end(&mut text).context(path.display())?;
    let mut stdout = io::stdout();
    stdout
        .write_all(last_lines(&text, LOG_LINES))
        .context("stdout")?;
    if !follow {
        return Ok(());
    }
    loop {
        stdout.flush().context("stdout")?;
        thread::sleep(FOLLOW_PAUSE);
        text.clear();
        file.read_to_end(&mut text).context(path.display())?;
        stdout.write_all(&text).context("stdout")?;
    }
}

fn last_lines(text: &[u8], count: usize) -> &[u8] {
    let body = text.strip_suffix(b"\n").unwrap_or(text);
    let start = body
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, byte)| **byte == b'\n')
        .nth(count.saturating_sub(1))
        .map_or(0, |(at, _)| at + 1);
    &text[start..]
}

/// Exactly one side is `BAY:PATH`; the other is a host path.
pub(crate) fn cp(hangar: &Hangar, src: &str, dest: &str) -> Result<Copied> {
    match (bay_side(hangar, src)?, bay_side(hangar, dest)?) {
        (None, Some((bay, dest))) => {
            let bay = running_bay(hangar, bay)?;
            let dest = Some(dest);
            files::copy(hangar, &bay, &Request::AdHoc { src, dest })
        }
        (Some((bay, path)), None) => copy_out(hangar, bay, path, dest),
        _ => Err(Error::with_hint(
            "cp takes exactly one BAY:PATH",
            "hangar cp FILE BAY:PATH, or hangar cp BAY:PATH FILE",
        )),
    }
}

/// `BAY:PATH` when the part before `:` is a name; anything else is a
/// host path.
fn bay_side<'a>(
    hangar: &Hangar,
    arg: &'a str,
) -> Result<Option<(&'a str, &'a str)>> {
    let Some((name, path)) = arg.split_once(':') else {
        return Ok(None);
    };
    if !valid_name(name) {
        return Ok(None);
    }
    bay_named(hangar, name)?;
    if path.is_empty() {
        return Err(Error::with_hint(
            format!("{arg} has no path"),
            format!("name one, like {name}:~/file"),
        ));
    }
    Ok(Some((name, path)))
}

/// Only from a running bay: copying from a stopped one never returns.
/// The destination must not exist yet, and is checked where it resolves.
fn copy_out(
    hangar: &Hangar,
    bay: &str,
    guest: &str,
    dest: &str,
) -> Result<Copied> {
    let bay = running_bay(hangar, bay)?;
    let guest = match guest.strip_prefix("~/") {
        Some(rest) => format!("{VM_HOME}/{rest}"),
        None => guest.to_string(),
    };
    let host = files::host_path(dest, &hangar.host_home)?;
    if fs::symlink_metadata(&host).is_ok() {
        return Err(Error::with_hint(
            format!("{} exists", host.display()),
            "copy to a path that doesn't exist yet",
        ));
    }
    let host = mounts::would_be(&host).context(host.display())?;
    mounts::check_hangar_dirs(&host, &bay::roots(hangar, &bay)?)
        .map_err(|why| Error::new(format!("{}: {why}", host.display())))?;
    hangar.sandbox.copy_out(&bay.vm, &guest, &host)?;
    Ok(Copied {
        copied: vec![host.display().to_string()],
        removed: Vec::new(),
    })
}

pub(crate) fn setup(
    hangar: &Hangar,
    bay: &str,
    app: &str,
    force: bool,
) -> Result<()> {
    let bay = bay_named(hangar, bay)?;
    let setup = app_setup(bay.settings, app)?;
    require_running(hangar, &bay)?;
    let sandbox = hangar.sandbox.as_ref();
    let again = format!("hangar setup {} {app}", bay.name);
    if !force && bay::setup_done(sandbox, &bay.vm, &setup.check)? {
        info!("{app} is already set up ('{again} --force' reruns it)");
        return Ok(());
    }
    let command = ["sh".to_string(), "-lc".into(), setup.command.clone()];
    let tty = io::stdin().is_terminal();
    let mut shell = pilot_shell(hangar, &bay, &command, tty);
    if !process::status(&mut shell)?.success() {
        return Err(Error::with_hint(
            format!("{app}'s setup failed: {}", setup.command),
            format!("run '{again}' to try again"),
        ));
    }
    if !bay::setup_done(sandbox, &bay.vm, &setup.check)? {
        return Err(Error::with_hint(
            format!("{app}'s setup check still fails: {}", setup.check),
            format!("run '{again}' to try again"),
        ));
    }
    info!("{app} is set up: run 'hangar up {}' to start it", bay.name);
    Ok(())
}

fn app_setup<'a>(settings: &'a BaySettings, name: &str) -> Result<&'a Setup> {
    let Some(app) = settings.apps.iter().find(|app| app.name == name) else {
        let enabled: Vec<&str> =
            settings.apps.iter().map(|app| app.name.as_str()).collect();
        return Err(Error::with_hint(
            format!("{name} is not an enabled app"),
            format!("enabled apps: {}", listed(&enabled)),
        ));
    };
    app.setup
        .as_ref()
        .ok_or_else(|| Error::new(format!("{name} has no setup")))
}

#[cfg(test)]
mod tests {
    use super::{
        UpReport, cp, down, exec, inspect, last_lines, logs, ps, rm,
        secret_set, setup, tail, target,
    };
    use crate::error::Result;
    use crate::hangar::Hangar;
    use crate::keychain::fake::FakeKeychain;
    use crate::prompt::fake::FakePrompt;
    use crate::sandbox::fake::FakeSandbox;
    use crate::sandbox::{BoxState, data_volume};
    use crate::status::{Detail, TokenSource};
    use crate::testing::{hangar_holding, hangar_with, scratch_dir, settled};
    use std::fs;
    use std::rc::Rc;

    fn up(hangar: &Hangar, names: &[String]) -> Result<UpReport> {
        super::up(hangar, names, &FakePrompt::default())
    }

    fn restart(hangar: &Hangar, names: &[String]) -> Result<UpReport> {
        super::restart(hangar, names, &FakePrompt::default())
    }

    const TOKENED: &str = r#"{"proxies": {
        "t": {"kind": "fake", "run": "managed", "managed": true,
              "token": true, "port": 3100},
        "c": {"kind": "fake", "url": "http://127.0.0.1:3101", "port": 3101,
              "token": true, "secrets": {"token":
                  {"command": ["/bin/sh", "-c", "echo from-cmd"]}}}},
        "bays": [{"name": "a", "proxy": "t"}, {"name": "b", "proxy": null}]}"#;

    fn tokened(
        name: &str,
        items: &[(&str, &str)],
    ) -> (Hangar, Rc<FakeKeychain>) {
        let keychain = Rc::new(FakeKeychain::holding(items));
        let state = scratch_dir(name);
        let hangar =
            hangar_holding(TOKENED, &state, settled(), keychain.clone());
        (hangar, keychain)
    }

    #[test]
    fn up_asks_for_missing_tokens_of_the_proxies_it_brings_up() {
        let (hangar, keychain) = tokened("cmd-up-ask", &[]);
        let prompt = FakePrompt::answering(&["tok-1"]);
        super::up(&hangar, &names(&["a"]), &prompt).unwrap();
        assert_eq!(*prompt.questions.borrow(), ["t token (the fake token)"]);
        assert_eq!(keychain.items.borrow()["t/token"], "tok-1");
        let again = FakePrompt::answering(&[]);
        super::up(&hangar, &names(&["a", "b"]), &again).unwrap();
        assert!(again.questions.borrow().is_empty());
    }

    #[test]
    fn up_without_a_terminal_never_asks() {
        let (hangar, keychain) = tokened("cmd-up-no-ask", &[]);
        let prompt = FakePrompt::default();
        let report = super::up(&hangar, &names(&["t"]), &prompt).unwrap();
        assert_eq!(summary(&report), ["t proxy Created"]);
        assert!(prompt.questions.borrow().is_empty());
        assert!(keychain.items.borrow().is_empty());
    }

    #[test]
    fn secret_set_stores_each_keychain_token_of_a_proxy() {
        let (hangar, keychain) =
            tokened("cmd-secret-set", &[("t/token", "old")]);
        let prompt = FakePrompt::answering(&["new"]);
        let report = secret_set(&hangar, "t", None, &prompt).unwrap();
        assert_eq!(summary(&report), [r#"t proxy Updated(["token saved"])"#]);
        assert_eq!(keychain.items.borrow()["t/token"], "new");
        let one = FakePrompt::answering(&["newer"]);
        secret_set(&hangar, "t", Some("token"), &one).unwrap();
        assert_eq!(keychain.items.borrow()["t/token"], "newer");
        let error = secret_set(&hangar, "t", Some("nope"), &one).err().unwrap();
        assert_eq!(error.message(), "proxy t keeps no nope in the keychain");
        assert_eq!(error.hint(), Some("it keeps: token"));
        let error = secret_set(&hangar, "c", None, &prompt).err().unwrap();
        assert_eq!(
            error.to_string(),
            "proxy c keeps no tokens in the keychain"
        );
        let error = secret_set(&hangar, "a", None, &prompt).err().unwrap();
        assert_eq!(error.to_string(), "a is a bay; only proxies have tokens");
        let bad = FakePrompt::answering(&["two words"]);
        let error = secret_set(&hangar, "t", None, &bad)
            .err()
            .unwrap()
            .to_string();
        assert!(error.starts_with("proxy t: token: "), "{error}");
        assert_eq!(keychain.items.borrow()["t/token"], "newer");
    }

    #[test]
    fn inspect_names_a_proxys_bays_token_sources_and_a_bays_way_out() {
        let (hangar, keychain) = tokened("cmd-inspect-tokens", &[]);
        let sources = |name: &str| {
            let Detail::Proxy(detail) = inspect(&hangar, name).unwrap() else {
                panic!("not a proxy");
            };
            (detail.bays, detail.tokens)
        };
        assert_eq!(
            sources("t"),
            (vec!["a".to_string()], vec![("token", TokenSource::Missing)])
        );
        keychain
            .items
            .borrow_mut()
            .insert("t/token".into(), "x".into());
        assert_eq!(sources("t").1, [("token", TokenSource::Keychain)]);
        assert_eq!(
            sources("c"),
            (vec![], vec![("token", TokenSource::Command)])
        );
        let Detail::Bay(bay) = inspect(&hangar, "a").unwrap() else {
            panic!("not a bay");
        };
        assert_eq!(bay.proxy_port, Some(3100));
        let Detail::Bay(direct) = inspect(&hangar, "b").unwrap() else {
            panic!("not a bay");
        };
        assert_eq!(direct.proxy_port, None);
    }

    #[test]
    fn rm_data_on_a_proxy_deletes_its_tokens_too() {
        let (hangar, keychain) =
            tokened("cmd-rm-tokens", &[("t/token", "x"), ("other/token", "y")]);
        rm(&hangar, "t", false, true, &no_prompt()).unwrap();
        assert!(keychain.items.borrow().contains_key("t/token"));
        rm(&hangar, "t", true, true, &no_prompt()).unwrap();
        assert!(!keychain.items.borrow().contains_key("t/token"));
        assert!(keychain.items.borrow().contains_key("other/token"));
        rm(&hangar, "c", true, true, &no_prompt()).unwrap();
    }

    const PROXIED: &str = r#"{"proxies": {
        "p": {"kind": "fake", "run": "managed", "managed": true,
              "port": 3100},
        "ext": {"kind": "fake", "url": "http://127.0.0.1:3101",
                "port": 3101}},
        "bays": [{"name": "a", "proxy": "p"}, {"name": "b", "proxy": "p"},
                 {"name": "c", "proxy": null}]}"#;

    fn proxied(name: &str, sandbox: &Rc<FakeSandbox>) -> Hangar {
        hangar_with(PROXIED, &scratch_dir(name), sandbox.clone())
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    /// `name resource outcome` per change.
    fn summary(report: &super::UpReport) -> Vec<String> {
        report
            .changes
            .iter()
            .map(|change| {
                let resource = change.resource.as_str();
                format!("{} {resource} {:?}", change.name, change.outcome)
            })
            .collect()
    }

    fn touched(sandbox: &FakeSandbox, vm: &str) -> bool {
        sandbox.calls.borrow().iter().any(|call| call.contains(vm))
    }

    #[test]
    fn up_brings_each_proxy_up_once_before_its_bays() {
        let sandbox = settled();
        let hangar = proxied("cmd-up", &sandbox);
        let report = up(&hangar, &names(&["a", "b", "a"])).unwrap();
        assert_eq!(
            summary(&report),
            ["p proxy Created", "a bay Created", "b bay Created"]
        );
        let created: Vec<String> = sandbox
            .created
            .borrow()
            .iter()
            .map(|vm| vm.name.clone())
            .collect();
        assert_eq!(created, ["hangar-proxy-p", "hangar-bay-a", "hangar-bay-b"]);
    }

    #[test]
    fn up_without_names_covers_every_proxy_and_bay() {
        let sandbox = settled();
        let hangar = proxied("cmd-up-all", &sandbox);
        let report = up(&hangar, &[]).unwrap();
        assert_eq!(
            summary(&report),
            [
                "ext proxy Unchanged",
                "p proxy Created",
                "a bay Created",
                "b bay Created",
                "c bay Created"
            ]
        );
    }

    #[test]
    fn a_failed_proxy_fails_its_bays_without_touching_their_vms() {
        let sandbox = settled();
        sandbox.fail_create.set(true);
        let hangar = proxied("cmd-up-fail", &sandbox);
        let report = up(&hangar, &names(&["a"])).unwrap();
        let summary = summary(&report);
        assert!(summary[0].starts_with("p proxy Failed"), "{summary:?}");
        assert_eq!(
            summary[1],
            r#"a bay Failed(Error { message: "proxy p failed", hint: None })"#
        );
        assert!(!touched(&sandbox, "hangar-bay-a"));
    }

    #[test]
    fn down_stops_bays_before_proxies_and_restart_starts_them_again() {
        let sandbox = settled();
        let hangar = proxied("cmd-down", &sandbox);
        up(&hangar, &[]).unwrap();
        sandbox.calls.borrow_mut().clear();
        let report = down(&hangar, &names(&["p", "a"])).unwrap();
        assert_eq!(summary(&report), ["a bay Stopped", "p proxy Stopped"]);
        assert_eq!(
            sandbox.changes(),
            ["stop hangar-bay-a", "stop hangar-proxy-p"]
        );
        let report = restart(&hangar, &names(&["c"])).unwrap();
        assert_eq!(
            summary(&report),
            ["c bay Stopped", r#"c bay Updated(["started"])"#]
        );
        assert!(sandbox.changes().contains(&"start hangar-bay-c".into()));
    }

    fn no_prompt() -> FakePrompt {
        FakePrompt::default()
    }

    #[test]
    fn rm_data_asks_first_and_keeps_everything_unless_told_yes() {
        let (hangar, keychain) = tokened("cmd-rm-confirm", &[("t/token", "x")]);
        let no = FakePrompt::answering(&["n"]);
        let error = rm(&hangar, "t", true, false, &no).err().unwrap();
        assert_eq!(error.to_string(), "not confirmed: nothing was removed");
        let question = &no.questions.borrow()[0];
        assert!(question.starts_with("Delete "), "{question}");
        assert!(question.contains(" and its tokens in "), "{question}");
        assert!(keychain.items.borrow().contains_key("t/token"));
        let yes = FakePrompt::answering(&["y"]);
        rm(&hangar, "t", true, false, &yes).unwrap();
        assert!(!keychain.items.borrow().contains_key("t/token"));
        let bay = FakePrompt::answering(&["n"]);
        rm(&hangar, "a", true, false, &bay).err().unwrap();
        let question = &bay.questions.borrow()[0];
        let volume = format!(" and the volume {}?", data_volume("a"));
        assert!(question.ends_with(&volume), "{question}");
    }

    #[test]
    fn rm_data_without_a_terminal_needs_yes() {
        let (hangar, _) = tokened("cmd-rm-no-tty", &[]);
        let error = rm(&hangar, "t", true, false, &no_prompt()).err().unwrap();
        assert_eq!(
            error.message(),
            "refusing to delete t's data without a terminal"
        );
        assert_eq!(error.hint(), Some("add --yes"));
        rm(&hangar, "t", false, false, &no_prompt()).unwrap();
    }

    #[test]
    fn rm_data_on_a_proxy_deletes_its_vm_and_folder() {
        let sandbox = settled();
        let state = scratch_dir("cmd-rm-proxy");
        let hangar = hangar_with(PROXIED, &state, sandbox.clone());
        up(&hangar, &names(&["p"])).unwrap();
        assert!(state.join("proxies/p").exists());
        sandbox.calls.borrow_mut().clear();
        let report = rm(&hangar, "p", true, true, &no_prompt()).unwrap();
        assert_eq!(summary(&report), ["p proxy Removed"]);
        assert_eq!(
            sandbox.changes(),
            ["stop hangar-proxy-p", "remove hangar-proxy-p"]
        );
        assert!(!state.join("proxies/p").exists());
    }

    #[test]
    fn unknown_names_list_bays_and_proxies() {
        let hangar = proxied("cmd-target", &settled());
        let error = target(&hangar, "x").err().unwrap();
        assert_eq!(
            error.to_string(),
            "no bay or proxy x: bays: a, b, c; proxies: ext, p"
        );
        assert!(up(&hangar, &names(&["x"])).is_err());
        assert!(rm(&hangar, "x", false, true, &no_prompt()).is_err());
    }

    #[test]
    fn bay_only_commands_refuse_a_proxy() {
        let hangar = proxied("cmd-proxy-not-bay", &settled());
        for error in [
            exec(&hangar, "p", &[]).unwrap_err(),
            setup(&hangar, "p", "web", false).unwrap_err(),
        ] {
            assert_eq!(error.to_string(), "p is a proxy, not a bay");
        }
        let error = cp(&hangar, "/tmp/x", "p:~/x").map(drop).unwrap_err();
        assert_eq!(error.to_string(), "p is a proxy, not a bay");
    }

    #[test]
    fn ps_and_inspect_show_proxies() {
        let sandbox = settled();
        let hangar = proxied("cmd-ps", &sandbox);
        up(&hangar, &names(&["p"])).unwrap();
        let overview = ps(&hangar);
        let proxies: Vec<_> = overview
            .proxies
            .iter()
            .map(|p| (p.name.clone(), p.state, p.image.clone()))
            .collect();
        assert_eq!(
            proxies,
            [
                ("ext".into(), None, None),
                ("p".into(), Some(BoxState::Running), Some("fake:1".into()))
            ]
        );
        let Detail::Proxy(detail) = inspect(&hangar, "p").unwrap() else {
            panic!("not a proxy");
        };
        assert_eq!(detail.vm.as_deref(), Some("hangar-proxy-p"));
        assert_eq!(detail.log.unwrap().to_str(), Some("fake.log"));
        let Detail::Proxy(external) = inspect(&hangar, "ext").unwrap() else {
            panic!("not a proxy");
        };
        assert!(external.vm.is_none() && external.log.is_none());
    }

    const WEB: &str = r#"{"bays": [{"name": "default", "proxy": null,
        "apps": ["web"]}], "apps": {"web": {"run": "serve",
        "setup": {"command": "login", "check": "ok"}}}}"#;

    fn running_web(name: &str, sandbox: &Rc<FakeSandbox>) -> Hangar {
        sandbox.set("hangar-bay-default", crate::sandbox::BoxState::Running);
        hangar_with(WEB, &scratch_dir(name), sandbox.clone())
    }

    #[test]
    fn setup_runs_the_command_then_checks_again() {
        let sandbox = settled();
        let hangar = running_web("cmd-setup", &sandbox);
        sandbox.reply_once("hangar-setup-check", Some("needed"));
        let error = setup(&hangar, "default", "web", false).unwrap_err();
        assert_eq!(
            error.to_string(),
            "web's setup failed: login: run 'hangar setup default web' \
             to try again"
        );
        sandbox.shell_succeeds.set(true);
        setup(&hangar, "default", "web", true).unwrap();
        assert!(
            sandbox.changes().contains(
                &"shell pilot@hangar-bay-default sh -lc login".into()
            )
        );
    }

    #[test]
    fn logs_follow_a_bay_journal() {
        let sandbox = settled();
        let hangar = running_web("cmd-journal", &sandbox);
        let error = logs(&hangar, Some("default"), None, true).unwrap_err();
        assert_eq!(error.to_string(), "can't read the journal of bay default");
        sandbox.shell_succeeds.set(true);
        logs(&hangar, Some("default"), Some("web"), true).unwrap();
        let journal = sandbox.changes().pop().unwrap();
        assert_eq!(
            journal,
            "shell pilot@hangar-bay-default journalctl --user -u \
             hangar-run-web.service --no-pager -n 200 -f"
        );
    }

    #[test]
    fn logs_of_a_proxy() {
        let hangar = proxied("cmd-logs", &settled());
        let error = |name, app| {
            logs(&hangar, Some(name), app, false)
                .unwrap_err()
                .to_string()
        };
        assert_eq!(error("p", Some("web")), "proxy p has no apps");
        assert_eq!(
            error("ext", None),
            "proxy ext runs elsewhere; its log is there"
        );
        assert!(error("p", None).starts_with("fake.log"));
    }

    #[test]
    fn a_log_reached_through_a_symlink_is_refused() {
        let dir = scratch_dir("cmd-tail-link");
        fs::write(dir.join("real.log"), "line\n").unwrap();
        std::os::unix::fs::symlink(dir.join("real.log"), dir.join("proxy.log"))
            .unwrap();
        assert!(tail(&dir.join("real.log"), false).is_ok());
        let error = tail(&dir.join("proxy.log"), false).unwrap_err();
        assert!(error.to_string().contains("proxy.log"), "{error}");
    }

    #[test]
    fn logs_of_an_app_without_a_run_say_there_is_nothing_to_show() {
        let config = r#"{"bays": [{"name": "default", "proxy": null,
            "apps": ["login-only"]}], "apps": {"login-only":
            {"setup": {"command": "login", "check": "ok"}}}}"#;
        let sandbox = settled();
        sandbox.set("hangar-bay-default", crate::sandbox::BoxState::Running);
        let hangar = hangar_with(config, &scratch_dir("cmd-no-run"), sandbox);
        let error = logs(&hangar, Some("default"), Some("login-only"), false)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "login-only has no long-running unit in bay default, so it has \
             no log: it only runs setup ('hangar setup default login-only')"
        );
        let error =
            logs(&hangar, Some("default"), Some("other"), false).unwrap_err();
        assert!(error.to_string().contains("add it to apps"), "{error}");
    }

    #[test]
    fn last_lines_keeps_the_tail() {
        assert_eq!(last_lines(b"a\nb\nc\n", 2), b"b\nc\n");
        assert_eq!(last_lines(b"a\nb\nc", 2), b"b\nc");
        assert_eq!(last_lines(b"a\n", 5), b"a\n");
        assert_eq!(last_lines(b"", 5), b"");
    }
}
