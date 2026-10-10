//! agent-vault as hangar's broker, run in the tower: its VM, its CLI (run
//! inside that VM) and its admin API on the host's 127.0.0.1, plus the
//! state files it keeps in `stateDir`. Each bay is one agent-vault agent,
//! `hangar-<bay>`, with its own token.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use super::{
    Access, Broker, BrokerHealth, Credential, CredentialKind, OAuthClient,
    OAuthState, PROXY, PendingLogin, Policy, Route, UiLogin, login_timed_out,
};
use crate::config::{Fields, valid_bay_name};
use crate::error::{Context, Error, Result, bail};
use crate::http;
use crate::json::{self, Json};
use crate::sandbox::{
    BoxState, Egress, Mount, MountMode, PublishedPort, Sandbox, TOWER_VM,
    VmSpec,
};
use crate::secret::{Secret, random_hex, trim_line_end};
use crate::state::write_private;
use log::info;
use miniserde::json::{Array, Object};

/// The admin API and the proxy inside the VM.
const ADMIN_PORT: u16 = 14321;
const PROXY_PORT: u16 = 14322;
const OWNER_EMAIL: &str = "owner@hangar.local";
/// All of hangar's credentials and settings live in this vault.
const VAULT: &str = "default";

// Trap: starting a sandbox doesn't rerun an image's entrypoint, so the
// server is started on every boot, in the background, which needs a shell
// inside the VM. A stale PID file from an unclean stop would block it. The
// password arrives on stdin and is piped on, never written or put on a
// command line. AGENT_VAULT_ADDR is the address the host's browser reaches
// it on; without it, OAuth callbacks would point at 0.0.0.0.
const START_SERVER: &str = r#"IFS= read -r password
[ -n "$password" ] || exit 1
rm -f /data/.agent-vault/agent-vault.pid
printf "%s\n" "$password" | AGENT_VAULT_ADDR="$2" setsid agent-vault server \
  --host 0.0.0.0 --port "$1" --password-stdin >/data/server.log 2>&1 &"#;

pub(crate) struct AgentVault {
    sandbox: Rc<dyn Sandbox>,
    image: String,
    admin_port: u16,
    proxy_port: u16,
    state: PathBuf,
}

impl AgentVault {
    pub(crate) fn new(
        tower: &Json,
        sandbox: Rc<dyn Sandbox>,
        state: &Path,
    ) -> Result<Self> {
        let settings =
            Fields::new(tower, "config.tower")?.object("agentVault")?;
        settings.only(&["image", "adminPort", "proxyPort"])?;
        Ok(Self {
            sandbox,
            image: settings.string("image")?,
            admin_port: settings.number("adminPort")?,
            proxy_port: settings.number("proxyPort")?,
            state: state.to_path_buf(),
        })
    }

    fn data_dir(&self) -> PathBuf {
        self.state.join("vault")
    }

    /// agent-vault's own data; its presence means the vault already has a
    /// master password.
    fn vault_data(&self) -> PathBuf {
        self.data_dir().join(".agent-vault")
    }

    fn server_log(&self) -> PathBuf {
        self.data_dir().join("server.log")
    }

    fn session(&self) -> PathBuf {
        self.vault_data().join("session.json")
    }

    fn owner_password(&self) -> PathBuf {
        self.state.join("owner-password")
    }

    /// One token file per bay hangar minted an agent for; a file is the
    /// only proof an agent is hangar's.
    fn agent_tokens(&self) -> PathBuf {
        self.state.join("agent-tokens")
    }

    /// A bay's proxy login. It reaches the bay only inside the proxy URL,
    /// on stdin into its proxy env.
    fn agent_token(&self, bay: &str) -> PathBuf {
        self.agent_tokens().join(bay)
    }

    /// The vault UI as the host's browser reaches it.
    fn host_address(&self) -> String {
        format!("http://127.0.0.1:{}", self.admin_port)
    }

    fn start_server_args(&self) -> [String; 6] {
        [
            "sh".into(),
            "-c".into(),
            START_SERVER.into(),
            "hangar-vault".into(),
            ADMIN_PORT.to_string(),
            self.host_address(),
        ]
    }

    fn healthy(&self) -> bool {
        http::get(self.admin_port, "/health")
            .is_ok_and(|response| response.status == 200)
    }

    fn wait_healthy(&self, attempts: u32, pause: Duration) -> bool {
        for _ in 0..attempts {
            if self.healthy() {
                return true;
            }
            thread::sleep(pause);
        }
        false
    }

    fn create_vm(&self) -> Result<()> {
        let data = self.data_dir();
        create_dir(&data)?;
        let publish: Vec<(u16, u16)> = self
            .ports()
            .iter()
            .map(|port| (port.host, port.vm))
            .collect();
        self.sandbox.create(&VmSpec {
            name: TOWER_VM,
            image: &self.image,
            cpus: 1,
            memory: "512M",
            disk: None,
            native_fs: &[],
            init: None,
            egress: Egress::Open,
            publish: &publish,
            mounts: &[Mount {
                host: &data,
                guest: "/data",
                mode: MountMode::ReadWrite,
                owner: Some((65532, 65532)),
            }],
            env: &[
                "HOME=/data",
                "AGENT_VAULT_TELEMETRY=false",
                "AGENT_VAULT_RATELIMIT_PROFILE=loose",
            ],
        })
    }

    /// Keeps the session where agent-vault's CLI keeps it.
    fn ensure_owner(&self) -> Result<()> {
        let file = self.owner_password();
        let action = if file.exists() {
            "login"
        } else {
            write_private(&file, random_hex(24)?.as_bytes())?;
            "register"
        };
        let password = fs::read_to_string(&file).context(file.display())?;
        let body = json::stringify(&json::object([
            ("email", json::string(OWNER_EMAIL)),
            ("password", json::string(trim_line_end(&password))),
            ("device_label", json::string("hangar")),
        ]));
        let path = format!("/v1/auth/{action}");
        let reply =
            http::call(self.admin_port, "POST", &path, None, Some(&body));
        if reply.is_err() && action == "register" {
            // Otherwise every later run would try to log in with it.
            fs::remove_file(&file).context(file.display())?;
        }
        write_private(&self.session(), reply?.as_bytes())
    }

    /// The bay's token, and whether it was just renewed. Without a token
    /// file, an agent of that name is deleted first: hangar never adopts
    /// an agent whose token it doesn't hold.
    fn ensure_agent_token(&self, bay: &str) -> Result<(Secret, bool)> {
        let file = self.agent_token(bay);
        if has_content(&file) {
            let token = fs::read_to_string(&file).context(file.display())?;
            return Ok((Secret::new(token.trim().to_string()), false));
        }
        let admin = self.admin()?;
        let agent = vault_agent(bay);
        let renewed = admin.delete_agent(&agent)?;
        let grant = json::object([
            ("vault_name", json::string(VAULT)),
            ("vault_role", json::string("proxy")),
        ]);
        let body = json::stringify(&json::object([
            ("name", json::string(&agent)),
            ("role", json::string("no-access")),
            ("vaults", Json::Array(Array::from_iter([grant]))),
        ]));
        let reply = admin.call("POST", "/v1/agents", Some(&body))?;
        let created: NewAgent = json::from_str(&reply)?;
        let token = Secret::new(created.av_agent_token);
        if token.expose().is_empty() {
            bail!("could not create the agent token");
        }
        write_private(&file, token.expose().as_bytes())?;
        Ok((token, renewed))
    }

    fn admin(&self) -> Result<Admin> {
        let file = self.session();
        let text = fs::read_to_string(&file).context(file.display())?;
        let session: Session =
            json::from_str(&text).context("no token in the vault session")?;
        Ok(Admin {
            port: self.admin_port,
            token: Secret::new(session.token),
        })
    }

    fn policy(&self) -> Option<Policy> {
        let policy = self
            .admin()
            .and_then(|admin| admin.policy())
            .inspect_err(|error| log::debug!("unlisted host policy: {error}"))
            .ok()
            .flatten();
        match policy.as_deref() {
            Some("deny") => Some(Policy::Deny),
            Some("allow") => Some(Policy::Allow),
            _ => None,
        }
    }
}

impl Broker for AgentVault {
    fn has_data(&self) -> Option<PathBuf> {
        Some(self.vault_data()).filter(|data| data.exists())
    }

    /// `extra` is flattened into the service agent-vault reads, so it may
    /// not set what hangar sets itself.
    fn validate(&self, route: &Route) -> Result<()> {
        for key in ["name", "host", "auth"] {
            if route.extra.contains_key(key) {
                bail!(
                    "config.tower.routes.{}.extra.{key}: set by hangar",
                    route.name
                );
            }
        }
        Ok(())
    }

    fn ensure_running(&self, password: &Secret) -> Result<bool> {
        let state = self.sandbox.state(TOWER_VM)?;
        match state {
            BoxState::Running => {}
            BoxState::Missing => {
                info!("creating {TOWER_VM}");
                self.create_vm()?;
            }
            BoxState::Stopped => {
                info!("starting {TOWER_VM}");
                self.sandbox.start(TOWER_VM)?;
            }
        }
        if !self.healthy() {
            let stdin = format!("{}\n", password.expose()).into_bytes();
            let start = self.start_server_args();
            let start: Vec<&str> = start.iter().map(String::as_str).collect();
            self.sandbox.exec(TOWER_VM, None, &start, Some(&stdin))?;
        }
        if !self.wait_healthy(30, Duration::from_secs(1)) {
            bail!(
                "agent-vault did not become healthy, see {}",
                self.server_log().display()
            );
        }
        info!("applying vault config");
        self.ensure_owner()?;
        Ok(state == BoxState::Missing)
    }

    fn deny_unlisted(&self) -> Result<()> {
        self.admin()?.deny_unlisted()
    }

    /// A PUT replaces the whole set.
    fn set_routes(&self, routes: &[Route]) -> Result<()> {
        let path = format!("/v1/vaults/{VAULT}/services");
        let services = render_services(routes);
        self.admin()?.call("PUT", &path, Some(&services)).map(drop)
    }

    fn credentials(&self) -> Result<Vec<Credential>> {
        let listed = self.admin()?.list()?;
        Ok(listed
            .into_iter()
            .map(|(key, entry)| Credential {
                key,
                kind: entry.map_or(CredentialKind::Static, Entry::kind),
            })
            .collect())
    }

    fn put_credential(&self, key: &str, value: &Secret) -> Result<()> {
        self.admin()?.post(key, value)
    }

    fn delete_credentials(&self, keys: &[String]) -> Result<()> {
        self.admin()?.delete(keys)
    }

    /// Built by agent-vault from `AGENT_VAULT_ADDR`, the host address.
    fn oauth_redirect_uri(&self) -> Result<String> {
        Ok(format!("{}/v1/oauth/callback", self.host_address()))
    }

    /// The login is done once `last_refreshed_at` moves off the marker,
    /// whatever the tower's clock says.
    fn oauth_begin(
        &self,
        key: &str,
        client: Option<&OAuthClient>,
    ) -> Result<PendingLogin> {
        let admin = self.admin()?;
        let stored = admin.oauth_entry(key)?;
        let marker = stored
            .as_ref()
            .and_then(|entry| entry.last_refreshed_at.clone())
            .unwrap_or_default();
        let url = match client {
            Some(client) => admin.connect(key, client)?,
            None => admin.connect(key, &stored_client(key, stored)?)?,
        };
        Ok(PendingLogin {
            key: key.to_string(),
            url,
            marker,
        })
    }

    fn oauth_wait(
        &self,
        login: &PendingLogin,
        timeout: Duration,
    ) -> Result<()> {
        self.admin()?.wait_for_tokens(login, timeout, POLL)
    }

    fn access(&self, bay: &str) -> Result<Access> {
        let (token, renewed) = self.ensure_agent_token(bay)?;
        let ca = "/v1/mitm/ca.pem";
        let ca_pem = http::call(self.admin_port, "GET", ca, None, None)?;
        let proxy_url = format!(
            "http://{}:{VAULT}@{}:{}",
            token.expose(),
            self.sandbox.host_address(),
            self.proxy_port
        );
        Ok(Access {
            proxy_url: Secret::new(proxy_url),
            ca_pem: ca_pem.into_bytes(),
            host_port: self.proxy_port,
            renewed,
        })
    }

    /// Token files whose names fail the bay rule aren't hangar's to judge,
    /// and are left alone.
    fn retain_bays(&self, bays: &[String]) -> Result<()> {
        let Ok(entries) = fs::read_dir(self.agent_tokens()) else {
            return Ok(());
        };
        let mut stale: Vec<String> = entries
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| valid_bay_name(name) && !bays.contains(name))
            .collect();
        if stale.is_empty() {
            return Ok(());
        }
        stale.sort();
        let admin = self.admin()?;
        for bay in stale {
            info!("deleting the vault agent of bay {bay}");
            admin.delete_agent(&vault_agent(&bay))?;
            let file = self.agent_token(&bay);
            fs::remove_file(&file).context(file.display())?;
        }
        Ok(())
    }

    fn health(&self) -> BrokerHealth {
        let healthy = self.healthy();
        BrokerHealth {
            reachable: healthy || http::get(self.admin_port, "/").is_ok(),
            healthy,
            unlisted: if healthy { self.policy() } else { None },
        }
    }

    fn ports(&self) -> Vec<PublishedPort> {
        vec![
            PublishedPort {
                app: None,
                bay: None,
                name: "vault-ui".into(),
                host: self.admin_port,
                vm: ADMIN_PORT,
                purpose: "agent-vault admin UI and API".into(),
                http: true,
            },
            PublishedPort {
                app: None,
                bay: None,
                name: PROXY.into(),
                host: self.proxy_port,
                vm: PROXY_PORT,
                purpose: "the bays' only way out".into(),
                http: false,
            },
        ]
    }

    fn ui(&self) -> Result<Option<UiLogin>> {
        let file = self.owner_password();
        let Ok(password) = fs::read_to_string(&file) else {
            return Ok(None);
        };
        Ok(Some(UiLogin {
            url: self.host_address(),
            login: OWNER_EMAIL.into(),
            password: Secret::new(password),
            password_file: file,
        }))
    }
}

/// The services the vault takes: one per route, with `extra` flattened
/// in.
fn render_services(routes: &[Route]) -> String {
    let services = routes
        .iter()
        .map(|route| {
            let mut object = route.extra.clone();
            object.insert("name".into(), json::string(&route.name));
            object.insert("host".into(), json::string(&route.host));
            object.insert("auth".into(), route.auth.to_json());
            Json::Object(object)
        })
        .collect::<Array>();
    json::stringify(&json::object([("services", Json::Array(services))]))
}

fn create_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).context(dir.display())
}

fn has_content(file: &Path) -> bool {
    fs::metadata(file).is_ok_and(|meta| meta.len() > 0)
}

/// A logged-in admin client; the session is read once per call site.
struct Admin {
    port: u16,
    token: Secret,
}

impl Admin {
    fn deny_unlisted(&self) -> Result<()> {
        let body = json::stringify(&VaultSettings {
            unmatched_host_policy: Some("deny".into()),
        });
        self.call("PATCH", &settings_path(), Some(&body)).map(drop)
    }

    fn policy(&self) -> Result<Option<String>> {
        let body = self.call("GET", &settings_path(), None)?;
        let settings: VaultSettings = json::from_str(&body)?;
        Ok(settings.unmatched_host_policy)
    }

    /// Names in the vault's order, with what it lists about each.
    fn list(&self) -> Result<Vec<(String, Option<Entry>)>> {
        let path = format!("/v1/credentials?vault={VAULT}");
        let list: ListResponse =
            json::from_str(&self.call("GET", &path, None)?)?;
        let mut entries: BTreeMap<String, Entry> = list
            .credentials
            .unwrap_or_default()
            .into_iter()
            .map(|entry| (entry.key.clone(), entry))
            .collect();
        Ok(list
            .keys
            .unwrap_or_default()
            .into_iter()
            .map(|key| {
                let entry = entries.remove(&key);
                (key, entry)
            })
            .collect())
    }

    fn oauth_entry(&self, key: &str) -> Result<Option<Entry>> {
        Ok(self
            .list()?
            .into_iter()
            .find(|(listed, _)| listed == key)
            .and_then(|(_, entry)| entry)
            .filter(Entry::is_oauth))
    }

    /// A refresh error left by an earlier login doesn't end the wait.
    fn wait_for_tokens(
        &self,
        login: &PendingLogin,
        timeout: Duration,
        pause: Duration,
    ) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let refreshed = self
                .oauth_entry(&login.key)?
                .and_then(|entry| entry.last_refreshed_at);
            if refreshed.is_some_and(|at| at != login.marker) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(login_timed_out(&login.key));
            }
            thread::sleep(pause);
        }
    }

    fn post(&self, key: &str, value: &Secret) -> Result<()> {
        let body = json::stringify(&PostBody {
            vault: VAULT.into(),
            credentials: BTreeMap::from([(
                key.to_string(),
                value.expose().to_string(),
            )]),
        });
        self.call("POST", "/v1/credentials", Some(&body))
            .map(drop)
            .context(format!("credential {key}"))
    }

    /// The client secret travels only in the body.
    fn connect(&self, key: &str, client: &OAuthClient) -> Result<String> {
        let mut body = Object::new();
        let mut put = |name: &str, value: &str| {
            if !value.is_empty() {
                body.insert(name.into(), json::string(value));
            }
        };
        put("vault", VAULT);
        put("key", key);
        put("authorization_url", &client.authorization_url);
        put("token_url", &client.token_url);
        put("client_id", &client.client_id);
        put("scopes", &client.scopes);
        put("token_auth_method", &client.token_auth_method);
        if let Some(secret) = &client.client_secret {
            put("client_secret", secret.expose());
        }
        let body = json::stringify(&Json::Object(body));
        let path = "/v1/credentials/oauth/connect";
        let reply = self
            .call("POST", path, Some(&body))
            .context(format!("OAuth login {key}"))?;
        let consent: ConnectResponse = json::from_str(&reply)?;
        Ok(consent.authorization_url)
    }

    fn delete(&self, keys: &[String]) -> Result<()> {
        let body = json::stringify(&DeleteBody {
            vault: VAULT.into(),
            keys: keys.to_vec(),
        });
        self.call("DELETE", "/v1/credentials", Some(&body))
            .map(drop)
            .context(format!("credentials {}", keys.join(", ")))
    }

    /// True when there was one to delete; 404 means it's already gone.
    fn delete_agent(&self, agent: &str) -> Result<bool> {
        let path = format!("/v1/agents/{agent}/delete");
        let token = Some(&self.token);
        let timeout = Duration::from_secs(30);
        let response =
            http::send(self.port, "POST", &path, token, Some("{}"), timeout)?;
        match response.status {
            200..300 => Ok(true),
            404 => Ok(false),
            status => bail!("POST {path}: HTTP {status}"),
        }
    }

    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<String> {
        http::call(self.port, method, path, Some(&self.token), body)
    }
}

/// agent-vault's `oauthSecretSentinel`: keep the stored client secret.
/// It keeps it only while `token_url` stays.
const KEEP_SECRET: &str = "••••••••";

/// How often a login's wait asks the vault.
const POLL: Duration = Duration::from_secs(1);

/// The client the vault holds for `key`, its secret kept.
fn stored_client(key: &str, stored: Option<Entry>) -> Result<OAuthClient> {
    let client = stored.and_then(|entry| {
        Some(OAuthClient {
            authorization_url: entry
                .authorization_url
                .filter(|url| !url.is_empty())?,
            token_url: entry.token_url?,
            client_id: entry.client_id?,
            client_secret: entry
                .client_secret
                .map(|_| Secret::new(KEEP_SECRET.into())),
            scopes: entry.scopes.unwrap_or_default(),
            token_auth_method: entry.token_auth_method.unwrap_or_default(),
        })
    });
    client.ok_or_else(|| {
        Error::with_hint(
            format!("{key} has no OAuth client to log in with"),
            "pass --authorization-url, --token-url and --client-id",
        )
    })
}

fn vault_agent(bay: &str) -> String {
    format!("hangar-{bay}")
}

fn settings_path() -> String {
    format!("/v1/vaults/{VAULT}/settings")
}

#[derive(miniserde::Serialize, miniserde::Deserialize)]
struct VaultSettings {
    unmatched_host_policy: Option<String>,
}

#[derive(miniserde::Serialize)]
struct PostBody {
    vault: String,
    credentials: BTreeMap<String, String>,
}

#[derive(miniserde::Serialize)]
struct DeleteBody {
    vault: String,
    keys: Vec<String>,
}

#[derive(miniserde::Deserialize)]
struct ListResponse {
    keys: Option<Vec<String>>,
    credentials: Option<Vec<Entry>>,
}

/// What the vault lists about a credential; it masks every secret.
#[derive(miniserde::Deserialize)]
struct Entry {
    key: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    connected_at: Option<String>,
    last_refreshed_at: Option<String>,
    last_refresh_error: Option<String>,
    authorization_url: Option<String>,
    token_url: Option<String>,
    client_id: Option<String>,
    scopes: Option<String>,
    token_auth_method: Option<String>,
    /// Only ever the mask.
    client_secret: Option<String>,
}

impl Entry {
    /// Its other kinds (dynamic secrets) count as static.
    fn is_oauth(&self) -> bool {
        self.kind.as_deref() == Some("oauth")
    }

    fn kind(self) -> CredentialKind {
        if !self.is_oauth() {
            return CredentialKind::Static;
        }
        CredentialKind::OAuth(
            match (self.last_refresh_error, self.connected_at) {
                (Some(error), _) => OAuthState::Failed(error),
                (None, Some(_)) => OAuthState::Connected,
                (None, None) => OAuthState::NotConnected,
            },
        )
    }
}

#[derive(miniserde::Deserialize)]
struct ConnectResponse {
    authorization_url: String,
}

#[derive(miniserde::Deserialize)]
struct Session {
    token: String,
}

#[derive(miniserde::Deserialize)]
struct NewAgent {
    av_agent_token: String,
}

#[cfg(test)]
mod tests {
    use super::{Admin, AgentVault, render_services};
    use crate::broker::{
        Auth, Broker, Credential, CredentialKind, OAuthClient, OAuthState,
        PendingLogin, Route,
    };
    use crate::json;
    use crate::sandbox::fake::FakeSandbox;
    use crate::sandbox::{Egress, TOWER_VM};
    use crate::secret::Secret;
    use crate::testing::{closed_port, scratch_dir, serve_each, settings};
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::rc::Rc;
    use std::time::Duration;

    fn agent_vault(
        config: &str,
        state: &Path,
        sandbox: Rc<FakeSandbox>,
    ) -> AgentVault {
        AgentVault::new(&settings(config).tower, sandbox, state).unwrap()
    }

    /// A vault whose admin API is at `port`, logged in.
    fn logged_in(
        state: &Path,
        port: u16,
        sandbox: Rc<FakeSandbox>,
    ) -> AgentVault {
        let session = state.join("vault/.agent-vault/session.json");
        std::fs::create_dir_all(session.parent().unwrap()).unwrap();
        std::fs::write(&session, r#"{"token": "session-token"}"#).unwrap();
        let config = format!(
            r#"{{"tower": {{"agentVault": {{"adminPort": {port}}}}}}}"#
        );
        agent_vault(&config, state, sandbox)
    }

    fn first_line(request: &str) -> &str {
        request.lines().next().unwrap()
    }

    fn admin(port: u16) -> Admin {
        Admin {
            port,
            token: Secret::new("session-token".into()),
        }
    }

    #[test]
    fn its_settings_are_read_from_tower_agent_vault() {
        let state = scratch_dir("agent-vault-settings");
        let sandbox = Rc::new(FakeSandbox::default());
        let config = r#"{"tower": {"agentVault": {"proxyPort": 15000}}}"#;
        let vault = agent_vault(config, &state, sandbox);
        let ports: Vec<(String, u16, u16)> = vault
            .ports()
            .into_iter()
            .map(|port| (port.name, port.host, port.vm))
            .collect();
        assert_eq!(
            ports,
            [
                ("vault-ui".into(), 14321, 14321),
                ("proxy".into(), 15000, 14322)
            ]
        );
        let settings = settings(r#"{"tower": {"agentVault": {"port": 1}}}"#);
        let error = AgentVault::new(
            &settings.tower,
            Rc::new(FakeSandbox::default()),
            &state,
        )
        .err()
        .unwrap()
        .to_string();
        assert_eq!(error, "config.tower.agentVault.port: unknown setting");
    }

    #[test]
    fn a_vault_that_never_answers_is_not_healthy() {
        let state = scratch_dir("agent-vault-unhealthy");
        let config = format!(
            r#"{{"tower": {{"agentVault": {{"adminPort": {}}}}}}}"#,
            closed_port()
        );
        let vault =
            agent_vault(&config, &state, Rc::new(FakeSandbox::default()));
        assert!(!vault.wait_healthy(2, Duration::from_millis(1)));
        let health = vault.health();
        assert!(!health.reachable && !health.healthy);
        assert_eq!(health.unlisted, None);
    }

    #[test]
    fn a_vault_answering_200_is_healthy() {
        let state = scratch_dir("agent-vault-healthy");
        let (port, _) = serve_each(vec![("200 OK", "")]);
        let config = format!(
            r#"{{"tower": {{"agentVault": {{"adminPort": {port}}}}}}}"#
        );
        let vault =
            agent_vault(&config, &state, Rc::new(FakeSandbox::default()));
        assert!(vault.wait_healthy(3, Duration::from_millis(10)));
    }

    #[test]
    fn the_server_learns_the_address_the_host_reaches_it_on() {
        let state = scratch_dir("agent-vault-addr");
        let config = r#"{"tower": {"agentVault": {"adminPort": 14421}}}"#;
        let vault =
            agent_vault(config, &state, Rc::new(FakeSandbox::default()));
        let args = vault.start_server_args();
        assert_eq!(
            args[3..],
            ["hangar-vault", "14321", "http://127.0.0.1:14421"]
        );
        assert!(args[2].contains(r#"AGENT_VAULT_ADDR="$2""#));
    }

    #[test]
    fn a_new_vm_publishes_both_ports_and_mounts_its_data() {
        let state = scratch_dir("agent-vault-create");
        let sandbox = Rc::new(FakeSandbox::default());
        agent_vault("{}", &state, sandbox.clone())
            .create_vm()
            .unwrap();
        let created = sandbox.created.borrow();
        assert_eq!(created[0].name, TOWER_VM);
        assert_eq!(created[0].init, None);
        assert_eq!(created[0].egress, Egress::Open);
        assert_eq!(created[0].publish, [(14321, 14321), (14322, 14322)]);
        assert_eq!(
            created[0].mounts,
            [format!(
                "{}:/data:ReadWrite:65532:65532",
                state.join("vault").display()
            )]
        );
    }

    #[test]
    fn access_mints_a_token_per_bay_once_and_fetches_the_ca() {
        let state = scratch_dir("agent-vault-access");
        let sandbox = Rc::new(FakeSandbox::default());
        let (port, server) = serve_each(vec![
            ("404 Not Found", "{}"),
            ("200 OK", r#"{"av_agent_token":"work-token"}"#),
            ("200 OK", "CA\n"),
            ("200 OK", "CA\n"),
        ]);
        let vault = logged_in(&state, port, sandbox.clone());
        let access = vault.access("work").unwrap();
        assert!(!access.renewed);
        assert_eq!(access.ca_pem, b"CA\n");
        assert_eq!(access.host_port, 14322);
        assert_eq!(
            access.proxy_url.expose(),
            "http://work-token:default@host.fake:14322"
        );
        vault.access("work").unwrap();
        let requests = server.join().unwrap();
        let lines: Vec<&str> = requests.iter().map(|r| first_line(r)).collect();
        assert_eq!(
            lines,
            [
                "POST /v1/agents/hangar-work/delete HTTP/1.1",
                "POST /v1/agents HTTP/1.1",
                "GET /v1/mitm/ca.pem HTTP/1.1",
                "GET /v1/mitm/ca.pem HTTP/1.1",
            ]
        );
        assert!(requests[1].contains("authorization: Bearer session-token"));
        assert!(requests[1].ends_with(
            r#"{"name":"hangar-work","role":"no-access","vaults":[{"vault_name":"default","vault_role":"proxy"}]}"#
        ));
        assert!(!requests[2].contains("authorization"));
        let token = state.join("agent-tokens/work");
        assert_eq!(std::fs::read_to_string(&token).unwrap(), "work-token");
        let mode = std::fs::metadata(&token).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(sandbox.changes(), Vec::<String>::new());
    }

    #[test]
    fn a_lost_token_file_replaces_the_bays_agent_and_says_so() {
        let state = scratch_dir("agent-vault-renew");
        let (port, server) = serve_each(vec![
            ("200 OK", "{}"),
            ("200 OK", r#"{"av_agent_token":"new-token"}"#),
            ("200 OK", "CA"),
        ]);
        let vault = logged_in(&state, port, Rc::new(FakeSandbox::default()));
        assert!(vault.access("work").unwrap().renewed);
        assert_eq!(
            first_line(&server.join().unwrap()[0]),
            "POST /v1/agents/hangar-work/delete HTTP/1.1"
        );

        let (port, _server) = serve_each(vec![
            ("404 Not Found", "{}"),
            ("200 OK", r#"{"av_agent_token":""}"#),
        ]);
        let empty = scratch_dir("agent-vault-no-token");
        let tokenless =
            logged_in(&empty, port, Rc::new(FakeSandbox::default()));
        let error = tokenless.access("work").err().unwrap().to_string();
        assert_eq!(error, "could not create the agent token");

        let (port, _server) =
            serve_each(vec![("500 Internal Server Error", "")]);
        let failing = scratch_dir("agent-vault-delete-fails");
        let vault = logged_in(&failing, port, Rc::new(FakeSandbox::default()));
        let error = vault.access("work").err().unwrap().to_string();
        assert_eq!(error, "POST /v1/agents/hangar-work/delete: HTTP 500");
    }

    #[test]
    fn only_unlisted_bays_with_a_token_file_lose_their_agent() {
        let state = scratch_dir("agent-vault-retain");
        let tokens = state.join("agent-tokens");
        std::fs::create_dir_all(&tokens).unwrap();
        for name in ["work", "old", "gone", "Not-A-Bay"] {
            std::fs::write(tokens.join(name), "token").unwrap();
        }
        let (port, server) =
            serve_each(vec![("404 Not Found", "{}"), ("200 OK", "{}")]);
        let vault = logged_in(&state, port, Rc::new(FakeSandbox::default()));
        vault.retain_bays(&["work".to_string()]).unwrap();
        let requests = server.join().unwrap();
        let paths: Vec<&str> = requests.iter().map(|r| first_line(r)).collect();
        assert_eq!(
            paths,
            [
                "POST /v1/agents/hangar-gone/delete HTTP/1.1",
                "POST /v1/agents/hangar-old/delete HTTP/1.1"
            ]
        );
        let mut left: Vec<String> = std::fs::read_dir(&tokens)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["Not-A-Bay", "work"]);
        // Nothing stale: no admin call at all, not even a login.
        let offline =
            agent_vault("{}", &state, Rc::new(FakeSandbox::default()));
        offline.retain_bays(&["work".to_string()]).unwrap();
        let none = scratch_dir("agent-vault-retain-none");
        let fresh = agent_vault("{}", &none, Rc::new(FakeSandbox::default()));
        fresh.retain_bays(&[]).unwrap();
    }

    #[test]
    fn data_and_login_exist_only_once_written() {
        let state = scratch_dir("agent-vault-data");
        let vault = agent_vault("{}", &state, Rc::new(FakeSandbox::default()));
        assert_eq!(vault.has_data(), None);
        assert!(vault.ui().unwrap().is_none());
        std::fs::create_dir_all(state.join("vault/.agent-vault")).unwrap();
        std::fs::write(state.join("owner-password"), "owner-1").unwrap();
        assert_eq!(vault.has_data(), Some(state.join("vault/.agent-vault")));
        let login = vault.ui().unwrap().unwrap();
        assert_eq!(login.url, "http://127.0.0.1:14321");
        assert_eq!(login.login, "owner@hangar.local");
        assert_eq!(login.password.expose(), "owner-1");
        assert_eq!(login.password_file, state.join("owner-password"));
    }

    fn service(extra: &str) -> Route {
        let Ok(json::Json::Object(extra)) = json::parse(extra) else {
            panic!("not an object");
        };
        Route {
            name: "twilio".into(),
            host: "api.twilio.com".into(),
            auth: Auth::Basic {
                username: "SID".into(),
                password: None,
            },
            extra,
        }
    }

    #[test]
    fn extra_is_flattened_into_the_service_and_may_not_shadow_it() {
        assert_eq!(
            render_services(&[service(r#"{"substitutions": []}"#)]),
            r#"{"services":[{"auth":{"type":"basic","username":"SID"},"host":"api.twilio.com","name":"twilio","substitutions":[]}]}"#
        );
        let state = scratch_dir("agent-vault-validate");
        let vault = agent_vault("{}", &state, Rc::new(FakeSandbox::default()));
        assert!(vault.validate(&service("{}")).is_ok());
        for key in ["name", "host", "auth"] {
            let extra = format!(r#"{{"{key}": "x"}}"#);
            let error = vault.validate(&service(&extra)).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!(
                    "config.tower.routes.twilio.extra.{key}: set by hangar"
                )
            );
        }
    }

    #[test]
    fn credentials_are_listed_with_the_session_token_and_their_state() {
        let state = scratch_dir("agent-vault-credentials");
        let (port, server) =
            serve_each(vec![("200 OK", r#"{"keys":["A","B"]}"#)]);
        let vault = logged_in(&state, port, Rc::new(FakeSandbox::default()));
        let listed = |key: &str, kind| Credential {
            key: key.into(),
            kind,
        };
        assert_eq!(
            vault.credentials().unwrap(),
            [
                listed("A", CredentialKind::Static),
                listed("B", CredentialKind::Static)
            ]
        );
        let request = server.join().unwrap().remove(0);
        assert!(request.starts_with("GET /v1/credentials?vault=default "));
        assert!(request.contains("authorization: Bearer session-token"));

        let entries = r#"{"keys":["OLD","NEW","ON","DYN","S"],"credentials":[
            {"key":"S","type":"static"},
            {"key":"DYN","type":"dynamic","value":"leased"},
            {"key":"ON","type":"oauth","connected_at":"t1"},
            {"key":"NEW","type":"oauth","client_secret":"••••••••"},
            {"key":"OLD","type":"oauth","connected_at":"t1",
             "last_refreshed_at":"t2","last_refresh_error":"invalid_grant",
             "access_token":"••••••••","refresh_token":"••••••••"}]}"#;
        let (port, _) = serve_each(vec![("200 OK", entries)]);
        let vault = logged_in(&state, port, Rc::new(FakeSandbox::default()));
        let oauth =
            |key: &str, state| listed(key, CredentialKind::OAuth(state));
        assert_eq!(
            vault.credentials().unwrap(),
            [
                oauth("OLD", OAuthState::Failed("invalid_grant".into())),
                oauth("NEW", OAuthState::NotConnected),
                oauth("ON", OAuthState::Connected),
                listed("DYN", CredentialKind::Static),
                listed("S", CredentialKind::Static),
            ]
        );

        let (port, _) =
            serve_each(vec![("500 Internal Server Error", " no store \n")]);
        let error = admin(port).list().err().unwrap().to_string();
        assert_eq!(
            error,
            "GET /v1/credentials?vault=default: HTTP 500: no store"
        );
    }

    const CONSENT: &str =
        r#"{"authorization_url":"https://a.example/authorize?state=s"}"#;
    const JIRA_STORED: &str = r#"{"keys":["JIRA"],"credentials":[
        {"key":"JIRA","type":"oauth","client_secret":"••••••••",
         "connected_at":"t0","last_refreshed_at":"t1",
         "authorization_url":"https://a.example/authorize",
         "token_url":"https://a.example/token","client_id":"stored",
         "scopes":"read","token_auth_method":"client_secret_basic"}]}"#;

    fn body(request: &str) -> &str {
        let (head, body) = request.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("POST /v1/credentials/oauth/connect "));
        body
    }

    #[test]
    fn a_login_sends_exactly_the_client_it_is_given() {
        let state = scratch_dir("agent-vault-oauth-begin");
        let (port, server) = serve_each(vec![
            ("200 OK", JIRA_STORED),
            ("200 OK", CONSENT),
            ("200 OK", r#"{"keys":[]}"#),
            ("200 OK", CONSENT),
            ("200 OK", r#"{"keys":[]}"#),
            ("400 Bad Request", r#"{"error":"bad token_url"}"#),
        ]);
        let vault = logged_in(&state, port, Rc::new(FakeSandbox::default()));
        let client = |secret: Option<&str>| OAuthClient {
            authorization_url: "https://a.example/authorize".into(),
            token_url: "https://a.example/token".into(),
            client_id: "c1".into(),
            client_secret: secret.map(|value| Secret::new(value.into())),
            scopes: "read".into(),
            token_auth_method: String::new(),
        };
        let login = vault.oauth_begin("JIRA", Some(&client(Some("s3cret"))));
        let login = login.unwrap();
        assert_eq!(login.url, "https://a.example/authorize?state=s");
        assert_eq!((login.key.as_str(), login.marker.as_str()), ("JIRA", "t1"));
        let login = vault.oauth_begin("JIRA", Some(&client(None))).unwrap();
        assert_eq!(login.marker, "");
        let error = vault.oauth_begin("JIRA", Some(&client(None)));
        let error = error.err().unwrap().to_string();
        assert!(error.starts_with("OAuth login JIRA: POST "), "{error}");

        let requests = server.join().unwrap();
        let fields = r#""authorization_url":"https://a.example/authorize","client_id":"c1""#;
        let rest = r#""key":"JIRA","scopes":"read","token_url":"https://a.example/token","vault":"default"}"#;
        assert!(!requests[1].split("\r\n\r\n").next().unwrap().contains("s3"));
        assert_eq!(
            body(&requests[1]),
            format!(r#"{{{fields},"client_secret":"s3cret",{rest}"#)
        );
        assert_eq!(body(&requests[3]), format!("{{{fields},{rest}"));
    }

    #[test]
    fn a_re_login_sends_the_stored_client_and_keeps_its_secret() {
        let state = scratch_dir("agent-vault-oauth-relogin");
        let no_authorize = r#"{"keys":["JIRA"],"credentials":[
            {"key":"JIRA","type":"oauth","token_url":"https://t.example",
             "client_id":"c1","authorization_url":""}]}"#;
        let (port, server) = serve_each(vec![
            ("200 OK", JIRA_STORED),
            ("200 OK", CONSENT),
            ("200 OK", r#"{"keys":["JIRA"]}"#),
            ("200 OK", no_authorize),
        ]);
        let vault = logged_in(&state, port, Rc::new(FakeSandbox::default()));
        let login = vault.oauth_begin("JIRA", None).unwrap();
        assert_eq!(login.marker, "t1");
        let expected = "JIRA has no OAuth client to log in with: pass \
                        --authorization-url, --token-url and --client-id";
        for _ in 0..2 {
            let error = vault.oauth_begin("JIRA", None).err().unwrap();
            assert_eq!(error.to_string(), expected);
        }
        let requests = server.join().unwrap();
        assert_eq!(
            body(&requests[1]),
            r#"{"authorization_url":"https://a.example/authorize","client_id":"stored","client_secret":"••••••••","key":"JIRA","scopes":"read","token_auth_method":"client_secret_basic","token_url":"https://a.example/token","vault":"default"}"#
        );
    }

    #[test]
    fn the_wait_ends_when_the_refresh_time_moves_or_gives_up() {
        let state = scratch_dir("agent-vault-oauth-wait");
        let (port, server) = serve_each(vec![
            ("200 OK", JIRA_STORED),
            ("200 OK", r#"{"keys":["JIRA"]}"#),
            ("200 OK", JIRA_STORED),
            ("200 OK", JIRA_STORED),
            ("200 OK", JIRA_STORED),
        ]);
        let vault = logged_in(&state, port, Rc::new(FakeSandbox::default()));
        let login = |marker: &str| PendingLogin {
            key: "JIRA".into(),
            url: String::new(),
            marker: marker.into(),
        };
        vault.oauth_wait(&login(""), Duration::ZERO).unwrap();
        let admin = admin(port);
        let twice = Duration::from_millis(200);
        admin.wait_for_tokens(&login("t0"), twice, twice).unwrap();
        let error = admin.wait_for_tokens(&login("t1"), twice, twice);
        assert_eq!(
            error.unwrap_err().to_string(),
            "no login for JIRA yet: the browser page shows the result; \
             check 'hangar credential list'"
        );
        assert_eq!(server.join().unwrap().len(), 5);
    }

    #[test]
    fn the_oauth_callback_is_the_vault_ui_address() {
        let state = scratch_dir("agent-vault-callback");
        let config = r#"{"tower": {"agentVault": {"adminPort": 15000}}}"#;
        let vault =
            agent_vault(config, &state, Rc::new(FakeSandbox::default()));
        assert_eq!(
            vault.oauth_redirect_uri().unwrap(),
            "http://127.0.0.1:15000/v1/oauth/callback"
        );
    }

    #[test]
    fn a_credential_value_travels_only_in_the_body() {
        let (port, server) = serve_each(vec![("200 OK", "{}")]);
        admin(port)
            .post("TOKEN", &Secret::new("the-value".into()))
            .unwrap();
        let request = server.join().unwrap().remove(0);
        let (head, body) = request.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("POST /v1/credentials "));
        assert!(!head.contains("the-value"));
        assert_eq!(
            body,
            r#"{"vault":"default","credentials":{"TOKEN":"the-value"}}"#
        );
    }

    #[test]
    fn deleting_names_the_keys_and_reading_the_policy_parses_it() {
        let (port, server) = serve_each(vec![("200 OK", "{}")]);
        admin(port).delete(&["A".into(), "B".into()]).unwrap();
        let request = server.join().unwrap().remove(0);
        assert!(request.starts_with("DELETE /v1/credentials "));
        assert!(request.ends_with(r#"{"vault":"default","keys":["A","B"]}"#));

        let (port, _) =
            serve_each(vec![("200 OK", r#"{"unmatched_host_policy":"deny"}"#)]);
        assert_eq!(admin(port).policy().unwrap().as_deref(), Some("deny"));
    }
}
