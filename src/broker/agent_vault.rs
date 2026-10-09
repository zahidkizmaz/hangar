//! agent-vault as hangar's broker, run in the tower: its VM, its CLI (run
//! inside that VM) and its admin API on the host's 127.0.0.1, plus the
//! state files it keeps in `stateDir`. Each bay is one agent-vault agent,
//! `hangar-<bay>`, with its own token.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::thread;
use std::time::Duration;

use super::{Access, Broker, BrokerHealth, PROXY, Policy, Route, UiLogin};
use crate::config::{Fields, valid_bay_name};
use crate::error::{Context, Result, bail};
use crate::http::{self, Request};
use crate::json::{self, Json};
use crate::sandbox::{
    BoxState, Egress, Mount, MountMode, PublishedPort, Sandbox, TOWER_VM,
    VmSpec,
};
use crate::secret::{Secret, random_hex};
use crate::state::write_private;
use log::info;
use miniserde::json::Array;

/// The admin API and the proxy inside the VM.
const ADMIN_PORT: u16 = 14321;
const PROXY_PORT: u16 = 14322;
fn inner_address() -> String {
    format!("http://127.0.0.1:{ADMIN_PORT}")
}
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
        http::get(self.admin_port, "/health", Duration::from_secs(2))
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

    fn exec(&self, command: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>> {
        self.sandbox.exec(TOWER_VM, None, command, stdin)
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

    fn ensure_owner(&self) -> Result<()> {
        let file = self.owner_password();
        let action = if file.exists() {
            "login"
        } else {
            write_private(&file, random_hex(24)?.as_bytes())?;
            "register"
        };
        let password = fs::read(&file).context(file.display())?;
        let result = self.exec(
            &[
                "agent-vault",
                "auth",
                action,
                "--address",
                &inner_address(),
                "--email",
                OWNER_EMAIL,
                "--password-stdin",
            ],
            Some(&password),
        );
        if result.is_err() && action == "register" {
            // Otherwise every later run would try to log in with it.
            fs::remove_file(&file).context(file.display())?;
        }
        result.map(drop)
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
        let agent = vault_agent(bay);
        let renewed = self.admin()?.delete_agent(&agent)?;
        let output = self.exec(
            &[
                "agent-vault",
                "agent",
                "create",
                &agent,
                "--vault",
                "default:proxy",
                "--token-only",
            ],
            None,
        )?;
        let token = Secret::new(String::from_utf8_lossy(&output).trim().into());
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
            let stdin = format!("{}\n", password.expose());
            let start = self.start_server_args();
            let start: Vec<&str> = start.iter().map(String::as_str).collect();
            self.exec(&start, Some(stdin.as_bytes()))?;
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

    /// `service set -f` replaces the whole set. Trap: sent over stdin, not
    /// via the /data mount, because a file deleted and recreated there
    /// stays unreadable inside the VM.
    fn set_routes(&self, routes: &[Route]) -> Result<()> {
        let services = render_services(routes);
        self.exec(
            &["agent-vault", "vault", "service", "set", "-f", "-"],
            Some(services.as_bytes()),
        )
        .map(drop)
    }

    fn credential_keys(&self) -> Result<Vec<String>> {
        self.admin()?.credential_keys()
    }

    fn put_credential(&self, key: &str, value: &Secret) -> Result<()> {
        self.admin()?.post(key, value)
    }

    fn delete_credentials(&self, keys: &[String]) -> Result<()> {
        self.admin()?.delete(keys)
    }

    fn access(&self, bay: &str) -> Result<Access> {
        let (token, renewed) = self.ensure_agent_token(bay)?;
        let ca_pem = self.exec(
            &["agent-vault", "ca", "fetch", "--address", &inner_address()],
            None,
        )?;
        let proxy_url = format!(
            "http://{}:{VAULT}@{}:{}",
            token.expose(),
            self.sandbox.host_address(),
            self.proxy_port
        );
        Ok(Access {
            proxy_url: Secret::new(proxy_url),
            ca_pem,
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
            reachable: healthy
                || http::get(self.admin_port, "/", Duration::from_secs(2))
                    .is_ok(),
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

/// The JSON `vault service set -f` reads: one service per route, with
/// `extra` flattened in.
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

    fn credential_keys(&self) -> Result<Vec<String>> {
        let path = format!("/v1/credentials?vault={VAULT}");
        let list: KeysResponse =
            json::from_str(&self.call("GET", &path, None)?)?;
        Ok(list.keys.unwrap_or_default())
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
        let request = Request {
            method: "POST",
            path: &path,
            token: Some(&self.token),
            body: Some(b"{}"),
        };
        let response =
            http::send(self.port, &request, Duration::from_secs(30))?;
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
        let request = Request {
            method,
            path,
            token: Some(&self.token),
            body: body.map(str::as_bytes),
        };
        let response = http::call(self.port, &request)?;
        Ok(String::from_utf8_lossy(&response).into_owned())
    }
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
struct KeysResponse {
    keys: Option<Vec<String>>,
}

#[derive(miniserde::Deserialize)]
struct Session {
    token: String,
}

#[cfg(test)]
mod tests {
    use super::{Admin, AgentVault, render_services};
    use crate::broker::{Auth, Broker, Route};
    use crate::json;
    use crate::sandbox::fake::FakeSandbox;
    use crate::sandbox::{Egress, TOWER_VM};
    use crate::secret::Secret;
    use crate::testing::{closed_port, scratch_dir, settings};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::rc::Rc;
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    fn agent_vault(
        config: &str,
        state: &Path,
        sandbox: Rc<FakeSandbox>,
    ) -> AgentVault {
        AgentVault::new(&settings(config).tower, sandbox, state).unwrap()
    }

    /// Answers one request per `(status, body)`, in order, and returns
    /// what it received.
    fn serve_each(
        responses: Vec<(&'static str, &'static str)>,
    ) -> (u16, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            responses
                .into_iter()
                .map(|(status, body)| {
                    let (mut stream, _) = listener.accept().unwrap();
                    let mut request = String::new();
                    let mut buffer = [0; 4096];
                    // Head and body may arrive in separate writes.
                    while !complete(&request) {
                        let read = stream.read(&mut buffer).unwrap();
                        request.push_str(&String::from_utf8_lossy(
                            &buffer[..read],
                        ));
                    }
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).unwrap();
                    request
                })
                .collect()
        });
        (port, server)
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

    fn complete(request: &str) -> bool {
        let Some((head, body)) = request.split_once("\r\n\r\n") else {
            return false;
        };
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .and_then(|length| length.parse::<usize>().ok())
            .unwrap_or(0);
        body.len() >= length
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
        sandbox.reply("agent create hangar-work", "work-token\n");
        sandbox.reply("ca fetch", "CA\n");
        let (port, server) = serve_each(vec![("404 Not Found", "{}")]);
        let vault = logged_in(&state, port, sandbox.clone());
        let access = vault.access("work").unwrap();
        assert!(!access.renewed);
        assert_eq!(access.ca_pem, b"CA\n");
        assert_eq!(access.host_port, 14322);
        assert_eq!(
            access.proxy_url.expose(),
            "http://work-token:default@host.fake:14322"
        );
        let requests = server.join().unwrap();
        assert_eq!(
            first_line(&requests[0]),
            "POST /v1/agents/hangar-work/delete HTTP/1.1"
        );
        vault.access("work").unwrap();
        let token = state.join("agent-tokens/work");
        assert_eq!(std::fs::read_to_string(&token).unwrap(), "work-token");
        let mode = std::fs::metadata(&token).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let minted: Vec<String> = sandbox
            .changes()
            .into_iter()
            .filter(|call| call.contains("agent create"))
            .collect();
        assert_eq!(
            minted,
            [format!(
                "exec {TOWER_VM} agent-vault agent create hangar-work --vault \
                 default:proxy --token-only"
            )]
        );
        assert!(!sandbox.changes().iter().any(|call| call.contains("rotate")));
    }

    #[test]
    fn a_lost_token_file_replaces_the_bays_agent_and_says_so() {
        let state = scratch_dir("agent-vault-renew");
        let sandbox = Rc::new(FakeSandbox::default());
        sandbox.reply("agent create", "new-token\n");
        let (port, server) = serve_each(vec![("200 OK", "{}")]);
        let vault = logged_in(&state, port, sandbox);
        let access = vault.access("work").unwrap();
        assert!(access.renewed);
        assert_eq!(
            first_line(&server.join().unwrap()[0]),
            "POST /v1/agents/hangar-work/delete HTTP/1.1"
        );

        let (port, _server) = serve_each(vec![("404 Not Found", "{}")]);
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
    fn credential_keys_are_listed_with_the_session_token() {
        let (port, server) =
            serve_each(vec![("200 OK", r#"{"keys":["A","B"]}"#)]);
        assert_eq!(admin(port).credential_keys().unwrap(), ["A", "B"]);
        let request = server.join().unwrap().remove(0);
        assert!(request.starts_with("GET /v1/credentials?vault=default "));
        assert!(request.contains("Authorization: Bearer session-token"));

        let (port, _) =
            serve_each(vec![("500 Internal Server Error", " no store \n")]);
        let error = admin(port).credential_keys().unwrap_err().to_string();
        assert_eq!(
            error,
            "GET /v1/credentials?vault=default: HTTP 500: no store"
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
