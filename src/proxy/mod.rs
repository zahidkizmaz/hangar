//! The pluggable proxy layer: what a bay's egress goes through. A kind is
//! one file in this folder plus one line in [`kinds`]; nothing else in
//! `src/` names one.

mod agent_vault;
pub(crate) mod ca;
#[cfg(test)]
pub(crate) mod fake;
mod http;
mod infisical;
pub(crate) mod managed;
mod secrets;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

use crate::config::Fields;
use crate::error::{Context, Error, Result, bail};
use crate::http::{http_authority, is_loopback_host};
use crate::sandbox::{Egress, MountMode, Sandbox};
use crate::secret::Secret;

pub(crate) use secrets::{Secrets, Slot};

pub(crate) struct Login {
    pub(crate) user: Secret,
    pub(crate) password: Secret,
}

pub(crate) enum Health {
    Up,
    Down(String),
}

pub(crate) trait Proxy {
    fn kind(&self) -> &'static str;
    fn egress(&self) -> Egress;
    fn ca(&self) -> Result<Option<Vec<u8>>>;
    fn login(&self, bay: &str) -> Result<Option<Login>>;
    fn health(&self) -> Health;
    fn managed(&self) -> Option<&dyn Managed> {
        None
    }
    fn bay_tokens(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    fn secrets(&self) -> &'static [Slot] {
        &[]
    }
}

pub(crate) trait Managed {
    fn vm(&self) -> ManagedVm;
    /// Changes whenever the VM must be recreated with a fresh data dir,
    /// e.g. a hash of the token it enrols with.
    fn fingerprint(&self) -> Result<String>;
    fn start(&self, sandbox: &dyn Sandbox, vm: &str) -> Result<()>;
    fn log_file(&self) -> PathBuf;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagedVm {
    pub(crate) image: String,
    pub(crate) cpus: u32,
    pub(crate) memory: String,
    pub(crate) publish: Vec<ProxyPort>,
    pub(crate) user: Option<(u32, u32)>,
    pub(crate) mounts: Vec<(PathBuf, String, MountMode)>,
    pub(crate) entrypoint: Option<String>,
    pub(crate) host_port: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProxyPort {
    pub(crate) name: &'static str,
    pub(crate) host: u16,
    pub(crate) vm: u16,
    pub(crate) http: bool,
}

impl ProxyPort {
    pub(crate) fn proxy(host: u16, vm: u16) -> Self {
        Self {
            name: "proxy",
            host,
            vm,
            http: false,
        }
    }
}

impl ManagedVm {
    pub(crate) fn egress(&self) -> Egress {
        self.host_port.map_or(Egress::Open, Egress::OpenAndHostPort)
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Location {
    Managed,
    External(String),
}

pub(crate) struct Spec<'a> {
    pub(crate) fields: Fields<'a>,
    pub(crate) location: Location,
    pub(crate) home: &'a Path,
    pub(crate) data: PathBuf,
    pub(crate) secrets: Secrets,
}

const CORE_KEYS: [&str; 4] = ["kind", "run", "url", "secrets"];

pub(crate) fn only(fields: &Fields, own: &[&str]) -> Result<()> {
    fields.only(&[&CORE_KEYS[..], own].concat())
}

type Build = fn(&Spec) -> Result<Box<dyn Proxy>>;

pub(crate) struct Access {
    pub(crate) url: Option<Secret>,
    pub(crate) ca_pem: Option<Vec<u8>>,
}

const PROXY_VM_PREFIX: &str = "hangar-proxy-";

pub(crate) fn kinds() -> Vec<(&'static str, Build)> {
    vec![
        ("http", http::build),
        ("agent-vault", agent_vault::build),
        ("infisical", infisical::build),
        #[cfg(test)]
        ("fake", fake::build),
    ]
}

fn kind_names() -> Vec<&'static str> {
    let mut names: Vec<&str> = kinds().iter().map(|(name, _)| *name).collect();
    names.sort_unstable();
    names
}

pub(crate) fn check_kind(fields: &Fields) -> Result<Build> {
    let kind = fields.string("kind")?;
    let found = kinds().into_iter().find(|(name, _)| *name == kind);
    if let Some((_, build)) = found {
        return Ok(build);
    }
    bail!(
        "{}: unknown kind \"{kind}\"; known: {}",
        fields.at("kind"),
        kind_names().join(", ")
    )
}

pub(crate) fn build(spec: &Spec) -> Result<Box<dyn Proxy>> {
    let proxy = check_kind(&spec.fields)?(spec)?;
    let own: Vec<String> =
        proxy.bay_tokens().into_iter().map(|(_, key)| key).collect();
    spec.secrets
        .check(proxy.secrets(), &own, spec.fields.path())?;
    Ok(proxy)
}

pub(crate) fn location(fields: &Fields) -> Result<Location> {
    match (
        fields.optional_string("run")?,
        fields.optional_string("url")?,
    ) {
        (Some(run), None) if run == "managed" => Ok(Location::Managed),
        (Some(_), None) => {
            bail!("{}: expected \"managed\"", fields.at("run"))
        }
        (None, Some(url)) => Ok(Location::External(url)),
        (None, None) => {
            bail!("{}: set \"run\": \"managed\" or \"url\"", fields.path())
        }
        (Some(_), Some(_)) => bail!(
            "{}: set \"run\": \"managed\" or \"url\", not both",
            fields.path()
        ),
    }
}

pub(crate) fn proxy_vm(name: &str) -> String {
    format!("{PROXY_VM_PREFIX}{name}")
}

pub(crate) fn external_url<'a>(spec: &'a Spec, kind: &str) -> Result<&'a str> {
    match &spec.location {
        Location::External(url) => Ok(url),
        Location::Managed => bail!(
            "{}: kind {kind} runs only external (set \"url\")",
            spec.fields.path()
        ),
    }
}

const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) fn login_key(proxy: &dyn Proxy, bay: &str) -> Option<String> {
    proxy
        .bay_tokens()
        .into_iter()
        .find(|(owner, _)| owner == bay)
        .map(|(_, key)| key)
        .or_else(|| {
            let slot = proxy.secrets().iter().find(|slot| slot.for_bays)?;
            Some(slot.key.to_string())
        })
}

pub(crate) fn token_keys(proxy: &dyn Proxy) -> Vec<String> {
    proxy
        .secrets()
        .iter()
        .map(|slot| slot.key.to_string())
        .chain(proxy.bay_tokens().into_iter().map(|(_, key)| key))
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LoginCheck {
    Taken,
    Refused(String),
    Unknown,
}

/// Asks the proxy on the loopback `port` whether it takes `login`, the
/// way a bay's client would: a CONNECT with Basic auth. Nothing reaches
/// an upstream; the host is reserved and never resolves.
pub(crate) fn check_login(port: u16, login: &Login) -> LoginCheck {
    use base64::Engine;
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = TcpStream::connect_timeout(&address, HEALTH_TIMEOUT)
    else {
        return LoginCheck::Unknown;
    };
    let _ = stream.set_read_timeout(Some(LOGIN_TIMEOUT));
    let pair = format!("{}:{}", login.user.expose(), login.password.expose());
    let basic =
        Secret::new(base64::engine::general_purpose::STANDARD.encode(pair));
    let target = "hangar-login-check.invalid:443";
    let request = Secret::new(format!(
        "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\
         Proxy-Authorization: Basic {}\r\n\r\n",
        basic.expose()
    ));
    if stream.write_all(request.expose().as_bytes()).is_err() {
        return LoginCheck::Unknown;
    }
    let mut answer = Vec::new();
    let _ = stream.take(4096).read_to_end(&mut answer);
    login_answer(&String::from_utf8_lossy(&answer))
}

const LOGIN_TIMEOUT: Duration = Duration::from_secs(5);

fn login_answer(answer: &str) -> LoginCheck {
    let Some(status) = answer.lines().next() else {
        return LoginCheck::Unknown;
    };
    let code = status.split_whitespace().nth(1).unwrap_or("");
    if !matches!(code, "400" | "401" | "403" | "404" | "407") {
        return if code == "200" {
            LoginCheck::Taken
        } else {
            LoginCheck::Unknown
        };
    }
    let body = answer
        .split_once("\r\n\r\n")
        .map_or("", |(_, body)| body)
        .trim();
    let reason = if body.is_empty() {
        status.splitn(3, ' ').nth(2).unwrap_or(code).trim()
    } else {
        body.lines().next().unwrap_or(body)
    };
    LoginCheck::Refused(reason.chars().take(200).collect())
}

pub(crate) fn tcp_health(port: u16) -> Health {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    match TcpStream::connect_timeout(&address, HEALTH_TIMEOUT) {
        Ok(_) => Health::Up,
        Err(why) => Health::Down(format!("127.0.0.1:{port}: {why}")),
    }
}

pub(crate) fn loopback_port(url: &str) -> Result<u16> {
    let expected = || format!("{url}: expected http://127.0.0.1:<port>");
    let (host, port) = http_authority(url).context(expected())?;
    if !is_loopback_host(&host) {
        return Err(Error::with_hint(
            format!("{url}: remote proxies aren't supported yet"),
            "see docs/proxy-kinds.md",
        ));
    }
    port.context(expected())
}

pub(crate) fn access(
    proxy: Option<(&str, &dyn Proxy)>,
    bay: &str,
    host_alias: &str,
) -> Result<Access> {
    let Some((name, proxy)) = proxy else {
        return Ok(Access {
            url: None,
            ca_pem: None,
        });
    };
    let ca_pem = proxy.ca()?;
    if let Some(pem) = &ca_pem {
        ca::verify(pem, None).context(format!("proxy {name}"))?;
    }
    let login = proxy.login(bay)?;
    Ok(Access {
        url: proxy_url(proxy.egress(), login.as_ref(), host_alias),
        ca_pem,
    })
}

const USERINFO: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

fn proxy_url(
    egress: Egress,
    login: Option<&Login>,
    host_alias: &str,
) -> Option<Secret> {
    let Egress::HostPort(port) = egress else {
        return None;
    };
    let userinfo = login.map_or_else(String::new, |login| {
        let user = utf8_percent_encode(login.user.expose(), USERINFO);
        let password = utf8_percent_encode(login.password.expose(), USERINFO);
        format!("{user}:{password}@")
    });
    Some(Secret::new(format!("http://{userinfo}{host_alias}:{port}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;
    use crate::testing::serve_each;

    fn a_login(user: &str, password: &str) -> Login {
        Login {
            user: Secret::new(user.into()),
            password: Secret::new(password.into()),
        }
    }

    #[test]
    fn a_login_check_is_a_connect_with_basic_auth_and_never_leaves() {
        let (port, server) = serve_each(vec![
            (
                "407 Proxy Authentication Required",
                "invalid or expired session\n",
            ),
            ("200 Connection established", ""),
        ]);
        assert_eq!(
            check_login(port, &a_login("av_tok", "default")),
            LoginCheck::Refused("invalid or expired session".into())
        );
        assert_eq!(
            check_login(port, &a_login("good", "default")),
            LoginCheck::Taken
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with(
                "CONNECT hangar-login-check.invalid:443 HTTP/1.1\r\n"
            ),
            "{requests:?}"
        );
        assert!(
            requests[0].contains(
                "Proxy-Authorization: Basic YXZfdG9rOmRlZmF1bHQ=\r\n"
            ),
            "{requests:?}"
        );
        assert_eq!(
            check_login(closed_port(), &a_login("a", "b")),
            LoginCheck::Unknown
        );
    }

    #[test]
    fn only_a_refusal_fails_a_login_and_names_why() {
        let answer = |status: &str, body: &str| {
            login_answer(&format!("HTTP/1.1 {status}\r\nX: y\r\n\r\n{body}"))
        };
        assert_eq!(
            answer("404 Not Found", "vault not found\n"),
            LoginCheck::Refused("vault not found".into())
        );
        assert_eq!(
            answer("403 Forbidden", ""),
            LoginCheck::Refused("Forbidden".into())
        );
        assert_eq!(answer("502 Bad Gateway", "dns"), LoginCheck::Unknown);
        assert_eq!(answer("200 OK", ""), LoginCheck::Taken);
        assert_eq!(login_answer(""), LoginCheck::Unknown);
    }
    use crate::proxy::fake::FakeProxy;
    use crate::sandbox::Egress;
    use crate::testing::{build_proxy, closed_port};

    fn built(text: &str) -> Result<Box<dyn Proxy>> {
        let (data, keychain) = (Path::new("/data/p"), std::rc::Rc::default());
        build_proxy(build, "p", text, Location::Managed, data, keychain)
    }

    fn located(text: &str) -> Result<Location> {
        let value = json::parse(text).unwrap();
        location(&Fields::new(&value, "proxies.p")?)
    }

    fn error_of<T>(result: Result<T>) -> String {
        result.err().unwrap().to_string()
    }

    fn login(user: &str, password: &str) -> Login {
        Login {
            user: Secret::new(user.into()),
            password: Secret::new(password.into()),
        }
    }

    #[test]
    fn unknown_kind_lists_known_kinds() {
        let error = error_of(built(r#"{"kind": "socks"}"#));
        assert_eq!(
            error,
            r#"proxies.p.kind: unknown kind "socks"; known: agent-vault, fake, http, infisical"#
        );
    }

    #[test]
    fn a_secrets_entry_the_kind_does_not_take_is_refused() {
        let error = error_of(built(
            r#"{"kind": "fake", "secrets": {"token": {"command": ["x"]}}}"#,
        ));
        assert_eq!(
            error,
            "proxies.p.secrets.token: unknown token; known: none"
        );
    }

    #[test]
    fn a_known_kind_is_built() {
        let proxy = built(r#"{"kind": "fake", "port": 4000}"#).unwrap();
        assert_eq!(proxy.kind(), "fake");
        assert_eq!(proxy.egress(), Egress::HostPort(4000));
    }

    #[test]
    fn the_fake_kind_reads_its_fields() {
        let proxy = built(
            r#"{"kind": "fake", "ca": "pem", "login": "u:p:q",
                "managed": true}"#,
        )
        .unwrap();
        assert_eq!(proxy.ca().unwrap(), Some(b"pem".to_vec()));
        let login = proxy.login("w").unwrap().unwrap();
        assert_eq!(
            (login.user.expose(), login.password.expose()),
            ("u", "p:q")
        );
        assert!(matches!(proxy.health(), Health::Up));
        let managed = proxy.managed().unwrap();
        assert_eq!(managed.vm().publish, [super::ProxyPort::proxy(3128, 3128)]);
        assert_eq!(managed.log_file(), PathBuf::from("fake.log"));
        managed
            .start(&crate::sandbox::fake::FakeSandbox::default(), "vm")
            .unwrap();
        let plain = FakeProxy {
            up: false,
            ..FakeProxy::at(1)
        };
        assert!(matches!(plain.health(), Health::Down(_)));
        assert!(plain.managed().is_none());
    }

    #[test]
    fn location_needs_exactly_one_of_run_or_url() {
        assert_eq!(
            error_of(located("{}")),
            r#"proxies.p: set "run": "managed" or "url""#
        );
        assert_eq!(
            error_of(located(r#"{"run": "managed", "url": "http://x"}"#)),
            r#"proxies.p: set "run": "managed" or "url", not both"#
        );
        assert_eq!(
            error_of(located(r#"{"run": "elsewhere"}"#)),
            r#"proxies.p.run: expected "managed""#
        );
        assert!(matches!(
            located(r#"{"run": "managed"}"#),
            Ok(Location::Managed)
        ));
        let Ok(Location::External(url)) = located(r#"{"url": "http://x:1"}"#)
        else {
            panic!("expected an external proxy");
        };
        assert_eq!(url, "http://x:1");
    }

    #[test]
    fn proxy_vm_is_prefixed() {
        assert_eq!(proxy_vm("main"), "hangar-proxy-main");
    }

    #[test]
    fn loopback_only_until_remote_proxies_exist() {
        assert_eq!(loopback_port("http://127.0.0.1:3128").unwrap(), 3128);
        assert_eq!(loopback_port("http://localhost:3128").unwrap(), 3128);
        let error = loopback_port("http://vault.example.com:14322")
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            "http://vault.example.com:14322: remote proxies aren't \
             supported yet: see docs/proxy-kinds.md"
        );
        assert_eq!(error.hint(), Some("see docs/proxy-kinds.md"));
        assert!(loopback_port("http://127.0.0.1.evil.com:1").is_err());
        for url in [
            "https://127.0.0.1:1",
            "http://127.0.0.1",
            "ftp://x",
            "http://127.0.0.1:80@evil.com:1",
        ] {
            let error = error_of(loopback_port(url));
            assert!(error.contains("expected http://"), "{error}");
        }
    }

    #[test]
    fn proxy_url_percent_encodes_the_login() {
        let egress = Egress::HostPort(17323);
        let alias = "host.microsandbox.internal";
        let secret =
            proxy_url(egress, Some(&login("x-agent-vault", "a:b@c/d")), alias);
        assert_eq!(
            secret.unwrap().expose(),
            "http://x-agent-vault:a%3Ab%40c%2Fd@host.microsandbox.internal\
             :17323"
        );
        let bare = proxy_url(egress, None, alias).unwrap();
        assert_eq!(bare.expose(), "http://host.microsandbox.internal:17323");
        assert!(proxy_url(Egress::Open, None, alias).is_none());
    }

    #[test]
    fn access_direct_has_no_url_and_no_ca() {
        let access = access(None, "w", "host").unwrap();
        assert!(access.url.is_none());
        assert!(access.ca_pem.is_none());
    }

    #[test]
    fn access_carries_the_proxy_login_and_ca() {
        let pem = include_bytes!("testdata/ca.pem").to_vec();
        let proxy = FakeProxy {
            ca: Some(pem.clone()),
            login: Some(("u".into(), "pw".into())),
            ..FakeProxy::at(5)
        };
        let access = access(Some(("p", &proxy)), "w", "host").unwrap();
        assert_eq!(access.url.unwrap().expose(), "http://u:pw@host:5");
        assert_eq!(access.ca_pem, Some(pem));
    }

    #[test]
    fn access_rejects_a_ca_that_is_not_pem() {
        let proxy = FakeProxy {
            ca: Some(b"nope".to_vec()),
            ..FakeProxy::at(5)
        };
        let error = error_of(access(Some(("p", &proxy)), "w", "host"));
        assert_eq!(error, "proxy p: CA isn't a PEM certificate");
    }

    #[test]
    fn access_refuses_a_ca_that_is_not_exactly_one_certificate() {
        let cert = include_str!("testdata/ca.pem");
        let key = format!(
            "-----BEGIN {0} KEY-----\nAAAA\n-----END {0} KEY-----\n",
            "PRIVATE"
        );
        for pem in [format!("{cert}{cert}"), format!("{cert}{key}")] {
            let proxy = FakeProxy {
                ca: Some(pem.into_bytes()),
                ..FakeProxy::at(5)
            };
            let error = error_of(access(Some(("p", &proxy)), "w", "host"));
            assert_eq!(error, "proxy p: CA must be exactly one certificate");
        }
    }

    #[test]
    fn access_never_debug_prints_the_login() {
        let proxy = FakeProxy {
            login: Some(("u".into(), "hunter2".into())),
            ..FakeProxy::at(5)
        };
        let access = access(Some(("p", &proxy)), "w", "host").unwrap();
        let shown = format!("{:?}", access.url);
        assert!(!shown.contains("hunter2"), "{shown}");
    }

    fn rust_files(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, found);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path);
            }
        }
    }

    #[test]
    fn no_core_module_names_a_kind() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        let forbidden =
            ["infisical", "agent-vault", "agent_vault", "kind() == "];
        let offenders: Vec<String> = files
            .iter()
            .filter(|path| !path.starts_with(src.join("proxy")))
            .flat_map(|path| {
                let text =
                    std::fs::read_to_string(path).unwrap().to_lowercase();
                forbidden
                    .iter()
                    .filter(move |word| text.contains(**word))
                    .map(move |word| format!("{}: {word}", path.display()))
            })
            .collect();
        assert!(offenders.is_empty(), "{offenders:#?}");
    }
}
