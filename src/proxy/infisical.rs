//! Infisical's Agent Vault proxy, managed in its own VM. It enrols with a
//! proxy token on every start; a bay logs in with a session token:
//! `http://x-agent-vault:<session token>@host`.

use std::path::PathBuf;

use super::ca::{CaSource, CaSpec, sha256_hex};
use super::{
    Health, Location, Login, Managed, ManagedVm, Proxy, Spec, only, read_token,
};
use crate::error::{Result, bail};
use crate::files;
use crate::http;
use crate::sandbox::{Egress, MountMode, Sandbox};
use crate::secret::Secret;

/// One line, so Renovate can bump it.
const IMAGE: &str = "infisical/cli:0.44.15@sha256:846fabd4c4a27e116fc8138f147d3029dd88662602b40373ff21d14b84e81783";
const PORT: u16 = 17323;
const USER: &str = "x-agent-vault";
const CA_PATH: &str = "/_agent-vault/ca";
const CA_FIELD: &str = "certificate";
const CPUS: u32 = 1;
const MEMORY: &str = "512M";
const DATA: &str = "/data";
/// The image's entrypoint runs `infisical`; a shell keeps the VM idle.
const ENTRYPOINT: &str = "/bin/sh";

/// The token comes on stdin, never argv: the proxy reads it from the
/// environment only. `$1` is the optional domain.
const START: &str = r#"IFS= read -r INFISICAL_AGENT_VAULT_PROXY_TOKEN || exit 1
export INFISICAL_AGENT_VAULT_PROXY_TOKEN
setsid infisical agent-vault proxy --data-dir /data --listen-host 0.0.0.0 \
  ${1:+--domain "$1"} >>/data/proxy.log 2>&1 &"#;

struct Infisical {
    image: String,
    host_port: u16,
    domain: String,
    proxy_token: Secret,
    proxy_token_file: PathBuf,
    session_token_file: PathBuf,
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
            password: read_token(&self.session_token_file)?,
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

    fn host_secrets(&self) -> Vec<PathBuf> {
        vec![
            self.proxy_token_file.clone(),
            self.session_token_file.clone(),
        ]
    }
}

impl Managed for Infisical {
    fn vm(&self) -> ManagedVm {
        ManagedVm {
            image: self.image.clone(),
            cpus: CPUS,
            memory: MEMORY.into(),
            publish: (self.host_port, PORT),
            mounts: vec![(
                self.data.join("data"),
                DATA.into(),
                MountMode::ReadWrite,
            )],
            fingerprint: sha256_hex(self.proxy_token.expose().as_bytes()),
            entrypoint: Some(ENTRYPOINT.into()),
        }
    }

    fn start(&self, sandbox: &dyn Sandbox, vm: &str) -> Result<()> {
        let token = format!("{}\n", self.proxy_token.expose());
        let command =
            ["/bin/sh", "-c", START, "hangar-infisical", &self.domain];
        sandbox.exec(vm, None, &command, Some(token.as_bytes()))?;
        Ok(())
    }

    fn log_file(&self) -> PathBuf {
        self.data.join("data/proxy.log")
    }
}

/// Reads `proxyTokenFile` and `sessionTokenFile` (required), `domain`,
/// `hostPort` (default 17323) and `image`.
pub(crate) fn build(spec: &Spec) -> Result<Box<dyn Proxy>> {
    let fields = &spec.fields;
    if let Location::External(_) = spec.location {
        bail!("{}: kind infisical runs only managed in v1", fields.path());
    }
    only(
        fields,
        &[
            "proxyTokenFile",
            "sessionTokenFile",
            "domain",
            "hostPort",
            "image",
        ],
    )?;
    let path = |key| files::host_path(&fields.string(key)?, spec.home);
    let host_port = if fields.map.contains_key("hostPort") {
        fields.number("hostPort")?
    } else {
        PORT
    };
    let proxy_token_file = path("proxyTokenFile")?;
    Ok(Box::new(Infisical {
        image: fields.optional_string("image")?.unwrap_or(IMAGE.into()),
        host_port,
        domain: fields.optional_string("domain")?.unwrap_or_default(),
        proxy_token: read_token(&proxy_token_file)?,
        proxy_token_file,
        session_token_file: path("sessionTokenFile")?,
        data: spec.data.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::config::Fields;
    use crate::json;
    use crate::proxy::Location;
    use crate::proxy::managed::wanted_record;
    use crate::sandbox::fake::FakeSandbox;
    use crate::testing::{scratch_dir, serve_each};

    const PEM: &str = include_str!("testdata/ca.pem");

    struct Files {
        dir: PathBuf,
    }

    impl Files {
        fn new(name: &str) -> Self {
            let dir = scratch_dir(name);
            std::fs::write(dir.join("proxy"), "pt-SECRET-1\n").unwrap();
            std::fs::write(dir.join("session"), "st-SECRET-2\n").unwrap();
            Self { dir }
        }

        fn fields(&self, extra: &str) -> String {
            format!(
                r#"{{"kind": "infisical", "run": "managed",
                    "proxyTokenFile": "{}", "sessionTokenFile": "{}"{extra}}}"#,
                self.dir.join("proxy").display(),
                self.dir.join("session").display()
            )
        }

        fn built(&self, extra: &str) -> Result<Box<dyn Proxy>> {
            built(&self.fields(extra), Location::Managed, &self.dir)
        }
    }

    fn built(
        fields: &str,
        location: Location,
        data: &Path,
    ) -> Result<Box<dyn Proxy>> {
        let value = json::parse(fields).unwrap();
        let spec = Spec {
            fields: Fields::new(&value, "proxies.infi")?,
            location,
            home: Path::new("/home/u"),
            data: data.join("data"),
        };
        build(&spec)
    }

    fn started(proxy: &dyn Proxy) -> FakeSandbox {
        let sandbox = FakeSandbox::default();
        proxy.managed().unwrap().start(&sandbox, "vm").unwrap();
        sandbox
    }

    #[test]
    fn infisical_token_only_on_stdin() {
        let files = Files::new("infisical-stdin");
        let proxy = files.built("").unwrap();
        let sandbox = started(proxy.as_ref());
        assert_eq!(*sandbox.stdins.borrow(), [b"pt-SECRET-1\n".to_vec()]);
        let calls = sandbox.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].starts_with("exec vm /bin/sh -c "), "{calls:?}");
        assert!(!calls[0].contains("SECRET"), "{calls:?}");
        assert!(!calls[0].contains("--proxy-token"), "{calls:?}");
    }

    #[test]
    fn infisical_domain_is_an_argument_not_a_secret() {
        let files = Files::new("infisical-domain");
        let plain = started(files.built("").unwrap().as_ref());
        assert!(plain.calls.borrow()[0].ends_with(" hangar-infisical "));
        let domain = r#", "domain": "https://eu.infisical.com""#;
        let proxy = files.built(domain).unwrap();
        let sandbox = started(proxy.as_ref());
        let call = &sandbox.calls.borrow()[0];
        assert!(
            call.ends_with(" hangar-infisical https://eu.infisical.com"),
            "{call}"
        );
        assert!(call.contains(r#"${1:+--domain "$1"}"#), "{call}");
        assert!(call.contains("TOKEN || exit 1"), "{call}");
        assert!(call.contains(">>/data/proxy.log"), "{call}");
    }

    #[test]
    fn infisical_vm_spec_open_egress_data_mount_publish() {
        let files = Files::new("infisical-vm");
        let proxy = files.built(r#", "hostPort": 27323"#).unwrap();
        assert_eq!(proxy.kind(), "infisical");
        assert_eq!(proxy.egress(), Egress::HostPort(27323));
        let managed = proxy.managed().unwrap();
        let vm = managed.vm();
        assert_eq!(vm.image, IMAGE);
        assert_eq!((vm.cpus, vm.memory.as_str()), (1, "512M"));
        assert_eq!(vm.publish, (27323, 17323));
        assert_eq!(vm.entrypoint.as_deref(), Some("/bin/sh"));
        let data = files.dir.join("data/data");
        let mount = (data.clone(), "/data".into(), MountMode::ReadWrite);
        assert_eq!(vm.mounts, [mount]);
        assert_eq!(managed.log_file(), data.join("proxy.log"));
        assert_eq!(wanted_record(&vm).egress.as_deref(), Some("open"));
        let image = r#", "image": "infisical/cli:9""#;
        let other = files.built(image).unwrap();
        assert_eq!(other.managed().unwrap().vm().image, "infisical/cli:9");
        assert_eq!(other.egress(), Egress::HostPort(17323));
    }

    #[test]
    fn infisical_token_change_changes_fingerprint() {
        let files = Files::new("infisical-fingerprint");
        let fingerprint =
            || files.built("").unwrap().managed().unwrap().vm().fingerprint;
        let before = fingerprint();
        assert_eq!(before, sha256_hex(b"pt-SECRET-1"));
        assert_eq!(before.len(), 64);
        std::fs::write(files.dir.join("proxy"), "pt-SECRET-1").unwrap();
        assert_eq!(fingerprint(), before);
        std::fs::write(files.dir.join("proxy"), "pt-SECRET-3\n").unwrap();
        assert_ne!(fingerprint(), before);
    }

    #[test]
    fn infisical_ca_from_proxy_json() {
        let files = Files::new("infisical-ca");
        let body = json::stringify(&json::object([(
            "certificate",
            json::string(PEM),
        )]));
        let (port, server) = serve_each(vec![
            ("200 OK", body.leak()),
            ("200 OK", "{}"),
            ("503 Starting", ""),
        ]);
        let proxy = files.built(&format!(r#", "hostPort": {port}"#)).unwrap();
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
        let files = Files::new("infisical-login");
        let proxy = files.built("").unwrap();
        let login = proxy.login("work").unwrap().unwrap();
        assert_eq!(login.user.expose(), "x-agent-vault");
        assert_eq!(login.password.expose(), "st-SECRET-2");
        std::fs::write(files.dir.join("session"), "\n").unwrap();
        let error = proxy.login("work").err().unwrap().to_string();
        assert!(error.ends_with("/session is empty"), "{error}");
    }

    #[test]
    fn infisical_host_secrets_are_both_token_files() {
        let files = Files::new("infisical-secrets");
        let proxy = files.built("").unwrap();
        assert_eq!(
            proxy.host_secrets(),
            [files.dir.join("proxy"), files.dir.join("session")]
        );
    }

    #[test]
    fn infisical_empty_proxy_token_is_an_error() {
        let files = Files::new("infisical-empty");
        std::fs::write(files.dir.join("proxy"), " \n").unwrap();
        let error = files.built("").err().unwrap().to_string();
        assert!(error.ends_with("/proxy is empty"), "{error}");
    }

    #[test]
    fn infisical_external_url_refused() {
        let files = Files::new("infisical-external");
        let fields = r#"{"kind": "infisical", "url": "http://127.0.0.1:1"}"#;
        let url = Location::External("http://127.0.0.1:1".into());
        let error = built(fields, url, &files.dir).err().unwrap();
        assert_eq!(
            error.to_string(),
            "proxies.infi: kind infisical runs only managed in v1"
        );
        let error = files.built(r#", "vault": "x""#).err().unwrap();
        assert_eq!(error.to_string(), "proxies.infi.vault: unknown setting");
    }
}
