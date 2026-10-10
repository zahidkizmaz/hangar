//! `hangar credential`: the user's own credentials in the broker, outside
//! the config. `up` never deletes them; it only manages `credentialFiles`.

use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Read};
use std::process::Stdio;
use std::thread;
use std::time::Duration;

use crate::broker::{
    Broker, ClientSecret, OAuthClient, OAuthLogin, StoredCredential,
};
use crate::config::{Settings, Source, managed_credentials, valid_key};
use crate::error::{Context, Error, Result, bail};
use crate::hangar::Hangar;
use crate::http::Https;
use crate::oauth;
use crate::process;
use crate::secret::{Secret, trim_line_end};
use crate::url::query_param;

// Echo comes back even on Ctrl-C (the trap runs in this shell). The value
// leaves on stdout, a pipe to hangar, never as an argument.
const READ_HIDDEN: &str = r#"trap 'stty echo' EXIT INT TERM
printf '%s: ' "$1" >&2
stty -echo
IFS= read -r value
printf '\n' >&2
printf '%s' "$value""#;

/// `credential set`, first half: the checks and the value.
pub(crate) fn read(hangar: &Hangar, name: &str) -> Result<Secret> {
    check_user_key(&hangar.settings, name)?;
    vault_ready(hangar)?;
    if stored(hangar, name)?.is_some_and(|stored| stored.oauth.is_some()) {
        return Err(Error::with_hint(
            format!("{name} is an OAuth credential"),
            format!("run 'hangar credential login {name}'"),
        ));
    }
    let value = read_value(name)?;
    if value.expose().is_empty() {
        bail!("{name}: empty value, nothing stored");
    }
    Ok(value)
}

/// `credential set`, second half, under the lock.
pub(crate) fn store(hangar: &Hangar, name: &str, value: &Secret) -> Result<()> {
    hangar.broker.put_credential(name, value)?;
    log::info!("stored {name}");
    Ok(())
}

fn stored(hangar: &Hangar, name: &str) -> Result<Option<StoredCredential>> {
    let credentials = hangar.broker.credentials()?;
    Ok(credentials.into_iter().find(|stored| stored.key == name))
}

pub(crate) struct CredentialList {
    pub(crate) entries: Vec<CredentialEntry>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CredentialEntry {
    pub(crate) name: String,
    /// `config` or `user`.
    pub(crate) source: &'static str,
    pub(crate) oauth: bool,
    pub(crate) state: String,
}

impl CredentialEntry {
    pub(crate) fn kind(&self) -> &'static str {
        if self.oauth { "oauth" } else { "static" }
    }
}

pub(crate) fn list(hangar: &Hangar) -> Result<CredentialList> {
    vault_ready(hangar)?;
    let credentials = hangar.broker.credentials()?;
    Ok(classify(
        credentials,
        &managed_credentials(&hangar.settings),
    ))
}

fn classify(
    credentials: Vec<StoredCredential>,
    managed: &BTreeMap<String, Source>,
) -> CredentialList {
    let entries = credentials
        .into_iter()
        .map(|stored| {
            let source = if managed.contains_key(&stored.key) {
                "config"
            } else {
                "user"
            };
            CredentialEntry {
                state: state(stored.oauth.as_ref()),
                oauth: stored.oauth.is_some(),
                name: stored.key,
                source,
            }
        })
        .collect();
    CredentialList { entries }
}

/// A refresh error is the token endpoint's answer: one line, cut short.
fn state(oauth: Option<&OAuthLogin>) -> String {
    let Some(login) = oauth else {
        return "set".into();
    };
    if let Some(error) = &login.error {
        let line = error.lines().next().unwrap_or_default();
        let short: String = line.chars().take(80).collect();
        return format!("oauth: refresh failed ({short})");
    }
    if login.connected {
        "oauth: connected".into()
    } else {
        "oauth: not connected".into()
    }
}

pub(crate) fn remove(hangar: &Hangar, name: &str) -> Result<()> {
    check_user_key(&hangar.settings, name)?;
    vault_ready(hangar)?;
    hangar.broker.delete_credentials(&[name.to_string()])?;
    log::info!("removed {name}");
    Ok(())
}

/// Where a login's client comes from.
pub(crate) enum ClientSource {
    /// The provider behind a URL; a client is registered unless given.
    Discover {
        url: String,
        client_id: Option<String>,
    },
    /// A client the user registered with the provider.
    Endpoints {
        authorization_url: String,
        token_url: String,
        client_id: String,
    },
    /// The client the broker holds.
    Stored,
}

impl ClientSource {
    /// clap keeps the endpoint flags together and apart from a URL; a
    /// client id alone is all it lets through.
    pub(crate) fn from_flags(
        url: Option<String>,
        authorization_url: Option<String>,
        token_url: Option<String>,
        client_id: Option<String>,
    ) -> Result<Self> {
        Ok(match (url, authorization_url, token_url, client_id) {
            (Some(url), None, None, client_id) => {
                Self::Discover { url, client_id }
            }
            (
                None,
                Some(authorization_url),
                Some(token_url),
                Some(client_id),
            ) => Self::Endpoints {
                authorization_url,
                token_url,
                client_id,
            },
            (None, None, None, None) => Self::Stored,
            _ => bail!(
                "--client-id needs a URL, or --authorization-url and --token-url"
            ),
        })
    }
}

pub(crate) struct LoginArgs {
    pub(crate) source: ClientSource,
    pub(crate) scopes: Vec<String>,
    /// Read the client's secret like `credential set` reads a value.
    pub(crate) client_secret: bool,
}

/// A login whose checks passed, ready to connect.
pub(crate) struct Login {
    name: String,
    client: OAuthClient,
    secret: ClientSecret,
    /// When the tokens were last renewed: the login is done once this
    /// changes, whatever the tower's clock says.
    before: Option<String>,
}

/// `credential login`, first part: the checks, the client and its
/// secret. Discovery and registration talk to the provider, never the
/// broker.
pub(crate) fn prepare_login(
    hangar: &Hangar,
    https: &dyn Https,
    name: &str,
    args: LoginArgs,
) -> Result<Login> {
    check_user_key(&hangar.settings, name)?;
    vault_ready(hangar)?;
    let stored = stored(hangar, name)?;
    if stored.as_ref().is_some_and(|stored| stored.oauth.is_none()) {
        return Err(Error::with_hint(
            format!("{name} holds a static value"),
            format!("run 'hangar credential rm {name}' first"),
        ));
    }
    let login = stored.and_then(|stored| stored.oauth).unwrap_or_default();
    let mut client = match args.source {
        ClientSource::Stored => {
            let (mut client, secret) = stored_client(name, &login)?;
            if !args.scopes.is_empty() {
                client.scopes = args.scopes.join(" ");
            }
            return Ok(Login {
                name: name.to_string(),
                client,
                secret,
                before: login.refreshed_at,
            });
        }
        ClientSource::Endpoints {
            authorization_url,
            token_url,
            client_id,
        } => {
            oauth::check_url(&authorization_url)?;
            oauth::check_url(&token_url)?;
            OAuthClient {
                authorization_url,
                token_url,
                client_id,
                ..OAuthClient::default()
            }
        }
        ClientSource::Discover { url, client_id } => {
            let provider = oauth::discover(https, &url)?;
            log::info!("found the OAuth endpoints for {url}");
            let client_id = if let Some(id) = client_id {
                id
            } else {
                let callback = hangar.broker.oauth_redirect_uri()?;
                let id = oauth::register(https, &provider, &callback)?;
                log::info!("registered hangar as OAuth client {id}");
                id
            };
            OAuthClient {
                authorization_url: provider.authorization_url,
                token_url: provider.token_url,
                client_id,
                ..OAuthClient::default()
            }
        }
    };
    client.scopes = args.scopes.join(" ");
    let secret = if args.client_secret {
        ClientSecret::New(read_client_secret(name)?)
    } else {
        ClientSecret::None
    };
    Ok(Login {
        name: name.to_string(),
        client,
        secret,
        before: login.refreshed_at,
    })
}

/// A re-login keeps the client secret the broker holds.
fn stored_client(
    name: &str,
    login: &OAuthLogin,
) -> Result<(OAuthClient, ClientSecret)> {
    let client = login
        .client
        .clone()
        .filter(|client| !client.authorization_url.is_empty());
    let Some(client) = client else {
        return Err(Error::with_hint(
            format!("{name} has no OAuth client to log in with"),
            "pass --authorization-url, --token-url and --client-id",
        ));
    };
    let secret = if login.has_secret {
        ClientSecret::Keep
    } else {
        ClientSecret::None
    };
    Ok((client, secret))
}

fn read_client_secret(name: &str) -> Result<Secret> {
    let secret = read_value(&format!("{name} client secret"))?;
    if secret.expose().is_empty() {
        bail!("{name}: empty client secret");
    }
    Ok(secret)
}

/// `credential login`, second part, under the lock: the consent URL.
pub(crate) fn connect(hangar: &Hangar, login: &Login) -> Result<String> {
    hangar
        .broker
        .oauth_connect(&login.name, &login.client, &login.secret)
}

/// How long the vault keeps a login's state.
const WAIT: Duration = Duration::from_secs(600);
const POLL: Duration = Duration::from_secs(1);

/// `credential login`, last part: the browser, then the wait. The URL
/// carries the login's state, so it goes to the terminal only, never
/// to a log or `--json`.
pub(crate) fn finish_login(
    hangar: &Hangar,
    login: &Login,
    consent: &str,
    open: &dyn Fn(&str),
) -> Result<()> {
    check_consent(consent, &hangar.broker.oauth_redirect_uri()?)?;
    if io::stderr().is_terminal() {
        eprintln!("Log {} in at:\n  {consent}", login.name);
    }
    open(consent);
    log::info!("waiting for the login in your browser");
    let attempts = u32::try_from(WAIT.as_secs() / POLL.as_secs()).unwrap_or(1);
    wait_for_tokens(
        hangar.broker.as_ref(),
        &login.name,
        login.before.as_deref(),
        attempts,
        POLL,
    )?;
    log::info!("logged in {}", login.name);
    Ok(())
}

/// A tower started before it learned its host address sends logins
/// back to an address the browser can't reach.
fn check_consent(consent: &str, callback: &str) -> Result<()> {
    if !consent.starts_with("https://") {
        bail!("the vault's consent URL isn't https; not opening it");
    }
    match query_param(consent, "redirect_uri") {
        Some(redirect) if redirect == callback => Ok(()),
        redirect => Err(Error::with_hint(
            format!(
                "the vault sends logins back to {}, not {callback}",
                redirect.as_deref().unwrap_or("nowhere")
            ),
            "restart the tower: hangar down && hangar up",
        )),
    }
}

/// Errors are ignored: one from an earlier login must not end this one.
fn wait_for_tokens(
    broker: &dyn Broker,
    name: &str,
    before: Option<&str>,
    attempts: u32,
    pause: Duration,
) -> Result<()> {
    for attempt in 0..attempts {
        if attempt > 0 {
            thread::sleep(pause);
        }
        let renewed = broker
            .credentials()?
            .into_iter()
            .find(|stored| stored.key == name)
            .and_then(|stored| stored.oauth)
            .and_then(|login| login.refreshed_at);
        if renewed.is_some() && renewed.as_deref() != before {
            return Ok(());
        }
    }
    Err(Error::with_hint(
        format!("no login for {name} yet"),
        "the browser page shows the result; check 'hangar credential list'",
    ))
}

fn check_user_key(settings: &Settings, name: &str) -> Result<()> {
    if !valid_key(name) {
        bail!("{name}: expected UPPER_SNAKE_CASE");
    }
    match managed_credentials(settings).get(name) {
        Some(Source::File(_)) => Err(Error::with_hint(
            format!("{name} is managed by tower.credentialFiles"),
            "change your config",
        )),
        Some(Source::App { app, .. }) => Err(Error::with_hint(
            format!("{name} is set by app {app}"),
            "a route uses the app's fixed value",
        )),
        None => Ok(()),
    }
}

fn vault_ready(hangar: &Hangar) -> Result<()> {
    if !hangar.broker.health().healthy {
        return Err(Error::with_hint(
            "the vault isn't running",
            "run 'hangar up' first",
        ));
    }
    Ok(())
}

fn read_value(name: &str) -> Result<Secret> {
    let value = if io::stdin().is_terminal() {
        read_hidden(name, Stdio::inherit())?
    } else {
        let mut value = String::new();
        io::stdin().read_to_string(&mut value).context(name)?;
        value
    };
    Ok(Secret::new(trim_line_end(&value).to_string()))
}

fn read_hidden(name: &str, input: Stdio) -> Result<String> {
    let mut read = process::command("sh");
    read.args(["-c", READ_HIDDEN, "hangar", name])
        .stdin(input)
        .stderr(Stdio::inherit());
    let output = read
        .stdout(Stdio::piped())
        .output()
        .map_err(|error| process::spawn_error(&read, &error))?;
    String::from_utf8(output.stdout).context(name)
}

#[cfg(test)]
mod tests {
    use super::{
        ClientSource, CredentialEntry, LoginArgs, check_consent,
        check_user_key, classify, connect, finish_login, list, prepare_login,
        read, read_hidden, remove, state, wait_for_tokens,
    };
    use crate::broker::fake::{FAKE_CALLBACK, FakeBroker};
    use crate::broker::{OAuthClient, OAuthLogin, StoredCredential};
    use crate::config::Source;
    use crate::hangar::Hangar;
    use crate::http::fake::FakeHttps;
    use crate::sandbox::fake::FakeSandbox;
    use crate::testing::{hangar_with_broker, scratch_dir, settings};
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::fs::{self, File};
    use std::process::Stdio;
    use std::rc::Rc;
    use std::time::Duration;

    fn entry(
        name: &str,
        source: &'static str,
        oauth: bool,
        state: &str,
    ) -> CredentialEntry {
        CredentialEntry {
            name: name.into(),
            source,
            oauth,
            state: state.into(),
        }
    }

    #[test]
    fn credentials_the_config_manages_are_marked_config() {
        let managed = BTreeMap::from([(
            "GITHUB_TOKEN".to_string(),
            Source::File("/t".into()),
        )]);
        let stored = |key: &str, oauth: Option<OAuthLogin>| StoredCredential {
            key: key.into(),
            oauth,
        };
        let list = classify(
            vec![
                stored("GITHUB_TOKEN", None),
                stored("MINE", Some(OAuthLogin::default())),
            ],
            &managed,
        );
        assert_eq!(
            list.entries,
            [
                entry("GITHUB_TOKEN", "config", false, "set"),
                entry("MINE", "user", true, "oauth: not connected"),
            ]
        );
        assert_eq!(list.entries[0].kind(), "static");
        assert_eq!(list.entries[1].kind(), "oauth");
    }

    #[test]
    fn an_oauth_state_says_whether_it_is_connected_or_why_not() {
        let login = |connected, error: Option<&str>| OAuthLogin {
            connected,
            refreshed_at: Some("t".into()),
            error: error.map(Into::into),
            ..OAuthLogin::default()
        };
        assert_eq!(state(None), "set");
        assert_eq!(state(Some(&login(true, None))), "oauth: connected");
        assert_eq!(state(Some(&login(false, None))), "oauth: not connected");
        let long = format!("invalid_grant {}\nsecond line", "x".repeat(90));
        assert_eq!(
            state(Some(&login(true, Some(&long)))),
            format!("oauth: refresh failed (invalid_grant {})", "x".repeat(66))
        );
    }

    #[test]
    fn list_and_remove_go_to_the_broker() {
        let state = scratch_dir("credential-broker");
        let broker = FakeBroker::default();
        broker
            .keys
            .borrow_mut()
            .extend(["GITHUB_TOKEN".into(), "MINE".into()]);
        let calls = broker.calls.clone();
        let config =
            r#"{"tower": {"credentialFiles": {"GITHUB_TOKEN": "/t"}}}"#;
        let sandbox = Rc::new(FakeSandbox::default());
        let hangar =
            hangar_with_broker(config, &state, sandbox, Box::new(broker));
        remove(&hangar, "MINE").unwrap();
        assert_eq!(
            list(&hangar).unwrap().entries,
            [entry("GITHUB_TOKEN", "config", false, "set")]
        );
        assert_eq!(*calls.borrow(), ["delete MINE", "credentials"]);
    }

    fn oauth_hangar(name: &str, broker: FakeBroker) -> Hangar {
        let state = scratch_dir(&format!("credential-{name}"));
        let sandbox = Rc::new(FakeSandbox::default());
        hangar_with_broker("{}", &state, sandbox, Box::new(broker))
    }

    fn flags(scopes: &[&str]) -> LoginArgs {
        LoginArgs {
            source: ClientSource::Endpoints {
                authorization_url: "https://a.example.com/authorize".into(),
                token_url: "https://a.example.com/token".into(),
                client_id: "c1".into(),
            },
            scopes: scopes.iter().map(ToString::to_string).collect(),
            client_secret: false,
        }
    }

    fn stored_login(refreshed: &str, has_secret: bool) -> OAuthLogin {
        OAuthLogin {
            connected: true,
            refreshed_at: Some(refreshed.into()),
            error: Some("invalid_grant".into()),
            client: Some(OAuthClient {
                authorization_url: "https://a.example.com/authorize".into(),
                token_url: "https://a.example.com/token".into(),
                client_id: "stored".into(),
                scopes: "read".into(),
                token_auth_method: "client_secret_basic".into(),
            }),
            has_secret,
        }
    }

    fn holding(name: &str, login: Option<OAuthLogin>) -> FakeBroker {
        let broker = FakeBroker::default();
        broker.keys.borrow_mut().insert(name.into());
        if let Some(login) = login {
            broker.oauth.borrow_mut().insert(name.into(), login);
        }
        broker
    }

    #[test]
    fn a_login_connects_opens_the_consent_url_and_waits_for_tokens() {
        let broker = FakeBroker::default();
        let (calls, connected) =
            (broker.calls.clone(), broker.connected.clone());
        let hangar = oauth_hangar("login", broker);
        let login = prepare_login(
            &hangar,
            &FakeHttps::default(),
            "JIRA",
            flags(&["read", "write"]),
        )
        .unwrap();
        let consent = connect(&hangar, &login).unwrap();
        let opened = RefCell::new(Vec::new());
        let open = |url: &str| opened.borrow_mut().push(url.to_string());
        finish_login(&hangar, &login, &consent, &open).unwrap();

        assert_eq!(*opened.borrow(), [consent]);
        let (client, secret) = connected.borrow().clone().unwrap();
        assert_eq!(
            client,
            OAuthClient {
                authorization_url: "https://a.example.com/authorize".into(),
                token_url: "https://a.example.com/token".into(),
                client_id: "c1".into(),
                scopes: "read write".into(),
                token_auth_method: String::new(),
            }
        );
        assert_eq!(secret, "none");
        assert_eq!(
            *calls.borrow(),
            ["credentials", "oauth_connect JIRA", "credentials"]
        );
    }

    const ISSUER: &str = r#"{"issuer":"https://mcp.example.com","authorization_endpoint":"https://mcp.example.com/authorize","token_endpoint":"https://mcp.example.com/token","registration_endpoint":"https://mcp.example.com/register"}"#;
    const ISSUER_URL: &str =
        "https://mcp.example.com/.well-known/oauth-authorization-server";

    #[test]
    fn a_url_finds_the_provider_and_registers_a_client_with_the_callback() {
        let broker = FakeBroker::default();
        let connected = broker.connected.clone();
        let hangar = oauth_hangar("login-discover", broker);
        let https =
            FakeHttps::default().answer(ISSUER_URL, 200, ISSUER).answer(
                "https://mcp.example.com/register",
                201,
                r#"{"client_id":"dyn-1"}"#,
            );
        let discover = |client_id: Option<&str>| LoginArgs {
            source: ClientSource::Discover {
                url: "https://mcp.example.com/mcp".into(),
                client_id: client_id.map(Into::into),
            },
            scopes: vec![],
            client_secret: false,
        };
        let login =
            prepare_login(&hangar, &https, "MCP", discover(None)).unwrap();
        connect(&hangar, &login).unwrap();
        let (client, secret) = connected.borrow().clone().unwrap();
        assert_eq!(
            (client.authorization_url.as_str(), client.client_id.as_str()),
            ("https://mcp.example.com/authorize", "dyn-1")
        );
        assert_eq!(
            (client.token_url.as_str(), secret),
            ("https://mcp.example.com/token", "none")
        );
        let registered = https.requests.borrow().last().cloned().unwrap();
        assert!(registered.contains(FAKE_CALLBACK), "{registered}");

        let before = https.requests.borrow().len();
        let login =
            prepare_login(&hangar, &https, "MCP", discover(Some("mine")))
                .unwrap();
        connect(&hangar, &login).unwrap();
        let (client, _) = connected.borrow().clone().unwrap();
        assert_eq!(client.client_id, "mine");
        let posts = https.requests.borrow()[before..]
            .iter()
            .filter(|request| request.starts_with("POST"))
            .count();
        assert_eq!(posts, 0, "registered although given a client id");
    }

    #[test]
    fn the_flags_pick_where_the_client_comes_from() {
        let some = |value: &str| Some(value.to_string());
        let source = |url, authorize, token, id| {
            ClientSource::from_flags(url, authorize, token, id)
        };
        assert!(matches!(
            source(some("https://u.example.com"), None, None, some("c")),
            Ok(ClientSource::Discover {
                client_id: Some(_),
                ..
            })
        ));
        assert!(matches!(
            source(None, some("https://a"), some("https://t"), some("c")),
            Ok(ClientSource::Endpoints { .. })
        ));
        assert!(matches!(
            source(None, None, None, None),
            Ok(ClientSource::Stored)
        ));
        let error = source(None, None, None, some("c")).err().unwrap();
        assert_eq!(
            error.to_string(),
            "--client-id needs a URL, or --authorization-url and --token-url"
        );
    }

    #[test]
    fn a_re_login_keeps_the_stored_client_and_its_secret() {
        let broker = holding("JIRA", Some(stored_login("t", true)));
        let connected = broker.connected.clone();
        let hangar = oauth_hangar("relogin", broker);
        let args = LoginArgs {
            source: ClientSource::Stored,
            scopes: vec![],
            client_secret: false,
        };
        let login = prepare_login(&hangar, &FakeHttps::default(), "JIRA", args)
            .unwrap();
        let consent = connect(&hangar, &login).unwrap();
        // An old refresh error doesn't end the wait; new tokens do.
        finish_login(&hangar, &login, &consent, &|_| {}).unwrap();
        let (client, secret) = connected.borrow().clone().unwrap();
        assert_eq!(client.client_id, "stored");
        assert_eq!(client.scopes, "read");
        assert_eq!(client.token_auth_method, "client_secret_basic");
        assert_eq!(secret, "keep");

        let broker = holding("JIRA", Some(stored_login("t", false)));
        let connected = broker.connected.clone();
        let hangar = oauth_hangar("relogin-public", broker);
        let args = LoginArgs {
            source: ClientSource::Stored,
            scopes: vec!["admin".into()],
            client_secret: false,
        };
        let login = prepare_login(&hangar, &FakeHttps::default(), "JIRA", args)
            .unwrap();
        connect(&hangar, &login).unwrap();
        let (client, secret) = connected.borrow().clone().unwrap();
        assert_eq!((client.scopes.as_str(), secret), ("admin", "none"));
    }

    #[test]
    fn a_login_refuses_static_values_missing_clients_and_plain_http() {
        let refused = |broker: FakeBroker, args: LoginArgs| {
            let hangar = oauth_hangar("login-refused", broker);
            prepare_login(&hangar, &FakeHttps::default(), "JIRA", args)
                .err()
                .unwrap()
                .to_string()
        };
        assert_eq!(
            refused(holding("JIRA", None), flags(&[])),
            "JIRA holds a static value: run 'hangar credential rm JIRA' first"
        );
        let again = || LoginArgs {
            source: ClientSource::Stored,
            scopes: vec![],
            client_secret: false,
        };
        let expected = "JIRA has no OAuth client to log in with: pass \
                        --authorization-url, --token-url and --client-id";
        assert_eq!(refused(FakeBroker::default(), again()), expected);
        let mut no_authorize = stored_login("t", false);
        if let Some(client) = &mut no_authorize.client {
            client.authorization_url.clear();
        }
        assert_eq!(
            refused(holding("JIRA", Some(no_authorize)), again()),
            expected
        );

        let plain = LoginArgs {
            source: ClientSource::Endpoints {
                authorization_url: "https://a.example.com/authorize".into(),
                token_url: "http://a.example.com/token".into(),
                client_id: "c1".into(),
            },
            ..flags(&[])
        };
        assert_eq!(
            refused(FakeBroker::default(), plain),
            "http://a.example.com/token: expected an https:// URL"
        );
        let hangar = oauth_hangar("login-name", FakeBroker::default());
        assert!(
            prepare_login(&hangar, &FakeHttps::default(), "_JIRA", flags(&[]))
                .is_err()
        );
    }

    #[test]
    fn the_consent_url_must_be_https_and_come_back_to_the_vault() {
        let good = format!(
            "https://a.example.com/authorize?redirect_uri={}",
            FAKE_CALLBACK.replace('/', "%2F").replace(':', "%3A")
        );
        assert!(check_consent(&good, FAKE_CALLBACK).is_ok());
        let error =
            check_consent(&good.replace("https", "http"), FAKE_CALLBACK)
                .unwrap_err()
                .to_string();
        assert_eq!(
            error,
            "the vault's consent URL isn't https; not opening it"
        );
        let old_tower = "https://a.example.com/authorize?redirect_uri=\
                         http%3A%2F%2F0.0.0.0%3A14321%2Fv1%2Foauth%2Fcallback";
        let error = check_consent(old_tower, FAKE_CALLBACK).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "the vault sends logins back to \
                 http://0.0.0.0:14321/v1/oauth/callback, not {FAKE_CALLBACK}: \
                 restart the tower: hangar down && hangar up"
            )
        );
        let error = check_consent("https://a.example/", FAKE_CALLBACK);
        assert!(error.unwrap_err().to_string().contains("back to nowhere"));

        let broker = FakeBroker::default();
        *broker.consent.borrow_mut() = Some(old_tower.into());
        let hangar = oauth_hangar("login-old-tower", broker);
        let login =
            prepare_login(&hangar, &FakeHttps::default(), "JIRA", flags(&[]))
                .unwrap();
        let consent = connect(&hangar, &login).unwrap();
        let opened = RefCell::new(0);
        let open = |_: &str| *opened.borrow_mut() += 1;
        assert!(finish_login(&hangar, &login, &consent, &open).is_err());
        assert_eq!(*opened.borrow(), 0, "opened a URL it refused");
    }

    #[test]
    fn the_wait_ends_on_new_tokens_or_gives_up() {
        let broker = holding("JIRA", Some(stored_login("t", false)));
        let pause = Duration::ZERO;
        assert!(
            wait_for_tokens(&broker, "JIRA", Some("before"), 1, pause).is_ok()
        );
        let error = wait_for_tokens(&broker, "JIRA", Some("t"), 3, pause)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "no login for JIRA yet: the browser page shows the result; \
             check 'hangar credential list'"
        );
        assert!(wait_for_tokens(&broker, "OTHER", None, 2, pause).is_err());
        assert_eq!(
            broker
                .calls
                .borrow()
                .iter()
                .filter(|c| *c == "credentials")
                .count(),
            6
        );
    }

    #[test]
    fn set_refuses_an_oauth_credential() {
        let state = scratch_dir("credential-set-oauth");
        let broker = FakeBroker::default();
        broker.keys.borrow_mut().insert("JIRA".into());
        broker
            .oauth
            .borrow_mut()
            .insert("JIRA".into(), OAuthLogin::default());
        let sandbox = Rc::new(FakeSandbox::default());
        let hangar =
            hangar_with_broker("{}", &state, sandbox, Box::new(broker));
        let error = read(&hangar, "JIRA").unwrap_err().to_string();
        assert_eq!(
            error,
            "JIRA is an OAuth credential: run 'hangar credential login JIRA'"
        );
    }

    #[test]
    fn managed_and_malformed_names_are_refused() {
        let with_file = settings(
            r#"{"tower": {"credentialFiles": {"GITHUB_TOKEN": "/t"}},
                "bays": [{"name": "default", "apps": ["github-token"]}]}"#,
        );
        let error = check_user_key(&with_file, "GITHUB_TOKEN").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("managed by tower.credentialFiles")
        );
        let error = check_user_key(&with_file, "GITHUB_GIT_USER").unwrap_err();
        assert!(
            error.to_string().contains("set by app github-token"),
            "{error}"
        );
        // A token stored by hand: github-token still gets its git username.
        let by_hand = settings(
            r#"{"bays": [{"name": "default", "apps": ["github-token"]}]}"#,
        );
        assert!(check_user_key(&by_hand, "GITHUB_TOKEN").is_ok());
        assert!(check_user_key(&by_hand, "GITHUB_GIT_USER").is_err());
        assert!(check_user_key(&by_hand, "lower").is_err());
        assert!(check_user_key(&by_hand, "MINE").is_ok());
    }

    #[test]
    fn the_hidden_prompt_returns_the_line_over_a_pipe() {
        let file = scratch_dir("hidden").join("input");
        fs::write(&file, "typed-secret\nignored\n").unwrap();
        // Not a terminal, so stty fails harmlessly; the line still arrives.
        let input = Stdio::from(File::open(&file).unwrap());
        assert_eq!(read_hidden("TOKEN", input).unwrap(), "typed-secret");
    }
}
