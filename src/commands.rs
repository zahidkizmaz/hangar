//! What each command does. Commands return data for `output` to render;
//! `main` only parses arguments and dispatches here.

use std::collections::BTreeMap;
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
use crate::proxy::{Proxy, Secrets, managed, proxy_vm};
use crate::sandbox::{BoxState, Egress, bay_vm, data_volume};
use crate::status::{self, Detail, Overview};
use crate::{mounts, output, process};
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

#[derive(Default)]
pub(crate) struct UpReport {
    pub(crate) changes: Vec<Change>,
    /// Where to reach what came up: name -> addresses on 127.0.0.1.
    pub(crate) addresses: BTreeMap<String, Vec<String>>,
    /// `ps` for what changed, shown with each result; `None` keeps the
    /// plain change table (`rm`, `secret set`).
    pub(crate) overview: Option<Overview>,
}

/// The user's own config, or why the Nix module's is not theirs to write.
fn writable_config(paths: &Paths) -> Result<&Path> {
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
    Ok(&paths.user_config)
}

fn no_config(path: &Path) -> Error {
    Error::with_hint(
        format!("no config at {}", path.display()),
        "run 'hangar init'",
    )
}

/// Prints the config file as written.
pub(crate) fn config_show(paths: &Paths) -> Result<()> {
    let path = &paths.config;
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if matches!(error.kind(), io::ErrorKind::NotFound) => {
            return Err(no_config(path));
        }
        Err(error) => {
            return Err(Error::new(format!("{}: {error}", path.display())));
        }
    };
    let mut stdout = io::stdout().lock();
    stdout.write_all(text.as_bytes()).context("stdout")?;
    if !text.ends_with('\n') {
        writeln!(stdout).context("stdout")?;
    }
    Ok(())
}

/// Opens the config in `editor` (default `vi`), then checks what was saved.
pub(crate) fn config_edit(paths: &Paths, editor: Option<&str>) -> Result<()> {
    let path = writable_config(paths)?;
    if !path.exists() {
        return Err(no_config(path));
    }
    let editor = editor.unwrap_or("vi");
    let mut words = editor.split_whitespace();
    let program = words.next().context("the editor is empty")?;
    let mut command = process::command(program);
    command.args(words).arg(path);
    let status = process::status(&mut command)?;
    if !status.success() {
        bail!("{editor} exited with {status}; the config wasn't checked");
    }
    match Hangar::load(paths) {
        Ok(_) => {
            info!("{} is valid: run 'hangar up' to apply it", path.display());
            Ok(())
        }
        Err(error) => Err(Error::with_hint(
            format!("{}: {}", path.display(), error.message()),
            error.hint().unwrap_or("run 'hangar config edit' to fix it"),
        )),
    }
}

pub(crate) fn init(paths: &Paths, force: bool) -> Result<()> {
    let path = writable_config(paths)?;
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

/// Names for a heading, `nothing` when there are none.
fn listed_names(names: &[&str]) -> String {
    if names.is_empty() {
        "nothing".into()
    } else {
        names.join(", ")
    }
}

/// One thing `up` brings up.
pub(crate) enum Step<'a> {
    Proxy(&'a str, &'a dyn Proxy),
    Bay(Bay<'a>),
}

/// The bays a proxy needs up first: a managed proxy reaching a port on
/// this machine needs the bay that publishes it.
fn upstream<'a>(hangar: &'a Hangar, proxy: &dyn Proxy) -> Vec<&'a str> {
    let egress = proxy.managed().map(|managed| managed.vm().egress());
    let Some(Egress::OpenAndHostPort(port)) = egress else {
        return Vec::new();
    };
    hangar
        .settings
        .bays
        .iter()
        .filter(|bay| bay.ports.iter().any(|published| published.host == port))
        .map(|bay| bay.name.as_str())
        .collect()
}

/// The named bays and proxies plus everything they need, each after what
/// it needs: a bay after its proxy, a proxy after its upstream bays.
pub(crate) fn plan_up<'a>(
    hangar: &'a Hangar,
    names: &[String],
) -> Result<Vec<Step<'a>>> {
    let mut selection = select(hangar, names)?;
    loop {
        let mut grew = false;
        let needed: Vec<&str> = selection
            .proxies
            .iter()
            .flat_map(|(_, proxy)| upstream(hangar, *proxy))
            .collect();
        for name in needed {
            let known = selection.bays.iter().any(|bay| bay.name == name);
            if let (false, Some(bay)) = (known, hangar.bay(name)) {
                selection.bays.push(bay);
                grew = true;
            }
        }
        let used: Vec<_> = selection
            .bays
            .iter()
            .filter_map(|bay| hangar.proxy_of(bay.settings))
            .collect();
        for (name, proxy) in used {
            if !selection.proxies.iter().any(|(known, _)| *known == name) {
                selection.add_proxy(name, proxy);
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    let (mut proxies, mut bays) = (selection.proxies, selection.bays);
    let mut done: Vec<&str> = Vec::new();
    let mut steps = Vec::new();
    while !proxies.is_empty() || !bays.is_empty() {
        let before = proxies.len() + bays.len();
        let (ready, waiting): (Vec<_>, Vec<_>) =
            proxies.into_iter().partition(|(_, proxy)| {
                upstream(hangar, *proxy)
                    .iter()
                    .all(|bay| done.contains(bay))
            });
        proxies = waiting;
        for (name, proxy) in ready {
            done.push(name);
            steps.push(Step::Proxy(name, proxy));
        }
        let (ready, waiting): (Vec<_>, Vec<_>) =
            bays.into_iter().partition(|bay| {
                hangar
                    .proxy_of(bay.settings)
                    .is_none_or(|(proxy, _)| done.contains(&proxy))
            });
        bays = waiting;
        for bay in ready {
            done.push(bay.name);
            steps.push(Step::Bay(bay));
        }
        if proxies.len() + bays.len() == before {
            let stuck: Vec<&str> = proxies
                .iter()
                .map(|(name, _)| *name)
                .chain(bays.iter().map(|bay| bay.name))
                .collect();
            bail!(
                "{} need each other, so none can come up first",
                stuck.join(", ")
            );
        }
    }
    Ok(steps)
}

/// Everything named and what it needs, in order; whatever a failed one
/// was needed by is left alone. A proxy's own tokens are asked for just
/// before it starts, its bays' tokens just before a bay of it does.
pub(crate) fn up(
    hangar: &Hangar,
    names: &[String],
    prompt: &dyn Prompt,
) -> Result<UpReport> {
    let mut changes: Vec<Change> = Vec::new();
    let mut asked: Vec<&str> = Vec::new();
    let steps = plan_up(hangar, names)?;
    let touching: Vec<&str> = steps
        .iter()
        .map(|step| match step {
            Step::Proxy(name, _) => *name,
            Step::Bay(bay) => bay.name,
        })
        .collect();
    crate::logger::heading(&format!("Starting {}", listed_names(&touching)));
    for step in steps {
        match step {
            Step::Proxy(name, proxy) => {
                let down = upstream(hangar, proxy)
                    .into_iter()
                    .find(|bay| failed(&changes, Resource::Bay, bay));
                let outcome = if let Some(bay) = down {
                    Outcome::Failed(Error::new(format!("bay {bay} failed")))
                } else {
                    let wanted = slot_tokens(proxy, false);
                    ask_missing(hangar, name, &wanted, false, prompt)?;
                    managed::up(hangar, name, proxy)
                };
                changes.push(change(name, Resource::Proxy, outcome));
            }
            Step::Bay(bay) => {
                let outcome = match hangar.proxy_of(bay.settings) {
                    Some((proxy, _))
                        if failed(&changes, Resource::Proxy, proxy) =>
                    {
                        Outcome::Failed(Error::new(format!(
                            "proxy {proxy} failed"
                        )))
                    }
                    Some((name, proxy)) => {
                        let wanted = match own_token(proxy, bay.name) {
                            Some(token) => vec![token],
                            None if asked.contains(&name) => Vec::new(),
                            None => {
                                asked.push(name);
                                slot_tokens(proxy, true)
                            }
                        };
                        ask_missing(hangar, name, &wanted, true, prompt)?;
                        bay::up(hangar, &bay)
                    }
                    None => bay::up(hangar, &bay),
                };
                changes.push(change(bay.name, Resource::Bay, outcome));
            }
        }
    }
    let addresses = changes
        .iter()
        .filter(|change| !matches!(change.outcome, Outcome::Failed(_)))
        .map(|change| (change.name.clone(), addresses(hangar, change)))
        .filter(|(_, list)| !list.is_empty())
        .collect();
    let overview = touched(hangar, &changes);
    Ok(UpReport {
        changes,
        addresses,
        overview: Some(overview),
    })
}

/// `ps` narrowed to the bays and proxies in `changes`.
fn touched(hangar: &Hangar, changes: &[Change]) -> Overview {
    let names: Vec<&str> =
        changes.iter().map(|change| change.name.as_str()).collect();
    status::of(hangar, &names)
}

/// Where to reach a bay's apps or a proxy, on the host's 127.0.0.1.
fn addresses(hangar: &Hangar, change: &Change) -> Vec<String> {
    match change.resource {
        Resource::Bay => {
            hangar.bay(&change.name).map_or_else(Vec::new, |bay| {
                bay.settings
                    .ports
                    .iter()
                    .map(output::port_address)
                    .collect()
            })
        }
        Resource::Proxy => {
            let Some(proxy) = hangar.proxies.get(&change.name) else {
                return Vec::new();
            };
            match proxy.managed() {
                Some(managed) => managed
                    .vm()
                    .publish
                    .iter()
                    .map(output::proxy_address)
                    .collect(),
                None => match proxy.egress() {
                    Egress::HostPort(port) => vec![format!("127.0.0.1:{port}")],
                    Egress::Open | Egress::OpenAndHostPort(_) => Vec::new(),
                },
            }
        }
    }
}

/// A token to ask for: its keychain key and what it is.
struct Token {
    key: String,
    about: String,
}

/// The proxy's own tokens, or with `for_bays` the ones its bays share.
fn slot_tokens(proxy: &dyn Proxy, for_bays: bool) -> Vec<Token> {
    proxy
        .secrets()
        .iter()
        .filter(|slot| slot.for_bays == for_bays)
        .map(|slot| Token {
            key: slot.key.into(),
            about: slot.about.into(),
        })
        .collect()
}

/// `bay`'s own token, when the proxy gives it one.
fn own_token(proxy: &dyn Proxy, bay: &str) -> Option<Token> {
    proxy
        .bay_tokens()
        .into_iter()
        .find(|(owner, _)| owner == bay)
        .map(|(_, key)| Token {
            key,
            about: match proxy.secrets().iter().find(|slot| slot.for_bays) {
                Some(slot) => format!("{}; only for bay {bay}", slot.about),
                None => format!("the token bay {bay} logs in with"),
            },
        })
}

/// Every token of `proxy`: its slots and its bays' own.
fn all_tokens(proxy: &dyn Proxy) -> Vec<Token> {
    let mut tokens = slot_tokens(proxy, false);
    tokens.extend(slot_tokens(proxy, true));
    for (bay, _) in proxy.bay_tokens() {
        tokens.extend(own_token(proxy, &bay));
    }
    tokens
}

/// Asks for those of `wanted` missing from the keychain. A bay's token
/// (`for_bays`) may be skipped with an empty answer, since it can need a
/// server that is only now up.
fn ask_missing(
    hangar: &Hangar,
    name: &str,
    wanted: &[Token],
    for_bays: bool,
    prompt: &dyn Prompt,
) -> Result<()> {
    if !prompt.terminal() {
        return Ok(());
    }
    let secrets = hangar.secrets(name)?;
    for token in wanted {
        if secrets.has(&token.key)? {
            continue;
        }
        let mut question = format!("{name} {} ({})", token.key, token.about);
        if for_bays {
            question.push_str("; Enter skips it");
        }
        let answer = prompt.ask(&question)?;
        if for_bays && answer.expose().is_empty() {
            info!("proxy {name}: {} skipped", token.key);
            continue;
        }
        secrets.store(&token.key, &answer)?;
        info!("proxy {name}: {} saved", token.key);
    }
    Ok(())
}

/// Stores each of `tokens`, asked for one by one; what was saved.
fn ask_for(
    proxy: &str,
    secrets: &Secrets,
    tokens: &[Token],
    prompt: &dyn Prompt,
) -> Result<Vec<String>> {
    tokens
        .iter()
        .map(|token| {
            let question = format!("{proxy} {} ({})", token.key, token.about);
            secrets.store(&token.key, &prompt.ask(&question)?)?;
            info!("proxy {proxy}: {} saved", token.key);
            Ok(format!("{} saved", token.key))
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
    let mut tokens: Vec<Token> = all_tokens(proxy)
        .into_iter()
        .filter(|token| !secrets.is_command(&token.key))
        .collect();
    if tokens.is_empty() {
        bail!("proxy {name} keeps no tokens in the keychain");
    }
    if let Some(wanted) = token {
        let keys: Vec<String> =
            tokens.iter().map(|token| token.key.clone()).collect();
        tokens.retain(|token| token.key == wanted);
        if tokens.is_empty() {
            return Err(Error::with_hint(
                format!("proxy {name} keeps no {wanted} in the keychain"),
                format!("it keeps: {}", keys.join(", ")),
            ));
        }
    }
    let saved = ask_for(name, &secrets, &tokens, prompt)?;
    Ok(UpReport {
        changes: vec![change(name, Resource::Proxy, Outcome::Updated(saved))],
        ..UpReport::default()
    })
}

fn failed(changes: &[Change], resource: Resource, name: &str) -> bool {
    changes.iter().any(|change| {
        change.resource == resource
            && change.name == name
            && matches!(change.outcome, Outcome::Failed(_))
    })
}

/// The bays first, then the proxies.
pub(crate) fn down(hangar: &Hangar, names: &[String]) -> Result<UpReport> {
    let selection = select(hangar, names)?;
    let touching: Vec<&str> = selection
        .bays
        .iter()
        .map(|bay| bay.name)
        .chain(selection.proxies.iter().map(|(name, _)| *name))
        .collect();
    crate::logger::heading(&format!("Stopping {}", listed_names(&touching)));
    let bays = selection.bays.iter().map(|bay| {
        let outcome = bay::down(hangar, bay).unwrap_or_else(Outcome::Failed);
        change(bay.name, Resource::Bay, outcome)
    });
    let proxies = selection.proxies.iter().map(|(name, proxy)| {
        let outcome =
            managed::down(hangar, name, *proxy).unwrap_or_else(Outcome::Failed);
        change(name, Resource::Proxy, outcome)
    });
    let changes: Vec<Change> = bays.chain(proxies).collect();
    let overview = Some(touched(hangar, &changes));
    Ok(UpReport {
        changes,
        overview,
        ..UpReport::default()
    })
}

pub(crate) fn restart(
    hangar: &Hangar,
    names: &[String],
    prompt: &dyn Prompt,
) -> Result<UpReport> {
    let mut report = down(hangar, names)?;
    let up = up(hangar, names, prompt)?;
    report.changes.extend(up.changes);
    report.addresses = up.addresses;
    report.overview = up.overview;
    Ok(report)
}

/// What `rm` deletes: the VM, and with `--data` the folder, the data
/// volume and a configured proxy's tokens in the keychain.
struct Doomed {
    resource: Resource,
    vm: String,
    folder: PathBuf,
    volume: Option<String>,
    tokens: Option<(Secrets, Vec<String>)>,
}

/// Configured or left over: a leftover is only known by its folder.
fn doomed(hangar: &Hangar, name: &str) -> Result<Doomed> {
    let mut tokens = None;
    let resource = match target(hangar, name) {
        Ok(Target::Bay(_)) => Resource::Bay,
        Ok(Target::Proxy(name, proxy)) => {
            let secrets = hangar.secrets(name)?;
            let keys: Vec<String> = all_tokens(proxy)
                .into_iter()
                .map(|token| token.key)
                .filter(|key| !secrets.is_command(key))
                .collect();
            tokens = (!keys.is_empty()).then_some((secrets, keys));
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
        ..UpReport::default()
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
    if let Some((secrets, keys)) = &doomed.tokens {
        secrets.delete(keys)?;
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
    fn a_token_only_bays_need_is_asked_when_one_of_its_bays_comes_up() {
        let keychain = Rc::new(FakeKeychain::default());
        let config = r#"{"proxies": {"v": {"kind": "fake", "run": "managed",
            "managed": true, "token": true, "bayToken": true, "port": 3100}},
            "bays": [{"name": "w", "proxy": "v"}]}"#;
        let state = scratch_dir("cmd-up-bay-token");
        let hangar =
            hangar_holding(config, &state, settled(), keychain.clone());
        let start = FakePrompt::answering(&["start-1"]);
        super::up(&hangar, &names(&["v"]), &start).unwrap();
        assert_eq!(*start.questions.borrow(), ["v token (the fake token)"]);
        let bay = FakePrompt::answering(&["bay-1"]);
        super::up(&hangar, &names(&["w"]), &bay).unwrap();
        assert_eq!(
            *bay.questions.borrow(),
            ["v bay-token (the fake bay token); Enter skips it"]
        );
        assert_eq!(keychain.items.borrow()["v/bay-token"], "bay-1");
    }

    #[test]
    fn a_skipped_bay_token_is_asked_once_and_never_stored() {
        let keychain =
            Rc::new(FakeKeychain::holding(&[("v/token", "start-1")]));
        let config = r#"{"proxies": {"v": {"kind": "fake", "run": "managed",
            "managed": true, "token": true, "bayToken": true, "port": 3100}},
            "bays": [{"name": "w", "proxy": "v"}, {"name": "x", "proxy": "v"},
                     {"name": "d", "proxy": null}]}"#;
        let state = scratch_dir("cmd-up-skip");
        let hangar =
            hangar_holding(config, &state, settled(), keychain.clone());
        let prompt = FakePrompt::answering(&[""]);
        let report = super::up(&hangar, &[], &prompt).unwrap();
        assert_eq!(
            *prompt.questions.borrow(),
            ["v bay-token (the fake bay token); Enter skips it"]
        );
        assert!(!keychain.items.borrow().contains_key("v/bay-token"));
        assert_eq!(summary(&report).len(), 4);
    }

    /// `p`'s upstream is port 9000, which bay `srv` publishes.
    const UPSTREAM: &str = r#"{"proxies": {"p": {"kind": "fake",
        "run": "managed", "managed": true, "port": 3100, "upstream": 9000}},
        "apps": {"api": {"run": "serve",
            "ports": [{"name": "api", "vm": 9000, "purpose": "API"}]}},
        "bays": [{"name": "w", "proxy": "p"},
                 {"name": "srv", "proxy": null, "apps": ["api"]}]}"#;

    #[test]
    fn up_brings_a_proxys_upstream_bay_up_before_it() {
        let sandbox = settled();
        let hangar = hangar_with(UPSTREAM, &scratch_dir("cmd-up-dep"), sandbox);
        let report = up(&hangar, &names(&["w"])).unwrap();
        let order: Vec<_> = report
            .changes
            .iter()
            .map(|change| change.name.as_str())
            .collect();
        assert_eq!(order, ["srv", "p", "w"]);
        let all = up(&hangar, &[]).unwrap();
        let order: Vec<_> = all
            .changes
            .iter()
            .map(|change| change.name.as_str())
            .collect();
        assert_eq!(order, ["srv", "p", "w"]);
    }

    #[test]
    fn a_failed_upstream_bay_fails_its_proxy_and_that_proxys_bays() {
        let sandbox = settled();
        sandbox.fail_create.set(true);
        let hangar =
            hangar_with(UPSTREAM, &scratch_dir("cmd-up-dep-fail"), sandbox);
        let summary = summary(&up(&hangar, &[]).unwrap());
        assert!(summary[0].starts_with("srv bay Failed"), "{summary:?}");
        assert_eq!(
            summary[1],
            r#"p proxy Failed(Error { message: "bay srv failed", hint: None })"#
        );
        assert_eq!(
            summary[2],
            r#"w bay Failed(Error { message: "proxy p failed", hint: None })"#
        );
    }

    #[test]
    fn a_bay_and_proxy_that_need_each_other_are_refused() {
        let config = UPSTREAM.replace(
            r#"{"name": "srv", "proxy": null"#,
            r#"{"name": "srv", "proxy": "p""#,
        );
        let hangar =
            hangar_with(&config, &scratch_dir("cmd-up-cycle"), settled());
        let error = up(&hangar, &[]).err().unwrap();
        assert_eq!(
            error.to_string(),
            "p, w, srv need each other, so none can come up first"
        );
    }

    /// `w` shares `v`'s bay token, `x` has its own.
    const OWN: &str = r#"{"proxies": {"v": {"kind": "fake", "run": "managed",
        "managed": true, "bayToken": true, "ownTokens": ["x"],
        "port": 3100}},
        "bays": [{"name": "w", "proxy": "v"}, {"name": "x", "proxy": "v"},
                 {"name": "y", "proxy": "v"}]}"#;

    fn owning(
        name: &str,
        items: &[(&str, &str)],
    ) -> (Hangar, Rc<FakeKeychain>) {
        let keychain = Rc::new(FakeKeychain::holding(items));
        let state = scratch_dir(name);
        let hangar = hangar_holding(OWN, &state, settled(), keychain.clone());
        (hangar, keychain)
    }

    #[test]
    fn a_bay_with_its_own_token_is_asked_for_it_and_never_shares() {
        let (hangar, keychain) = owning("cmd-own-up", &[("v/token", "t")]);
        let prompt = FakePrompt::answering(&["shared-1", "own-x"]);
        super::up(&hangar, &[], &prompt).unwrap();
        assert_eq!(
            *prompt.questions.borrow(),
            [
                "v bay-token (the fake bay token); Enter skips it",
                "v token@x (the fake bay token; only for bay x); Enter skips it",
            ]
        );
        let items = keychain.items.borrow();
        assert_eq!(items["v/bay-token"], "shared-1");
        assert_eq!(items["v/token@x"], "own-x");
    }

    #[test]
    fn secret_set_rm_and_inspect_know_a_bays_own_token() {
        let (hangar, keychain) = owning("cmd-own-set", &[]);
        let one = FakePrompt::answering(&["own-x"]);
        secret_set(&hangar, "v", Some("token@x"), &one).unwrap();
        assert_eq!(keychain.items.borrow()["v/token@x"], "own-x");
        let Detail::Proxy(detail) = inspect(&hangar, "v").unwrap() else {
            panic!("not a proxy");
        };
        assert_eq!(
            detail.tokens,
            [
                ("token".into(), TokenSource::Missing),
                ("bay-token".into(), TokenSource::Missing),
                ("token@x".into(), TokenSource::Keychain),
            ]
        );
        let unknown = FakePrompt::answering(&[]);
        let error = secret_set(&hangar, "v", Some("token@w"), &unknown)
            .err()
            .unwrap();
        assert_eq!(error.hint(), Some("it keeps: token, bay-token, token@x"));
        rm(&hangar, "v", true, true, &no_prompt()).unwrap();
        assert!(keychain.items.borrow().is_empty());
    }

    #[test]
    fn a_bay_whose_login_the_proxy_refuses_fails_with_why_and_the_fix() {
        let (port, _server) = crate::testing::serve_each(vec![(
            "407 Proxy Authentication Required",
            "invalid or expired session",
        )]);
        let config = format!(
            r#"{{"proxies": {{"p": {{"kind": "fake", "port": {port},
                "url": "http://127.0.0.1:{port}", "login": "u:pw",
                "bayToken": true}}}},
                "bays": [{{"name": "a", "proxy": "p"}}]}}"#
        );
        let sandbox = settled();
        let hangar = hangar_with(&config, &scratch_dir("cmd-login"), sandbox);
        let report = up(&hangar, &names(&["a"])).unwrap();
        let crate::lifecycle::Outcome::Failed(error) =
            &report.changes[1].outcome
        else {
            panic!("{:?}", summary(&report));
        };
        assert_eq!(
            error.message(),
            "bay a: proxy env: proxy p refused the login of bay a: invalid or \
             expired session"
        );
        assert_eq!(error.hint(), Some("run 'hangar secret set p bay-token'"));
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
            (
                vec!["a".to_string()],
                vec![("token".into(), TokenSource::Missing)]
            )
        );
        keychain
            .items
            .borrow_mut()
            .insert("t/token".into(), "x".into());
        assert_eq!(sources("t").1, [("token".into(), TokenSource::Keychain)]);
        assert_eq!(
            sources("c"),
            (vec![], vec![("token".into(), TokenSource::Command)])
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
        // Read for the summary, never changed.
        assert!(!sandbox.changes().iter().any(|c| c.contains("hangar-bay-a")));
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
