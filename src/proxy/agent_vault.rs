//! An Agent Vault that already runs on the host. A bay logs in with a
//! token the user minted (the proxy's `token` secret) and a vault name:
//! `http://<token>:<vault>@host`.

use super::ca::CaSpec;
use super::{
    Health, Login, Proxy, Secrets, Slot, Spec, external_url, loopback_port,
    only,
};
use crate::error::Result;
use crate::sandbox::Egress;
use crate::secret::Secret;

const TOKEN: &str = "token";
const SLOTS: &[Slot] = &[Slot {
    key: TOKEN,
    about: "the Agent Vault token the bays log in with",
}];

struct AgentVault {
    port: u16,
    ca: CaSpec,
    secrets: Secrets,
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
        Ok(Some(Login {
            user: self.secrets.get(TOKEN)?,
            password: Secret::new(self.vault.clone()),
        }))
    }

    fn health(&self) -> Health {
        super::tcp_health(self.port)
    }

    fn secrets(&self) -> &'static [Slot] {
        SLOTS
    }
}

/// Reads `ca` (required) and `vault` (default `default`).
pub(crate) fn build(spec: &Spec) -> Result<Box<dyn Proxy>> {
    let fields = &spec.fields;
    only(fields, &["ca", "vault"])?;
    let port = loopback_port(external_url(spec, "agent-vault")?)?;
    Ok(Box::new(AgentVault {
        port,
        ca: CaSpec::parse(&fields.object("ca")?, spec.home)?,
        secrets: spec.secrets.clone(),
        vault: fields.optional_string("vault")?.unwrap_or("default".into()),
    }))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::rc::Rc;

    use super::*;
    use crate::config::Fields;
    use crate::json;
    use crate::keychain::fake::FakeKeychain;
    use crate::proxy::{Health, Location};
    use crate::sandbox::Egress;

    const CA: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/src/proxy/testdata/ca.pem");

    fn built_with(
        fields: &str,
        location: Location,
        keychain: Rc<FakeKeychain>,
    ) -> Result<Box<dyn Proxy>> {
        let value = json::parse(fields).unwrap();
        let fields = Fields::new(&value, "proxies.av")?;
        let spec = Spec {
            secrets: Secrets::new("av", keychain, &fields)?,
            fields,
            location,
            home: Path::new("/home/u"),
            data: PathBuf::from("/data/av"),
        };
        super::super::build(&spec)
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
}
