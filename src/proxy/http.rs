//! An HTTP proxy that already runs on the host.

use super::ca::CaSpec;
use super::{Health, Login, Proxy, Spec, external_egress, external_url, only};
use crate::error::Result;
use crate::sandbox::Egress;

struct Http {
    egress: Egress,
    ca: Option<CaSpec>,
}

impl Proxy for Http {
    fn kind(&self) -> &'static str {
        "http"
    }

    fn egress(&self) -> Egress {
        self.egress.clone()
    }

    fn ca(&self) -> Result<Option<Vec<u8>>> {
        self.ca.as_ref().map(CaSpec::load).transpose()
    }

    fn login(&self, _bay: &str) -> Result<Option<Login>> {
        Ok(None)
    }

    fn health(&self) -> Health {
        self.egress.proxy().map_or(Health::Up, super::tcp_health)
    }
}

pub(crate) fn build(spec: &Spec) -> Result<Box<dyn Proxy>> {
    only(&spec.fields, &["ca", "unsafe"])?;
    external_url(spec, "http")?;
    let egress = external_egress(spec)?;
    let ca = if spec.fields.map.contains_key("ca") {
        Some(CaSpec::parse(&spec.fields.object("ca")?, spec.home)?)
    } else {
        None
    };
    Ok(Box::new(Http { egress, ca }))
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::path::Path;

    use super::*;

    use crate::proxy::{Health, Location};
    use crate::sandbox::Egress;
    use crate::testing::closed_port;

    const CA: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/src/proxy/testdata/ca.pem");
    const PIN: &str =
        "ae1ec4d78197cbd62b970e2e0d7afc3d5d9a66dbd47f3f39cd56f6fe98387a5f";

    fn built(fields: &str, location: Location) -> Result<Box<dyn Proxy>> {
        let keychain = std::rc::Rc::default();
        let data = Path::new("/data/p");
        crate::testing::build_proxy(
            build, "p", fields, location, data, keychain,
        )
    }

    fn at(port: u16) -> Box<dyn Proxy> {
        let url = format!("http://127.0.0.1:{port}");
        built(r#"{"kind": "http"}"#, Location::External(url)).unwrap()
    }

    #[test]
    fn http_kind_external_only() {
        let error = built(r#"{"kind": "http"}"#, Location::Managed)
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            r#"proxies.p: kind http runs only external (set "url")"#
        );
        let proxy = at(3128);
        assert_eq!(proxy.kind(), "http");
        assert_eq!(proxy.egress(), Egress::HostPort(3128));
        assert!(proxy.managed().is_none());
        assert_eq!(proxy.secrets(), &[]);
    }

    #[test]
    fn http_kind_refuses_a_remote_url_and_unknown_keys() {
        let remote = Location::External("http://example.com:3128".into());
        assert!(built(r#"{"kind": "http"}"#, remote).is_err());
        let local = Location::External("http://127.0.0.1:1".into());
        let error = built(r#"{"kind": "http", "x": 1}"#, local).err().unwrap();
        assert_eq!(error.to_string(), "proxies.p.x: unknown setting");
    }

    #[test]
    fn http_kind_health_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(matches!(at(port).health(), Health::Up));
        drop(listener);
        let Health::Down(why) = at(closed_port()).health() else {
            panic!("expected down");
        };
        assert!(why.starts_with("127.0.0.1:"), "{why}");
    }

    #[test]
    fn http_kind_no_login() {
        assert!(at(3128).login("work").unwrap().is_none());
    }

    #[test]
    fn http_kind_ca_pinned_from_file() {
        let location = || Location::External("http://127.0.0.1:1".into());
        assert_eq!(at(1).ca().unwrap(), None);
        let pinned = format!(
            r#"{{"kind": "http", "ca": {{"file": "{CA}", "sha256": "{PIN}"}}}}"#
        );
        let pem = built(&pinned, location()).unwrap().ca().unwrap().unwrap();
        assert_eq!(pem, std::fs::read(CA).unwrap());
        let wrong = pinned.replace(PIN, "ab");
        let error = built(&wrong, location()).unwrap().ca().err().unwrap();
        assert!(
            error.to_string().contains("doesn't match pinned"),
            "{error}"
        );
    }
}
