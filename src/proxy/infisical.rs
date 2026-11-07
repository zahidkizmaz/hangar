//! Infisical's Agent Vault proxy, managed in its own VM. It enrols with a
//! proxy token on every start; a bay logs in with a session token:
//! `http://x-agent-vault:<session token>@host`. Both are the proxy's
//! [`Secrets`].

use std::path::PathBuf;

use super::ca::{CaSource, CaSpec, sha256_hex};
use super::{
    Health, Location, Login, Managed, ManagedVm, Proxy, ProxyPort, Secrets,
    Slot, Spec, only,
};
use crate::error::{Context, Error, Result, bail};
use crate::http;
use crate::sandbox::{Egress, MountMode, Sandbox};
use crate::secret::Secret;

/// One line, so Renovate can bump it.
const IMAGE: &str = "infisical/cli:0.45.3@sha256:9624fb475ae8a6d45ef815c7a16331c7b902fc429cfac67c70e94db0c8c3a975";
const PORT: u16 = 17323;
const USER: &str = "x-agent-vault";
const CA_PATH: &str = "/_agent-vault/ca";
const CA_FIELD: &str = "certificate";
const CPUS: u32 = 1;
const MEMORY: &str = "512M";
const DATA: &str = "/data";
/// The image's entrypoint runs `infisical`; a shell keeps the VM idle.
const ENTRYPOINT: &str = "/bin/sh";
const PROXY_TOKEN: &str = "proxy-token";
const SESSION_TOKEN: &str = "session-token";
const SLOTS: &[Slot] = &[
    Slot {
        key: PROXY_TOKEN,
        about: "the proxy token from Infisical's proxy setup",
        for_bays: false,
    },
    Slot {
        key: SESSION_TOKEN,
        about: "the session token the bays log in with",
        for_bays: true,
    },
];

/// The token comes on stdin, never argv: the proxy reads it from the
/// environment only. `$1` is the optional domain. The log is moved aside
/// past 10 MiB, keeping one old one.
const START: &str = r#"IFS= read -r INFISICAL_AGENT_VAULT_PROXY_TOKEN || exit 1
export INFISICAL_AGENT_VAULT_PROXY_TOKEN
[ -f /data/proxy.log ] &&
  [ "$(wc -c </data/proxy.log)" -gt 10485760 ] &&
  mv -f /data/proxy.log /data/proxy.log.1
setsid infisical agent-vault proxy --data-dir /data --listen-host 0.0.0.0 \
  ${1:+--domain "$1"} >>/data/proxy.log 2>&1 &"#;

struct Infisical {
    image: String,
    host_port: u16,
    domain: String,
    /// The port of an Infisical on this machine (a loopback `domain`).
    local_port: Option<u16>,
    secrets: Secrets,
    data: PathBuf,
}

impl Proxy for Infisical {
    fn kind(&self) -> &'static str {
        "infisical"
    }

    fn egress(&self) -> Egress {
        Egress::HostPort(self.host_port)
    }

    fn ca(&self) -> Result<Option<Vec<u8>>> {
        let source = CaSource::Url {
            url: format!("http://127.0.0.1:{}{CA_PATH}", self.host_port),
            field: Some(CA_FIELD.into()),
        };
        CaSpec {
            source,
            sha256: None,
        }
        .load()
        .map(Some)
    }

    fn login(&self, _bay: &str) -> Result<Option<Login>> {
        Ok(Some(Login {
            user: Secret::new(USER.into()),
            password: self.secrets.get(SESSION_TOKEN)?,
        }))
    }

    fn health(&self) -> Health {
        match http::get(self.host_port, CA_PATH) {
            Ok(response) if response.status == 200 => Health::Up,
            Ok(response) => Health::Down(format!("HTTP {}", response.status)),
            Err(why) => Health::Down(why.to_string()),
        }
    }

    fn managed(&self) -> Option<&dyn Managed> {
        Some(self)
    }

    fn secrets(&self) -> &'static [Slot] {
        SLOTS
    }
}

impl Managed for Infisical {
    fn vm(&self) -> ManagedVm {
        ManagedVm {
            image: self.image.clone(),
            cpus: CPUS,
            memory: MEMORY.into(),
            publish: vec![ProxyPort::proxy(self.host_port, PORT)],
            user: None,
            mounts: vec![(
                self.data.join("data"),
                DATA.into(),
                MountMode::ReadWrite,
            )],
            entrypoint: Some(ENTRYPOINT.into()),
            host_port: self.local_port,
        }
    }

    fn fingerprint(&self) -> Result<String> {
        let token = self.secrets.get(PROXY_TOKEN)?;
        Ok(sha256_hex(token.expose().as_bytes()))
    }

    fn start(&self, sandbox: &dyn Sandbox, vm: &str) -> Result<()> {
        let token = self.secrets.get(PROXY_TOKEN)?;
        let token = format!("{}\n", token.expose());
        let domain = match self.local_port {
            Some(port) => format!("http://{}:{port}", sandbox.host_address()),
            None => self.domain.clone(),
        };
        let command = ["/bin/sh", "-c", START, "hangar-infisical", &domain];
        sandbox.exec(vm, None, &command, Some(token.as_bytes()))?;
        Ok(())
    }

    fn log_file(&self) -> PathBuf {
        self.data.join("data/proxy.log")
    }
}

/// Reads `domain`, `hostPort` (default 17323) and `image`.
pub(crate) fn build(spec: &Spec) -> Result<Box<dyn Proxy>> {
    let fields = &spec.fields;
    if let Location::External(_) = spec.location {
        bail!("{}: kind infisical runs only managed in v1", fields.path());
    }
    only(fields, &["domain", "hostPort", "image"])?;
    let host_port = if fields.map.contains_key("hostPort") {
        fields.number("hostPort")?
    } else {
        PORT
    };
    let domain = fields.optional_string("domain")?.unwrap_or_default();
    let local_port = local_port(&domain)
        .map_err(|why| Error::new(format!("{}: {why}", fields.at("domain"))))?;
    Ok(Box::new(Infisical {
        image: fields.optional_string("image")?.unwrap_or(IMAGE.into()),
        host_port,
        domain,
        local_port,
        secrets: spec.secrets.clone(),
        data: spec.data.clone(),
    }))
}

/// The port of a loopback `domain`, which the proxy's VM reaches on the
/// host; `None` for any other.
fn local_port(domain: &str) -> Result<Option<u16>> {
    match http::http_authority(domain) {
        Some((host, port)) if http::is_loopback_host(&host) => port
            .context(format!("{domain}: expected http://127.0.0.1:<port>"))
            .map(Some),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::rc::Rc;

    use super::*;
    use crate::config::Fields;
    use crate::json;
    use crate::keychain::fake::FakeKeychain;
    use crate::proxy::Location;
    use crate::proxy::managed::wanted_record;
    use crate::sandbox::fake::FakeSandbox;
    use crate::testing::{scratch_dir, serve_each};

    const PEM: &str = include_str!("testdata/ca.pem");

    struct Setup {
        dir: std::path::PathBuf,
        keychain: Rc<FakeKeychain>,
    }

    impl Setup {
        fn new(name: &str) -> Self {
            Self {
                dir: scratch_dir(name),
                keychain: Rc::new(FakeKeychain::holding(&[
                    ("infi/proxy-token", "pt-SECRET-1"),
                    ("infi/session-token", "st-SECRET-2"),
                ])),
            }
        }

        fn built(&self, extra: &str) -> Result<Box<dyn Proxy>> {
            let fields =
                format!(r#"{{"kind": "infisical", "run": "managed"{extra}}}"#);
            built(&fields, Location::Managed, &self.dir, &self.keychain)
        }

        fn set(&self, account: &str, value: &str) {
            self.keychain
                .items
                .borrow_mut()
                .insert(account.into(), value.into());
        }
    }

    fn built(
        fields: &str,
        location: Location,
        data: &Path,
        keychain: &Rc<FakeKeychain>,
    ) -> Result<Box<dyn Proxy>> {
        let value = json::parse(fields).unwrap();
        let fields = Fields::new(&value, "proxies.infi")?;
        let spec = Spec {
            secrets: Secrets::new("infi", keychain.clone(), &fields)?,
            fields,
            location,
            home: Path::new("/home/u"),
            data: data.join("data"),
        };
        super::super::build(&spec)
    }

    fn started(proxy: &dyn Proxy) -> FakeSandbox {
        let sandbox = FakeSandbox::default();
        proxy.managed().unwrap().start(&sandbox, "vm").unwrap();
        sandbox
    }

    #[test]
    fn infisical_token_only_on_stdin() {
        let setup = Setup::new("infisical-stdin");
        let proxy = setup.built("").unwrap();
        let sandbox = started(proxy.as_ref());
        assert_eq!(*sandbox.stdins.borrow(), [b"pt-SECRET-1\n".to_vec()]);
        let calls = sandbox.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].starts_with("exec vm /bin/sh -c "), "{calls:?}");
        assert!(!calls[0].contains("SECRET"), "{calls:?}");
        assert!(!calls[0].contains("--proxy-token"), "{calls:?}");
    }

    #[test]
    fn a_local_infisical_is_reached_through_the_host_alias_and_its_port() {
        let setup = Setup::new("infisical-local");
        let domain = r#", "domain": "http://127.0.0.1:8080""#;
        let proxy = setup.built(domain).unwrap();
        assert_eq!(proxy.managed().unwrap().vm().host_port, Some(8080));
        let sandbox = started(proxy.as_ref());
        let call = &sandbox.calls.borrow()[0];
        assert!(
            call.ends_with(" hangar-infisical http://host.fake:8080"),
            "{call}"
        );
        let remote = r#", "domain": "https://eu.infisical.com""#;
        let proxy = setup.built(remote).unwrap();
        assert_eq!(proxy.managed().unwrap().vm().host_port, None);
        let portless = r#", "domain": "http://localhost""#;
        let error = setup.built(portless).err().unwrap().to_string();
        assert_eq!(
            error,
            "proxies.infi.domain: http://localhost: expected \
             http://127.0.0.1:<port>"
        );
    }

    #[test]
    fn infisical_domain_is_an_argument_not_a_secret() {
        let setup = Setup::new("infisical-domain");
        let plain = started(setup.built("").unwrap().as_ref());
        assert!(plain.calls.borrow()[0].ends_with(" hangar-infisical "));
        let domain = r#", "domain": "https://eu.infisical.com""#;
        let proxy = setup.built(domain).unwrap();
        let sandbox = started(proxy.as_ref());
        let call = &sandbox.calls.borrow()[0];
        assert!(
            call.ends_with(" hangar-infisical https://eu.infisical.com"),
            "{call}"
        );
        assert!(call.contains(r#"${1:+--domain "$1"}"#), "{call}");
        assert!(call.contains("TOKEN || exit 1"), "{call}");
        assert!(call.contains(">>/data/proxy.log"), "{call}");
        let rotate = r#"[ -f /data/proxy.log ] &&
  [ "$(wc -c </data/proxy.log)" -gt 10485760 ] &&
  mv -f /data/proxy.log /data/proxy.log.1"#;
        assert!(call.contains(rotate), "{call}");
        assert!(call.find(rotate) < call.find("setsid"), "{call}");
    }

    #[test]
    fn infisical_vm_spec_open_egress_data_mount_publish() {
        let setup = Setup::new("infisical-vm");
        let proxy = setup.built(r#", "hostPort": 27323"#).unwrap();
        assert_eq!(proxy.kind(), "infisical");
        assert_eq!(proxy.egress(), Egress::HostPort(27323));
        let managed = proxy.managed().unwrap();
        let vm = managed.vm();
        assert_eq!(vm.image, IMAGE);
        assert_eq!((vm.cpus, vm.memory.as_str()), (1, "512M"));
        assert_eq!(vm.publish, [ProxyPort::proxy(27323, 17323)]);
        assert_eq!(vm.entrypoint.as_deref(), Some("/bin/sh"));
        let data = setup.dir.join("data/data");
        let mount = (data.clone(), "/data".into(), MountMode::ReadWrite);
        assert_eq!(vm.mounts, [mount]);
        assert_eq!(managed.log_file(), data.join("proxy.log"));
        let record = wanted_record(&vm, "f");
        assert_eq!(record.egress.as_deref(), Some("open"));
        let image = r#", "image": "infisical/cli:9""#;
        let other = setup.built(image).unwrap();
        assert_eq!(other.managed().unwrap().vm().image, "infisical/cli:9");
        assert_eq!(other.egress(), Egress::HostPort(17323));
    }

    #[test]
    fn infisical_token_change_changes_fingerprint() {
        let setup = Setup::new("infisical-fingerprint");
        let proxy = setup.built("").unwrap();
        let fingerprint = || proxy.managed().unwrap().fingerprint().unwrap();
        let before = fingerprint();
        assert_eq!(before, sha256_hex(b"pt-SECRET-1"));
        assert_eq!(before.len(), 64);
        setup.set("infi/session-token", "st-other");
        assert_eq!(fingerprint(), before);
        setup.set("infi/proxy-token", "pt-SECRET-3");
        assert_ne!(fingerprint(), before);
    }

    #[test]
    fn infisical_ca_from_proxy_json() {
        let setup = Setup::new("infisical-ca");
        let body = json::stringify(&json::object([(
            "certificate",
            json::string(PEM),
        )]));
        let (port, server) = serve_each(vec![
            ("200 OK", body.leak()),
            ("200 OK", "{}"),
            ("503 Starting", ""),
        ]);
        let proxy = setup.built(&format!(r#", "hostPort": {port}"#)).unwrap();
        assert_eq!(proxy.ca().unwrap().unwrap(), PEM.as_bytes());
        assert!(matches!(proxy.health(), Health::Up));
        assert!(matches!(proxy.health(), Health::Down(_)));
        let requests = server.join().unwrap();
        assert!(
            requests
                .iter()
                .all(|r| r.starts_with("GET /_agent-vault/ca "))
        );
        assert!(matches!(proxy.health(), Health::Down(_)));
    }

    #[test]
    fn infisical_login_is_x_agent_vault_and_session_token() {
        let setup = Setup::new("infisical-login");
        let proxy = setup.built("").unwrap();
        let login = proxy.login("work").unwrap().unwrap();
        assert_eq!(login.user.expose(), "x-agent-vault");
        assert_eq!(login.password.expose(), "st-SECRET-2");
    }

    #[test]
    fn infisical_tokens_are_read_only_when_needed() {
        let setup = Setup::new("infisical-lazy");
        setup.keychain.items.borrow_mut().clear();
        let proxy = setup.built("").unwrap();
        assert_eq!(proxy.secrets(), SLOTS);
        let managed = proxy.managed().unwrap();
        assert_eq!(managed.vm().publish, [ProxyPort::proxy(17323, 17323)]);
        let error = managed.fingerprint().unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("no proxy-token for proxy infi in "),
            "{error}"
        );
        assert_eq!(error.hint(), Some("run 'hangar secret set infi'"));
        let error = proxy.login("work").err().unwrap().to_string();
        assert!(
            error.starts_with("no session-token for proxy infi"),
            "{error}"
        );
    }

    #[test]
    fn infisical_tokens_may_come_from_a_command() {
        let setup = Setup::new("infisical-command");
        let proxy = setup
            .built(
                r#", "secrets": {"session-token":
                    {"command": ["/bin/sh", "-c", "echo st-from-cmd"]}}"#,
            )
            .unwrap();
        let login = proxy.login("work").unwrap().unwrap();
        assert_eq!(login.password.expose(), "st-from-cmd");
    }

    #[test]
    fn infisical_external_url_refused() {
        let setup = Setup::new("infisical-external");
        let fields = r#"{"kind": "infisical", "url": "http://127.0.0.1:1"}"#;
        let url = Location::External("http://127.0.0.1:1".into());
        let error = built(fields, url, &setup.dir, &setup.keychain)
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            "proxies.infi: kind infisical runs only managed in v1"
        );
        let error = setup.built(r#", "vault": "x""#).err().unwrap();
        assert_eq!(error.to_string(), "proxies.infi.vault: unknown setting");
        let error = setup.built(r#", "proxyTokenFile": "x""#).err().unwrap();
        assert_eq!(
            error.to_string(),
            "proxies.infi.proxyTokenFile: unknown setting"
        );
    }

    #[test]
    fn the_local_infisical_example_comes_up_server_first() {
        use crate::commands::{Step, plan_up};
        use crate::testing::{hangar_with, settled};
        let config = include_str!("../../examples/local-infisical/hangar.json");
        let state = scratch_dir("infisical-example");
        let hangar = hangar_with(config, &state, settled());
        let order: Vec<&str> = plan_up(&hangar, &[])
            .unwrap()
            .iter()
            .map(|step| match step {
                Step::Proxy(name, _) => *name,
                Step::Bay(bay) => bay.name,
            })
            .collect();
        assert_eq!(order, ["infisical", "vault", "work"]);
    }
}
