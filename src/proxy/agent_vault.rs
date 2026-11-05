//! An Agent Vault that already runs on the host. A bay logs in with a
//! token the user minted and a vault name: `http://<token>:<vault>@host`.

use std::fs;
use std::path::PathBuf;

use super::ca::CaSpec;
use super::{Health, Login, Proxy, Spec, external_url, loopback_port, only};
use crate::error::{Context, Result};
use crate::files;
use crate::sandbox::Egress;
use crate::secret::{Secret, trim_line_end};

struct AgentVault {
    port: u16,
    ca: CaSpec,
    token_file: PathBuf,
    vault: String,
}

impl Proxy for AgentVault {
    fn kind(&self) -> &'static str {
        "agent-vault"
    }

    fn egress(&self) -> Egress {
        Egress::HostPort(self.port)
    }

    fn ca(&self) -> Result<Option<Vec<u8>>> {
        self.ca.load().map(Some)
    }

    fn login(&self, _bay: &str) -> Result<Option<Login>> {
        let path = &self.token_file;
        let text = fs::read_to_string(path).context(path.display())?;
        Ok(Some(Login {
            user: trim_line_end(&text).to_string(),
            password: Secret::new(self.vault.clone()),
        }))
    }

    fn health(&self) -> Health {
        super::tcp_health(self.port)
    }
}

/// Reads `ca` (required), `tokenFile` and `vault` (default `default`).
pub(crate) fn build(spec: &Spec) -> Result<Box<dyn Proxy>> {
    let fields = &spec.fields;
    only(fields, &["ca", "tokenFile", "vault"])?;
    let port = loopback_port(external_url(spec, "agent-vault")?)?;
    let token_file = fields.string("tokenFile")?;
    Ok(Box::new(AgentVault {
        port,
        ca: CaSpec::parse(&fields.object("ca")?, spec.home)?,
        token_file: files::host_path(&token_file, spec.home)?,
        vault: fields.optional_string("vault")?.unwrap_or("default".into()),
    }))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::config::Fields;
    use crate::json;
    use crate::proxy::{Health, Location};
    use crate::sandbox::Egress;
    use crate::testing::scratch_dir;

    const CA: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/src/proxy/testdata/ca.pem");

    fn built(fields: &str, location: Location) -> Result<Box<dyn Proxy>> {
        let value = json::parse(fields).unwrap();
        let spec = Spec {
            name: "av",
            fields: Fields::new(&value, "proxies.av")?,
            location,
            home: Path::new("/home/u"),
            data: PathBuf::from("/data/av"),
        };
        build(&spec)
    }

    fn external() -> Location {
        Location::External("http://127.0.0.1:14322".into())
    }

    fn with_token(token: &str, extra: &str) -> Result<Box<dyn Proxy>> {
        let file = scratch_dir("agent-vault-token").join("token");
        std::fs::write(&file, token).unwrap();
        built(
            &format!(
                r#"{{"kind": "agent-vault", "ca": {{"file": "{CA}"}},
                    "tokenFile": "{}"{extra}}}"#,
                file.display()
            ),
            external(),
        )
    }

    #[test]
    fn agent_vault_login_from_token_file() {
        let proxy = with_token("av_tok-1\n", "").unwrap();
        assert_eq!(proxy.kind(), "agent-vault");
        assert_eq!(proxy.egress(), Egress::HostPort(14322));
        let login = proxy.login("work").unwrap().unwrap();
        assert_eq!(login.user, "av_tok-1");
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
        let error =
            built(r#"{"kind": "agent-vault", "tokenFile": "/t"}"#, external())
                .err()
                .unwrap();
        assert_eq!(error.to_string(), "proxies.av.ca: missing");
    }

    #[test]
    fn agent_vault_managed_refused() {
        let error = built(r#"{"kind": "agent-vault"}"#, Location::Managed)
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            r#"proxies.av: kind agent-vault runs only external (set "url")"#
        );
    }

    #[test]
    fn agent_vault_token_file_missing_names_path() {
        let proxy = built(
            &format!(
                r#"{{"kind": "agent-vault", "ca": {{"file": "{CA}"}},
                    "tokenFile": "/nope/token"}}"#
            ),
            external(),
        )
        .unwrap();
        let error = proxy.login("work").err().unwrap();
        assert!(error.to_string().starts_with("/nope/token: "), "{error}");
    }

    #[test]
    fn agent_vault_health_is_a_tcp_connect() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let fields = format!(
            r#"{{"kind": "agent-vault", "ca": {{"file": "{CA}"}},
                "tokenFile": "/t"}}"#
        );
        let url = format!("http://127.0.0.1:{port}");
        let proxy = built(&fields, Location::External(url)).unwrap();
        assert!(matches!(proxy.health(), Health::Up));
        drop(listener);
        assert!(matches!(proxy.health(), Health::Down(_)));
    }
}
