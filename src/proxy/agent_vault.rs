//! Infisical's open-source Agent Vault server. hangar runs it in its own
//! VM (`"run": "managed"`), or reaches one you run yourself
//! (`"url"`). A bay logs in with an agent token the user minted (the
//! proxy's `token` secret) and a vault name:
//! `http://<token>:<vault>@host`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::ca::{CaSource, CaSpec};
use super::{
    Health, Location, Login, Managed, ManagedVm, Proxy, ProxyPort, Secrets,
    Slot, Spec, external_egress, only,
};
use crate::error::{Result, bail};
use crate::http;
use crate::sandbox::{Egress, MountMode, Sandbox};
use crate::secret::Secret;

const TOKEN: &str = "token";
const MASTER_PASSWORD: &str = "master-password";
const TOKEN_SLOT: Slot = Slot {
    key: TOKEN,
    about: "an agent token from Agent Vault's Agents page, with the proxy \
            role on the vault",
    for_bays: true,
};
const SLOTS: &[Slot] = &[TOKEN_SLOT];
const MANAGED_SLOTS: &[Slot] = &[
    Slot {
        key: MASTER_PASSWORD,
        about: "the server's master password; a new one on first start",
        for_bays: false,
    },
    TOKEN_SLOT,
];

const IMAGE: &str = "infisical/agent-vault:0.40.2@sha256:d09894c86c9b8df64ea37442ace9907dcdf82dabd2730b78b9e0c4c5e070e79a";
const PROXY_PORT: u16 = 14322;
const UI_PORT: u16 = 14321;
const CPUS: u32 = 1;
const MEMORY: &str = "512M";
const DATA: &str = "/data";
const USER: (u32, u32) = (65532, 65532);
const ENTRYPOINT: &str = "/bin/sh";
/// The master password comes on stdin, never argv or env. `$1` is the
/// UI's address on the host, for the links and OAuth redirects it makes.
/// The log is moved aside past 10 MiB, keeping one old one. Trap: the
/// shell reads the password before it backgrounds the server, since the
/// exec may end, closing stdin, before a background reader gets to it;
/// `printf` is a builtin, so it never shows in a process's argv.
const START: &str = r#"IFS= read -r password || exit 1
export HOME=/data AGENT_VAULT_ADDR="$1"
cd /data || exit 1
[ -f server.log ] && [ "$(wc -c <server.log)" -gt 10485760 ] &&
  mv -f server.log server.log.1
printf '%s\n' "$password" |
  setsid agent-vault server --password-stdin --telemetry=false \
    --host 0.0.0.0 --port 14321 >>server.log 2>&1 &"#;

struct AgentVault {
    egress: Egress,
    ca: CaSpec,
    secrets: Secrets,
    vault: String,
    bays: BTreeMap<String, String>,
    server: Option<Server>,
}

fn own_token(bay: &str) -> String {
    format!("{TOKEN}@{bay}")
}

struct Server {
    image: String,
    port: u16,
    ui_port: u16,
    data: PathBuf,
    secrets: Secrets,
}

impl Proxy for AgentVault {
    fn kind(&self) -> &'static str {
        "agent-vault"
    }

    fn egress(&self) -> Egress {
        self.egress.clone()
    }

    fn ca(&self) -> Result<Option<Vec<u8>>> {
        self.ca.load().map(Some)
    }

    /// A bay listed in `bays` logs in to its own vault with its own
    /// token: a shared token with access to several vaults would let any
    /// bay name another one.
    fn login(&self, bay: &str) -> Result<Option<Login>> {
        let (token, vault) = match self.bays.get(bay) {
            Some(vault) => (own_token(bay), vault),
            None => (TOKEN.to_string(), &self.vault),
        };
        Ok(Some(Login {
            user: self.secrets.get(&token)?,
            password: Secret::new(vault.clone()),
        }))
    }

    fn bay_tokens(&self) -> Vec<(String, String)> {
        self.bays
            .keys()
            .map(|bay| (bay.clone(), own_token(bay)))
            .collect()
    }

    fn health(&self) -> Health {
        let Some(server) = &self.server else {
            return self.egress.proxy().map_or(Health::Up, super::tcp_health);
        };
        match http::get(server.ui_port, "/health") {
            Ok(response) if response.status == 200 => Health::Up,
            Ok(response) => Health::Down(format!("HTTP {}", response.status)),
            Err(why) => Health::Down(why.to_string()),
        }
    }

    fn managed(&self) -> Option<&dyn Managed> {
        self.server.as_ref().map(|server| server as &dyn Managed)
    }

    fn secrets(&self) -> &'static [Slot] {
        if self.server.is_some() {
            MANAGED_SLOTS
        } else {
            SLOTS
        }
    }
}

impl Managed for Server {
    fn vm(&self) -> ManagedVm {
        ManagedVm {
            image: self.image.clone(),
            cpus: CPUS,
            memory: MEMORY.into(),
            publish: vec![
                ProxyPort::proxy(self.port, PROXY_PORT),
                ProxyPort {
                    name: "ui",
                    host: self.ui_port,
                    vm: UI_PORT,
                    http: true,
                },
            ],
            user: Some(USER),
            mounts: vec![(
                self.data.join("data"),
                DATA.into(),
                MountMode::ReadWrite,
            )],
            entrypoint: Some(ENTRYPOINT.into()),
            host_port: None,
        }
    }

    /// Constant: nothing hangar holds changes what the vault saved, so its
    /// data is never emptied. A new master password needs
    /// `agent-vault master-password change` instead.
    fn fingerprint(&self) -> Result<String> {
        Ok("agent-vault".into())
    }

    fn start(&self, sandbox: &dyn Sandbox, vm: &str) -> Result<()> {
        let password = self.secrets.get(MASTER_PASSWORD)?;
        let password = format!("{}\n", password.expose());
        let address = format!("http://127.0.0.1:{}", self.ui_port);
        let command = ["/bin/sh", "-c", START, "hangar-agent-vault", &address];
        sandbox.exec(vm, None, &command, Some(password.as_bytes()))?;
        Ok(())
    }

    fn log_file(&self) -> PathBuf {
        self.data.join("data/server.log")
    }
}

fn port_or(spec: &Spec, key: &str, default: u16) -> Result<u16> {
    if spec.fields.map.contains_key(key) {
        spec.fields.number(key)
    } else {
        Ok(default)
    }
}

fn check_vault(name: &str, at: &str) -> Result<()> {
    let slug = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !slug {
        bail!("{at}: {name:?} isn't a vault name (a-z, 0-9 and -)");
    }
    Ok(())
}

fn bay_vaults(spec: &Spec) -> Result<BTreeMap<String, String>> {
    let fields = &spec.fields;
    if !fields.map.contains_key("bays") {
        return Ok(BTreeMap::new());
    }
    let bays = fields.object("bays")?;
    let mut vaults = BTreeMap::new();
    for bay in bays.map.keys() {
        let vault = bays.string(bay)?;
        check_vault(&vault, &bays.at(bay))?;
        vaults.insert(bay.clone(), vault);
    }
    Ok(vaults)
}

pub(crate) fn build(spec: &Spec) -> Result<Box<dyn Proxy>> {
    let fields = &spec.fields;
    let vault = || -> Result<String> {
        let vault =
            fields.optional_string("vault")?.unwrap_or("default".into());
        check_vault(&vault, &fields.at("vault"))?;
        Ok(vault)
    };
    let Location::External(_) = &spec.location else {
        only(fields, &["vault", "bays", "hostPort", "uiPort", "image"])?;
        let port = port_or(spec, "hostPort", PROXY_PORT)?;
        let ui_port = port_or(spec, "uiPort", UI_PORT)?;
        if port == ui_port {
            bail!("{}: hostPort and uiPort must differ", fields.path());
        }
        let ca = CaSpec {
            source: CaSource::Url {
                url: format!("http://127.0.0.1:{ui_port}/v1/mitm/ca.pem"),
                field: None,
            },
            sha256: None,
        };
        return Ok(Box::new(AgentVault {
            egress: Egress::HostPort(port),
            ca,
            secrets: spec.secrets.clone(),
            vault: vault()?,
            bays: bay_vaults(spec)?,
            server: Some(Server {
                image: fields.optional_string("image")?.unwrap_or(IMAGE.into()),
                port,
                ui_port,
                data: spec.data.clone(),
                secrets: spec.secrets.clone(),
            }),
        }));
    };
    only(fields, &["ca", "vault", "bays", "unsafe"])?;
    Ok(Box::new(AgentVault {
        egress: external_egress(spec)?,
        ca: CaSpec::parse(&fields.object("ca")?, spec.home)?,
        secrets: spec.secrets.clone(),
        vault: vault()?,
        bays: bay_vaults(spec)?,
        server: None,
    }))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::rc::Rc;

    use super::*;

    use crate::keychain::fake::FakeKeychain;
    use crate::proxy::{Health, Location};
    use crate::sandbox::Egress;
    use crate::sandbox::fake::FakeSandbox;
    use crate::testing::{build_proxy, serve_each};

    const CA: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/src/proxy/testdata/ca.pem");

    fn built_with(
        fields: &str,
        location: Location,
        keychain: Rc<FakeKeychain>,
    ) -> Result<Box<dyn Proxy>> {
        let data = Path::new("/data/av");
        build_proxy(super::super::build, "av", fields, location, data, keychain)
    }

    fn built(fields: &str, location: Location) -> Result<Box<dyn Proxy>> {
        built_with(fields, location, Rc::new(FakeKeychain::default()))
    }

    fn external() -> Location {
        Location::External("http://127.0.0.1:14322".into())
    }

    fn with_token(token: &str, extra: &str) -> Result<Box<dyn Proxy>> {
        let keychain = FakeKeychain::holding(&[("av/token", token)]);
        built_with(
            &format!(
                r#"{{"kind": "agent-vault", "ca": {{"file": "{CA}"}}{extra}}}"#
            ),
            external(),
            Rc::new(keychain),
        )
    }

    #[test]
    fn agent_vault_login_from_its_token() {
        let proxy = with_token("av_tok-1", "").unwrap();
        assert_eq!(proxy.kind(), "agent-vault");
        assert_eq!(proxy.egress(), Egress::HostPort(14322));
        assert_eq!(proxy.secrets(), SLOTS);
        let login = proxy.login("work").unwrap().unwrap();
        assert_eq!(login.user.expose(), "av_tok-1");
        assert!(!format!("{:?}", login.user).contains("av_tok"));
        assert_eq!(login.password.expose(), "default");
        assert!(format!("{:?}", login.password).find("default").is_none());
        let named = with_token("t", r#", "vault": "ops""#).unwrap();
        assert_eq!(
            named.login("work").unwrap().unwrap().password.expose(),
            "ops"
        );
        assert!(proxy.ca().unwrap().is_some());
    }

    #[test]
    fn agent_vault_requires_ca() {
        let error = built(r#"{"kind": "agent-vault"}"#, external())
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "proxies.av.ca: missing");
    }

    fn built_managed(extra: &str, items: &[(&str, &str)]) -> Box<dyn Proxy> {
        built_with(
            &format!(r#"{{"kind": "agent-vault"{extra}}}"#),
            Location::Managed,
            Rc::new(FakeKeychain::holding(items)),
        )
        .unwrap()
    }

    #[test]
    fn a_managed_server_runs_in_its_own_vm_with_proxy_and_ui_ports() {
        let proxy = built_managed("", &[]);
        assert_eq!(proxy.egress(), Egress::HostPort(14322));
        assert_eq!(proxy.secrets(), MANAGED_SLOTS);
        let managed = proxy.managed().unwrap();
        let vm = managed.vm();
        assert_eq!(vm.image, IMAGE);
        assert_eq!(
            vm.publish,
            [
                ProxyPort::proxy(14322, 14322),
                ProxyPort {
                    name: "ui",
                    host: 14321,
                    vm: 14321,
                    http: true
                }
            ]
        );
        assert_eq!(vm.user, Some((65532, 65532)));
        assert_eq!(
            vm.mounts,
            [(
                PathBuf::from("/data/av/data"),
                "/data".to_string(),
                MountMode::ReadWrite
            )]
        );
        assert_eq!(vm.egress(), Egress::Open);
        assert_eq!(managed.fingerprint().unwrap(), "agent-vault");
        assert_eq!(
            managed.log_file(),
            PathBuf::from("/data/av/data/server.log")
        );
        let moved =
            built_managed(r#", "hostPort": 24322, "uiPort": 24321"#, &[]);
        let ports: Vec<u16> = moved
            .managed()
            .unwrap()
            .vm()
            .publish
            .iter()
            .map(|port| port.host)
            .collect();
        assert_eq!(ports, [24322, 24321]);
        assert_eq!(moved.egress(), Egress::HostPort(24322));
    }

    #[test]
    fn a_managed_server_starts_with_its_master_password_on_stdin_only() {
        let proxy = built_managed("", &[("av/master-password", "mp-SECRET")]);
        let sandbox = FakeSandbox::default();
        proxy.managed().unwrap().start(&sandbox, "vm").unwrap();
        assert_eq!(*sandbox.stdins.borrow(), [b"mp-SECRET\n".to_vec()]);
        let call = &sandbox.calls.borrow()[0];
        assert!(!call.contains("SECRET"), "{call}");
        assert!(call.contains("--password-stdin"), "{call}");
        assert!(call.contains("IFS= read -r password || exit 1"), "{call}");
        assert!(call.contains("--telemetry=false"), "{call}");
        assert!(
            call.ends_with(" hangar-agent-vault http://127.0.0.1:14321"),
            "{call}"
        );
        let missing = built_managed("", &[]);
        let error = missing
            .managed()
            .unwrap()
            .start(&FakeSandbox::default(), "vm")
            .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("no master-password for proxy av"),
            "{error}"
        );
    }

    #[test]
    fn a_managed_server_takes_its_ca_from_its_own_ui() {
        let (port, server) = serve_each(vec![("200 OK", "not a pem")]);
        let proxy = built_managed(&format!(r#", "uiPort": {port}"#), &[]);
        assert!(proxy.ca().is_err());
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /v1/mitm/ca.pem "),
            "{requests:?}"
        );
    }

    #[test]
    fn a_managed_server_is_up_only_when_it_answers_health() {
        let (port, server) =
            serve_each(vec![("200 OK", "ok"), ("503 Unavailable", "")]);
        let proxy = built_managed(&format!(r#", "uiPort": {port}"#), &[]);
        assert!(matches!(proxy.health(), Health::Up));
        assert!(
            matches!(proxy.health(), Health::Down(why) if why == "HTTP 503")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /health "), "{requests:?}");
        let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = silent.local_addr().unwrap().port();
        let proxy = built_managed(&format!(r#", "uiPort": {port}"#), &[]);
        assert!(matches!(proxy.health(), Health::Down(_)));
    }

    #[test]
    fn a_listed_bay_logs_in_to_its_own_vault_with_its_own_token() {
        let proxy = built_managed(
            r#", "bays": {"oss": "oss-vault"}"#,
            &[("av/token", "shared-1"), ("av/token@oss", "own-1")],
        );
        assert_eq!(
            proxy.bay_tokens(),
            [("oss".to_string(), "token@oss".to_string())]
        );
        let own = proxy.login("oss").unwrap().unwrap();
        assert_eq!(own.user.expose(), "own-1");
        assert_eq!(own.password.expose(), "oss-vault");
        let shared = proxy.login("work").unwrap().unwrap();
        assert_eq!(shared.user.expose(), "shared-1");
        assert_eq!(shared.password.expose(), "default");
        let missing = built_managed(r#", "bays": {"oss": "oss-vault"}"#, &[]);
        let error = missing.login("oss").err().unwrap().to_string();
        assert!(
            error.starts_with("no token@oss for proxy av in "),
            "{error}"
        );
    }

    #[test]
    fn vault_names_follow_agent_vaults_rule() {
        let error = |extra: &str| {
            built(
                &format!(r#"{{"kind": "agent-vault"{extra}}}"#),
                Location::Managed,
            )
            .err()
            .unwrap()
            .to_string()
        };
        assert_eq!(
            error(r#", "vault": "Ops""#),
            r#"proxies.av.vault: "Ops" isn't a vault name (a-z, 0-9 and -)"#
        );
        assert_eq!(
            error(r#", "bays": {"oss": "a b"}"#),
            r#"proxies.av.bays.oss: "a b" isn't a vault name (a-z, 0-9 and -)"#
        );
        let command = r#", "bays": {"oss": "oss"},
            "secrets": {"token@oss": {"command": ["true"]}}"#;
        assert!(
            built(
                &format!(r#"{{"kind": "agent-vault"{command}}}"#),
                Location::Managed
            )
            .is_ok()
        );
        assert_eq!(
            error(r#", "secrets": {"token@x": {"command": ["true"]}}"#),
            "proxies.av.secrets.token@x: unknown token; known: master-password, token"
        );
    }

    #[test]
    fn a_managed_server_refuses_external_keys_and_one_port_for_both() {
        let error = |extra: &str| {
            built(
                &format!(r#"{{"kind": "agent-vault"{extra}}}"#),
                Location::Managed,
            )
            .err()
            .unwrap()
            .to_string()
        };
        assert_eq!(
            error(r#", "ca": {"file": "x"}"#),
            "proxies.av.ca: unknown setting"
        );
        assert_eq!(
            error(r#", "hostPort": 5, "uiPort": 5"#),
            "proxies.av: hostPort and uiPort must differ"
        );
        let external = built(
            &format!(r#"{{"kind": "agent-vault", "ca": {{"file": "{CA}"}}}}"#),
            external(),
        )
        .unwrap();
        assert!(external.managed().is_none());
        assert_eq!(external.secrets(), SLOTS);
    }

    #[test]
    fn agent_vault_missing_token_says_how_to_set_it() {
        let fields =
            format!(r#"{{"kind": "agent-vault", "ca": {{"file": "{CA}"}}}}"#);
        let proxy = built(&fields, external()).unwrap();
        let error = proxy.login("work").err().unwrap();
        assert!(
            error.to_string().starts_with("no token for proxy av in "),
            "{error}"
        );
        assert_eq!(error.hint(), Some("run 'hangar secret set av'"));
    }

    #[test]
    fn agent_vault_health_is_a_tcp_connect() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let fields =
            format!(r#"{{"kind": "agent-vault", "ca": {{"file": "{CA}"}}}}"#);
        let url = format!("http://127.0.0.1:{port}");
        let proxy = built(&fields, Location::External(url)).unwrap();
        assert!(matches!(proxy.health(), Health::Up));
        drop(listener);
        assert!(matches!(proxy.health(), Health::Down(_)));
    }

    #[test]
    fn the_local_agent_vault_example_starts_the_vault_then_its_bays() {
        use crate::commands::{Target, plan_up};
        use crate::testing::{hangar_with, scratch_dir, settled};
        let config =
            include_str!("../../examples/local-agent-vault/hangar.json");
        let state = scratch_dir("agent-vault-example");
        let hangar = hangar_with(config, &state, settled());
        let order: Vec<&str> = plan_up(&hangar, &[])
            .unwrap()
            .iter()
            .map(|step| match step {
                Target::Proxy(name, _) => *name,
                Target::Bay(bay) => bay.name,
            })
            .collect();
        assert_eq!(order, ["vault", "work", "oss"]);
        let vault = &hangar.proxies["vault"];
        assert_eq!(
            vault.bay_tokens(),
            [("oss".to_string(), "token@oss".to_string())]
        );
    }

    #[test]
    fn the_remote_agent_vault_example_is_reached_by_its_host() {
        use crate::commands::{Target, plan_up};
        use crate::testing::{hangar_with, scratch_dir, settled};
        let config =
            include_str!("../../examples/remote-agent-vault/hangar.json");
        let state = scratch_dir("remote-agent-vault-example");
        let hangar = hangar_with(config, &state, settled());
        let order: Vec<&str> = plan_up(&hangar, &[])
            .unwrap()
            .iter()
            .map(|step| match step {
                Target::Proxy(name, _) => *name,
                Target::Bay(bay) => bay.name,
            })
            .collect();
        assert_eq!(order, ["vault", "work"]);
        assert_eq!(
            hangar.proxies["vault"].egress(),
            Egress::Remote("vault.example.com".into(), 14322)
        );
    }
}
