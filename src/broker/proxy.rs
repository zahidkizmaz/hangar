//! A `proxy` tower: an HTTP forward proxy hangar either runs itself
//! (`local`) or points at (`host`). Bays leave through it and trust the CA
//! it serves; the operator manages what it injects (for example
//! Infisical's agent-vault proxy). hangar sets no routes, credentials or
//! deny mode here, so [`Broker::manages_policy`] is false and `up` skips
//! those steps.

use std::fs::{self, OpenOptions};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::thread;
use std::time::Duration;

use super::{
    Access, Broker, BrokerHealth, Credential, OAuthClient, PendingLogin,
    PublishedPort, Route, UiLogin, no_oauth,
};
use crate::config::{Fields, Tower};
use crate::error::{Context, Error, Result, bail};
use crate::http;
use crate::json::{self, Json};
use crate::process;
use crate::sandbox::{Egress, Sandbox};
use crate::secret::{Secret, trim_line_end};
use crate::state::{StateDir, write_private};

/// The proxy URL's basic-auth user when the tower doesn't name one; the
/// value Infisical's agent-vault proxy expects.
const DEFAULT_TOKEN_USER: &str = "x-agent-vault";
/// The JSON field Infisical's agent-vault proxy serves its CA in.
const DEFAULT_CA_FIELD: &str = "certificate";
/// Infisical's agent-vault proxy listens here.
const DEFAULT_PORT: u16 = 17323;
const CA_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// The command a local proxy starts with, and its subcommand.
const DEFAULT_COMMAND: &str = "infisical";
const DEFAULT_ARGS: [&str; 2] = ["agent-vault", "proxy"];
/// How long a local proxy gets to start listening.
const LOCAL_WAIT_ATTEMPTS: u32 = 30;
const LOCAL_WAIT_PAUSE: Duration = Duration::from_secs(1);

pub(crate) struct Proxy {
    host: String,
    port: u16,
    ca: CaSource,
    ca_format: CaFormat,
    fingerprint: Option<String>,
    token_file: Option<PathBuf>,
    token_user: String,
    local: Option<LocalProxy>,
}

/// A proxy hangar starts and stops on the host. The operator enrolls it
/// once (its `dataDir` holds the enrollment); hangar only runs it.
struct LocalProxy {
    dir: PathBuf,
    domain: String,
    data_dir: PathBuf,
    command: String,
    args: Vec<String>,
    port: u16,
}

/// Where the proxy's CA comes from: a local file, or a URL `up` pulls.
enum CaSource {
    File(PathBuf),
    Url(String),
}

/// How a URL CA answer is read. `Auto` takes the `certificate` field of a
/// JSON object and falls back to the whole body; `Json` insists on a field;
/// `Pem` takes the whole body.
enum CaFormat {
    Auto,
    Json(String),
    Pem,
}

impl CaFormat {
    fn parse(value: &str, field: Option<String>) -> Result<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "json" => Ok(Self::Json(
                field.unwrap_or_else(|| DEFAULT_CA_FIELD.to_string()),
            )),
            "pem" => Ok(Self::Pem),
            other => bail!(
                "config.towers.<name>.proxy.caFormat: \
                 expected auto, json or pem, got {other}"
            ),
        }
    }
}

impl Proxy {
    pub(crate) fn new(
        tower: &Tower,
        sandbox: &dyn Sandbox,
        state: &StateDir,
    ) -> Result<Self> {
        let path = format!("config.towers.{}", tower.name);
        let settings = Fields::new(&tower.config, &path)?.object("proxy")?;
        settings.only(&[
            "host",
            "port",
            "caFile",
            "caFormat",
            "caField",
            "caFingerprint",
            "tokenFile",
            "tokenUser",
            "local",
        ])?;
        let port = settings.optional_number("port")?.unwrap_or(DEFAULT_PORT);
        let local = settings
            .optional_object("local")?
            .map(|local| LocalProxy::new(&local, tower, state, port))
            .transpose()?;
        let ca_format = CaFormat::parse(
            &settings
                .optional_string("caFormat")?
                .unwrap_or_else(|| "auto".to_string()),
            settings.optional_string("caField")?,
        )?;
        let ca_file = match settings.optional_string("caFile")? {
            Some(file) => file,
            None if local.is_some() => {
                format!("http://127.0.0.1:{port}/_agent-vault/ca")
            }
            None => bail!("{path}.proxy.caFile: missing"),
        };
        let ca = if ca_file.starts_with("http://")
            || ca_file.starts_with("https://")
        {
            CaSource::Url(ca_file)
        } else {
            CaSource::File(PathBuf::from(ca_file))
        };
        let host = if local.is_some() {
            sandbox.host_address().to_string()
        } else {
            settings.string("host")?
        };
        Ok(Self {
            host,
            port,
            ca,
            ca_format,
            fingerprint: settings.optional_string("caFingerprint")?,
            token_file: settings
                .optional_string("tokenFile")?
                .map(PathBuf::from),
            token_user: settings
                .optional_string("tokenUser")?
                .unwrap_or_else(|| DEFAULT_TOKEN_USER.to_string()),
            local,
        })
    }

    /// The bytes to trust, from a file or pulled from the URL, checked
    /// against `caFingerprint` when set.
    fn ca_pem(&self) -> Result<Vec<u8>> {
        let pem = match &self.ca {
            CaSource::File(path) => fs::read(path).context(path.display())?,
            CaSource::Url(url) => {
                let body = http::get_url(url, CA_TIMEOUT)?.body;
                certificate(&body, &self.ca_format)?
            }
        };
        if let Some(expected) = &self.fingerprint {
            let actual = fingerprint(&pem)?;
            if actual != normalize_fingerprint(expected) {
                bail!(
                    "tower CA at {} has fingerprint {actual}, not {expected}",
                    self.ca.describe()
                );
            }
        }
        Ok(pem)
    }

    /// The bay's login, from `tokenFile` when set: a secret, so it only
    /// ever reaches the bay inside the proxy URL, on stdin.
    fn token(&self) -> Result<Option<Secret>> {
        let Some(file) = &self.token_file else {
            return Ok(None);
        };
        let text = fs::read_to_string(file).context(file.display())?;
        Ok(Some(Secret::new(trim_line_end(&text).to_string())))
    }

    fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

impl LocalProxy {
    fn new(
        local: &Fields,
        tower: &Tower,
        state: &StateDir,
        port: u16,
    ) -> Result<Self> {
        local.only(&["domain", "dataDir", "command", "args"])?;
        let domain = local.string("domain")?;
        let dir = state.tower_dir(&tower.name);
        let data_dir = local
            .optional_string("dataDir")?
            .map_or_else(|| dir.join("proxy"), PathBuf::from);
        let command = local
            .optional_string("command")?
            .unwrap_or_else(|| DEFAULT_COMMAND.to_string());
        let args = local.strings("args")?;
        let args = if args.is_empty() {
            DEFAULT_ARGS.iter().map(|arg| (*arg).to_string()).collect()
        } else {
            args
        };
        Ok(Self {
            dir,
            domain,
            data_dir,
            command,
            args,
            port,
        })
    }

    fn pid_file(&self) -> PathBuf {
        self.dir.join("proxy.pid")
    }

    fn log_file(&self) -> PathBuf {
        self.dir.join("proxy.log")
    }

    fn ensure_running(&self) -> Result<()> {
        if let Some(pid) = read_pid(&self.pid_file())
            && process_alive(pid)
        {
            return Ok(());
        }
        self.start()?;
        self.wait_healthy()
    }

    fn start(&self) -> Result<()> {
        fs::create_dir_all(&self.dir).context(self.dir.display())?;
        fs::create_dir_all(&self.data_dir).context(self.data_dir.display())?;
        let mut command = process::command(&self.command);
        command
            .args(&self.args)
            .arg("--domain")
            .arg(&self.domain)
            .arg("--port")
            .arg(self.port.to_string())
            .arg("--data-dir")
            .arg(&self.data_dir);
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_file())
            .context(self.log_file().display())?;
        let stderr = log.try_clone().context("cloning proxy log")?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr));
        // Its own process group: a hangar exit doesn't take it down.
        command.process_group(0);
        let mut child = process::spawn(&mut command)?;
        write_private(&self.pid_file(), child.id().to_string().as_bytes())?;
        // Reap it in the background so it never lingers as a zombie.
        thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }

    fn wait_healthy(&self) -> Result<()> {
        for _ in 0..LOCAL_WAIT_ATTEMPTS {
            if self.health() {
                return Ok(());
            }
            thread::sleep(LOCAL_WAIT_PAUSE);
        }
        bail!("proxy did not start, see {}", self.log_file().display())
    }

    fn stop(&self) {
        let pid_file = self.pid_file();
        let Some(pid) = read_pid(&pid_file) else {
            return;
        };
        if process_alive(pid) {
            // A negative pid signals the whole process group we gave it.
            let group = format!("-{pid}");
            let _ = process::command("kill")
                .arg("-TERM")
                .arg("--")
                .arg(&group)
                .stderr(Stdio::null())
                .status();
            for _ in 0..50 {
                if !process_alive(pid) {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            if process_alive(pid) {
                let _ = process::command("kill")
                    .arg("-KILL")
                    .arg("--")
                    .arg(&group)
                    .stderr(Stdio::null())
                    .status();
            }
        }
        fs::remove_file(&pid_file).ok();
    }

    fn health(&self) -> bool {
        format!("127.0.0.1:{}", self.port)
            .to_socket_addrs()
            .is_ok_and(|mut addresses| {
                addresses.any(|address| {
                    TcpStream::connect_timeout(&address, PROBE_TIMEOUT).is_ok()
                })
            })
    }
}

fn read_pid(file: &Path) -> Option<u32> {
    fs::read_to_string(file).ok()?.trim().parse().ok()
}

fn process_alive(pid: u32) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // The third field is the state; 'Z' means a zombie, which is dead.
    stat.rsplit_once(')')
        .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
}

impl CaSource {
    fn describe(&self) -> String {
        match self {
            Self::File(path) => path.display().to_string(),
            Self::Url(url) => url.clone(),
        }
    }
}

/// The CA's PEM from a proxy's answer, read as `format` says. The field
/// named by `Json` must be a string; its absence is an error.
fn certificate(body: &str, format: &CaFormat) -> Result<Vec<u8>> {
    let json_certificate = |field: &str| -> Result<Vec<u8>> {
        let Json::Object(map) =
            json::from_str::<Json>(body).context("tower CA: not JSON")?
        else {
            bail!("tower CA: JSON is not an object");
        };
        match map.get(field) {
            Some(Json::String(pem)) => Ok(pem.as_bytes().to_vec()),
            Some(_) => bail!("tower CA: field {field} is not a string"),
            None => bail!("tower CA: no field {field}"),
        }
    };
    match format {
        CaFormat::Json(field) => json_certificate(field),
        CaFormat::Pem => Ok(body.as_bytes().to_vec()),
        CaFormat::Auto => {
            if body.trim_start().starts_with('{')
                && let Ok(Json::Object(map)) = json::from_str::<Json>(body)
                && let Some(Json::String(pem)) = map.get(DEFAULT_CA_FIELD)
            {
                return Ok(pem.as_bytes().to_vec());
            }
            Ok(body.as_bytes().to_vec())
        }
    }
}

/// The X.509 fingerprint of a CA: SHA-256 of its DER, lowercase hex. The
/// PEM armor and its certificate's base64 body are decoded first, so the
/// value matches what a proxy's UI shows.
fn fingerprint(pem: &[u8]) -> Result<String> {
    use base64::Engine as _;
    use sha2::{Digest as _, Sha256};

    let text = String::from_utf8_lossy(pem);
    let body: String = text
        .lines()
        .filter(|line| !line.trim_start().starts_with("-----"))
        .collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .context("tower CA: not a PEM certificate")?;
    Ok(hex(Sha256::digest(&der).as_slice()))
}

/// `sha256:AB:CD…` or `AB:CD…` to lowercase, colon-free hex.
fn normalize_fingerprint(value: &str) -> String {
    let value = value.trim();
    let value = value
        .strip_prefix("sha256:")
        .or_else(|| value.strip_prefix("SHA256:"))
        .unwrap_or(value);
    value
        .chars()
        .filter(|c| *c != ':')
        .flat_map(char::to_lowercase)
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

fn unmanaged(what: &str) -> Error {
    Error::with_hint(
        "this tower is a remote proxy hangar doesn't drive",
        format!("{what} lives in the proxy's own configuration"),
    )
}

impl Broker for Proxy {
    fn manages_policy(&self) -> bool {
        false
    }

    fn has_data(&self) -> Option<PathBuf> {
        None
    }

    fn validate(&self, _route: &Route) -> Result<()> {
        Err(unmanaged("a route"))
    }

    fn ensure_running(&self, _password: &Secret) -> Result<bool> {
        if let Some(local) = &self.local {
            local.ensure_running()?;
        }
        Ok(false)
    }

    fn deny_unlisted(&self) -> Result<()> {
        Err(unmanaged("deny mode"))
    }

    fn set_routes(&self, _routes: &[Route]) -> Result<()> {
        Err(unmanaged("routes"))
    }

    fn credentials(&self) -> Result<Vec<Credential>> {
        Ok(Vec::new())
    }

    fn put_credential(&self, _key: &str, _value: &Secret) -> Result<()> {
        Err(unmanaged("credentials"))
    }

    fn delete_credentials(&self, _keys: &[String]) -> Result<()> {
        Err(unmanaged("credentials"))
    }

    fn oauth_begin(
        &self,
        _key: &str,
        _client: Option<&OAuthClient>,
    ) -> Result<PendingLogin> {
        Err(no_oauth())
    }

    fn access(&self, _bay: &str) -> Result<Access> {
        let userinfo = match self.token()? {
            Some(token) => format!(
                "{}:{}@",
                self.token_user,
                percent_encoding::utf8_percent_encode(
                    token.expose(),
                    percent_encoding::NON_ALPHANUMERIC
                )
            ),
            None => String::new(),
        };
        let proxy_url = format!("http://{userinfo}{}", self.address());
        Ok(Access {
            proxy_url: Secret::new(proxy_url),
            ca_pem: self.ca_pem()?,
            host_port: self.port,
            renewed: false,
        })
    }

    fn egress(&self) -> Egress {
        if self.local.is_some() {
            Egress::OnlyHostPort(self.port)
        } else {
            Egress::Host {
                host: self.host.clone(),
                port: self.port,
            }
        }
    }

    fn stop(&self) -> Result<()> {
        if let Some(local) = &self.local {
            local.stop();
        }
        Ok(())
    }

    fn retain_bays(&self, _bays: &[String]) -> Result<()> {
        Ok(())
    }

    fn health(&self) -> BrokerHealth {
        let reachable = match &self.local {
            Some(local) => local.health(),
            None => self
                .address()
                .to_socket_addrs()
                .ok()
                .and_then(|mut addresses| addresses.next())
                .is_some_and(|address| {
                    TcpStream::connect_timeout(&address, PROBE_TIMEOUT).is_ok()
                }),
        };
        // The proxy's own settings decide what it lets through: unknown.
        BrokerHealth {
            reachable,
            healthy: reachable,
            unlisted: None,
        }
    }

    fn ports(&self) -> Vec<PublishedPort> {
        Vec::new()
    }

    fn ui(&self) -> Result<Option<UiLogin>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CaFormat, Proxy, certificate, fingerprint, normalize_fingerprint,
        process_alive, read_pid,
    };
    use crate::broker::Broker;
    use crate::config::DEFAULT_TOWER;
    use crate::error::Result;
    use crate::sandbox::Egress;
    use crate::sandbox::fake::FakeSandbox;
    use crate::state::StateDir;
    use crate::testing::{scratch_dir, settings};
    use sha2::{Digest as _, Sha256};
    use std::fs;
    use std::rc::Rc;

    fn proxy(config: &str) -> Proxy {
        let mut settings = settings(config);
        let tower = settings.towers.remove(DEFAULT_TOWER).unwrap();
        let sandbox: Rc<dyn crate::sandbox::Sandbox> =
            Rc::new(FakeSandbox::default());
        let state = StateDir::new(scratch_dir("proxy-state"));
        Proxy::new(&tower, sandbox.as_ref(), &state).unwrap()
    }

    fn proxy_error(config: &str) -> String {
        let mut settings = settings(config);
        let tower = settings.towers.remove(DEFAULT_TOWER).unwrap();
        let sandbox: Rc<dyn crate::sandbox::Sandbox> =
            Rc::new(FakeSandbox::default());
        let state = StateDir::new(scratch_dir("proxy-state"));
        match Proxy::new(&tower, sandbox.as_ref(), &state) {
            Ok(_) => panic!("expected an error"),
            Err(error) => error.to_string(),
        }
    }

    const BASE: &str = r#"
        "towers": {"default": {"backend": "proxy", "proxy": {
            "host": "vault.example", "port": 17323,
            "caFile": "%s"
        }}}"#;

    fn with_ca(value: &str) -> String {
        format!(
            r#"{{"sandbox": {{"backend": "msb"}}, {}}}"#,
            BASE.replace("%s", value)
        )
    }

    #[test]
    fn a_file_ca_is_read_and_the_egress_names_the_host() {
        let dir = crate::testing::scratch_dir("proxy-ca");
        let ca = dir.join("ca.pem");
        fs::write(&ca, "PEM\n").unwrap();
        let proxy = proxy(&with_ca(ca.to_str().unwrap()));
        assert_eq!(
            proxy.egress(),
            Egress::Host {
                host: "vault.example".into(),
                port: 17323
            }
        );
        assert_eq!(proxy.access("work").unwrap().host_port, 17323);
    }

    #[test]
    fn a_url_ca_pulls_the_json_certificate() {
        let pem =
            "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
        let body =
            format!(r#"{{"certificate": "{}"}}"#, pem.replace('\n', "\\n"));
        let auto = CaFormat::Auto;
        assert_eq!(certificate(&body, &auto).unwrap(), pem.as_bytes());
        assert_eq!(certificate("RAW", &auto).unwrap(), b"RAW");
        assert_eq!(
            certificate(&body, &CaFormat::Json("certificate".into())).unwrap(),
            pem.as_bytes()
        );
        assert_eq!(certificate("RAW", &CaFormat::Pem).unwrap(), b"RAW");
        assert!(
            certificate("RAW", &CaFormat::Json("certificate".into())).is_err()
        );
        assert!(
            certificate(
                r#"{"other": "x"}"#,
                &CaFormat::Json("certificate".into())
            )
            .is_err()
        );
    }

    #[test]
    fn ca_format_and_field_are_configurable() {
        let dir = crate::testing::scratch_dir("proxy-ca-format");
        let ca = dir.join("ca.pem");
        fs::write(&ca, "PEM\n").unwrap();
        let config = format!(
            r#"{{"sandbox": {{"backend": "msb"}}, "towers": {{"default": {{
                "backend": "proxy", "proxy": {{
                    "host": "h", "caFile": "{}",
                    "caFormat": "json", "caField": "ca"
                }}}}}}}}"#,
            ca.to_str().unwrap()
        );
        // A file CA ignores the URL format; construction still parses it.
        let _ = proxy(&config);
        assert!(CaFormat::parse("nope", None).is_err());
        assert!(matches!(CaFormat::parse("pem", None), Ok(CaFormat::Pem)));
        assert!(matches!(
            CaFormat::parse("json", None),
            Ok(CaFormat::Json(field)) if field == "certificate"
        ));
    }

    #[test]
    fn the_fingerprint_is_the_certificates_der_hash() {
        use base64::Engine as _;
        let der = b"a certificate's DER";
        let pem = format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            base64::engine::general_purpose::STANDARD.encode(der)
        );
        let expected = super::hex(Sha256::digest(der).as_slice());
        assert_eq!(fingerprint(pem.as_bytes()).unwrap(), expected);
        assert_eq!(
            normalize_fingerprint(&format!(
                "SHA256:{}",
                expected.to_uppercase()
            )),
            expected
        );
    }

    #[test]
    fn a_url_ca_honours_the_configured_format() {
        let (port, server) =
            crate::testing::serve_each(vec![("200 OK", r#"{"ca":"PEM"}"#)]);
        let config = format!(
            r#"{{"sandbox": {{"backend": "msb"}}, "towers": {{"default": {{
                "backend": "proxy", "proxy": {{
                    "host": "h",
                    "caFile": "http://127.0.0.1:{port}/ca",
                    "caFormat": "json", "caField": "ca"
                }}}}}}}}"#
        );
        let proxy = proxy(&config);
        assert_eq!(proxy.access("work").unwrap().ca_pem, b"PEM");
        server.join().unwrap();
    }

    #[test]
    fn unknown_and_missing_keys_are_refused() {
        let missing = r#"{"sandbox": {"backend": "msb"},
            "towers": {"default": {"backend": "proxy"}}}"#;
        assert!(proxy_error(missing).contains("config.towers.default.proxy"));
        let unknown = r#"{"sandbox": {"backend": "msb"},
            "towers": {"default": {"backend": "proxy",
                "proxy": {"host": "h", "caFile": "/x", "nope": 1}}}}"#;
        assert!(proxy_error(unknown).contains("unknown setting"));
    }

    #[test]
    fn an_unmanaged_proxy_refuses_policy_calls() {
        let proxy = proxy(&with_ca("/x"));
        assert!(!proxy.manages_policy());
        assert_eq!(proxy.has_data(), None);
        assert_eq!(proxy.ports(), Vec::new());
        let result: Result<()> = proxy.deny_unlisted();
        assert!(result.is_err());
    }

    #[test]
    fn a_local_proxy_is_started_and_stopped() {
        let dir = crate::testing::scratch_dir("local-proxy");
        fs::write(dir.join("token"), "token\n").unwrap();
        let config = format!(
            r#"{{"sandbox": {{"backend": "msb"}}, "towers": {{"default": {{
                "backend": "proxy", "proxy": {{
                    "local": {{
                        "domain": "https://example.com",
                        "dataDir": "{}",
                        "command": "sh",
                        "args": ["-c", "sleep 100"]
                    }},
                    "port": 17323,
                    "tokenFile": "{}"
                }}
            }}}}}}"#,
            dir.join("data").display(),
            dir.join("token").display()
        );
        let proxy = proxy(&config);
        assert!(proxy.local.is_some());
        assert_eq!(proxy.egress(), Egress::OnlyHostPort(17323));
        let local = proxy.local.as_ref().unwrap();
        local.start().unwrap();
        let pid = read_pid(&local.pid_file()).unwrap();
        assert!(process_alive(pid));
        local.stop();
        assert!(!process_alive(pid));
    }
}
