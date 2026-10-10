//! Test doubles: a fake `msb` (shell script, state in files) and a fake
//! agent-vault admin API (a loopback HTTP server that records requests).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};

const FAKE_MSB: &str = include_str!("msb.sh");
// Only a debug hangar takes the fake keychain: a release build would call
// the real one, so the CLI tests don't even compile there.
const _: () = assert!(
    cfg!(debug_assertions),
    "the CLI tests need a debug build (the fake keychain)"
);

/// A host tool that logs its argv to `$HANGAR_FAKE/<tool>` and its stdin
/// to `<tool>.in`, then runs `then` (its exit status).
const RECORDER: &str = r#"tool=$HANGAR_FAKE/${0##*/}
echo "$*" >>"$tool"
cat >>"$tool.in""#;

/// One isolated machine: HOME, the hangar state, the fakes' files and bin.
pub struct Machine {
    pub home: PathBuf,
    pub state: PathBuf,
    pub fake: PathBuf,
}

impl Machine {
    pub fn new(name: &str) -> Self {
        let home = std::env::temp_dir()
            .join(format!("hangar-cli-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let machine = Self {
            state: home.join("state"),
            fake: home.join("fake"),
            home,
        };
        fs::create_dir_all(machine.home.join("bin")).unwrap();
        fs::create_dir_all(&machine.fake).unwrap();
        machine.script("msb", FAKE_MSB);
        // Never has the item, takes any store: `security` exits 44 for a
        // missing item, `secret-tool` 1.
        let missing = if cfg!(target_os = "macos") { 44 } else { 1 };
        let keychain =
            format!("case $1 in -i | store) ;; *) exit {missing} ;; esac");
        machine.recorder("keychain", &keychain);
        // The host's clipboard and browser are never touched.
        for tool in ["pbcopy", "wl-copy", "xclip", "open", "xdg-open"] {
            machine.recorder(tool, "exit 1");
        }
        machine
    }

    /// An executable in the machine's bin, first on PATH.
    pub fn script(&self, name: &str, body: &str) {
        let path = self.home.join("bin").join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    pub fn recorder(&self, name: &str, then: &str) {
        self.script(name, &format!("{RECORDER}\n{then}"));
    }

    /// Makes the fake msb fail at `what` with `why` (see msb.sh).
    pub fn fail(&self, what: &str, why: &str) {
        fs::write(self.fake.join(format!("fail-{what}")), why).unwrap();
    }

    pub fn config(&self, json: &str) -> PathBuf {
        let path = self.home.join("hangar.json");
        fs::write(&path, json).unwrap();
        path
    }

    pub fn msb_log(&self) -> String {
        fs::read_to_string(self.fake.join("msb.log")).unwrap_or_default()
    }

    pub fn fake_file(&self, name: &str) -> String {
        fs::read_to_string(self.fake.join(name)).unwrap_or_default()
    }

    /// Runs hangar with a clean environment: only HOME, PATH (machine bin
    /// first), the fakes' and coverage variables, the fake keychain under
    /// a test-only service name, plus `env`.
    pub fn hangar(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        self.command(args, env).output().unwrap()
    }

    /// Like `hangar`, with `input` on stdin.
    pub fn hangar_with_stdin(
        &self,
        args: &[&str],
        env: &[(&str, &str)],
        input: &str,
    ) -> Output {
        let mut child = self
            .command(args, env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /// Like `hangar`, running in the background with stderr piped.
    pub fn spawn(&self, args: &[&str], env: &[(&str, &str)]) -> Child {
        self.command(args, env)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let path = tools_path(self.home.join("bin"));
        let service = format!("hangar-test-{}", std::process::id());
        let mut command = Command::new(env!("CARGO_BIN_EXE_hangar"));
        command
            .args(args)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", path)
            .env("HANGAR_FAKE", &self.fake)
            .env("HANGAR_KEYCHAIN_SERVICE", service)
            .env("HANGAR_TEST_KEYCHAIN_TOOL", self.home.join("bin/keychain"))
            .envs(env.iter().copied());
        // Lets `cargo llvm-cov` count the binary's runs.
        if let Ok(profile) = std::env::var("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        command
    }
}

/// The caller's PATH (the fake scripts need coreutils, which the Nix build
/// sandbox has only there) minus any directory holding a real `msb`, so a
/// test can never drive real VMs.
fn tools_path(fakes: PathBuf) -> OsString {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tools =
        std::env::split_paths(&path).filter(|dir| !dir.join("msb").exists());
    std::env::join_paths(std::iter::once(fakes).chain(tools)).unwrap()
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub auth: Option<String>,
    pub body: String,
}

/// agent-vault's admin API, as far as hangar uses it. Healthy once the fake
/// msb has "started" the server (`$HANGAR_FAKE/hangar-vault`); with
/// `$HANGAR_FAKE/fail-deny` a deny PATCH succeeds but changes nothing, and
/// `fail-register` or `fail-login` fails the owner's.
pub struct FakeVault {
    pub port: u16,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl FakeVault {
    pub fn start(fake: &Path) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (recorded, fake) = (Arc::clone(&requests), fake.to_path_buf());
        std::thread::spawn(move || {
            let mut vault = Stored {
                policy: String::from("allow"),
                keys: BTreeSet::new(),
                oauth: BTreeMap::new(),
                logins: 0,
                port,
            };
            for stream in listener.incoming().flatten() {
                serve(stream, &fake, &recorded, &mut vault);
            }
        });
        Self { port, requests }
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    /// Calls with the session token only: health checks, port probes
    /// (`GET /`), the owner's login and the CA are noise for assertions.
    pub fn admin_requests(&self) -> Vec<Request> {
        let public = [
            "/health",
            "/",
            "/v1/auth/register",
            "/v1/auth/login",
            "/v1/mitm/ca.pem",
        ];
        self.requests()
            .into_iter()
            .filter(|request| !public.contains(&request.path.as_str()))
            .collect()
    }
}

/// What the fake vault remembers between requests.
struct Stored {
    policy: String,
    keys: BTreeSet<String>,
    /// An OAuth key's list entry; a login completes the moment it starts.
    oauth: BTreeMap<String, String>,
    logins: usize,
    port: u16,
}

impl Stored {
    fn list(&self) -> String {
        let keys: Vec<String> =
            self.keys.iter().map(|key| format!(r#""{key}""#)).collect();
        let entries: Vec<String> = self
            .keys
            .iter()
            .map(|key| {
                self.oauth.get(key).cloned().unwrap_or_else(|| {
                    format!(r#"{{"key":"{key}","type":"static"}}"#)
                })
            })
            .collect();
        format!(
            r#"{{"keys":[{}],"credentials":[{}]}}"#,
            keys.join(","),
            entries.join(",")
        )
    }

    fn connect(&mut self, body: &str) -> String {
        let field = |name| body_field(body, name);
        let key = field("key");
        self.logins += 1;
        let secret = if body.contains(r#""client_secret""#) {
            r#","client_secret":"••••••••""#
        } else {
            ""
        };
        let entry = format!(
            r#"{{"key":"{key}","type":"oauth","connected_at":"t0","last_refreshed_at":"t{}","authorization_url":"{}","token_url":"{}","client_id":"{}","scopes":"{}"{secret}}}"#,
            self.logins,
            field("authorization_url"),
            field("token_url"),
            field("client_id"),
            field("scopes"),
        );
        self.keys.insert(key.clone());
        self.oauth.insert(key, entry);
        format!(
            r#"{{"authorization_url":"{}?client_id={}&redirect_uri=http%3A%2F%2F127.0.0.1%3A{}%2Fv1%2Foauth%2Fcallback&state=s"}}"#,
            field("authorization_url"),
            field("client_id"),
            self.port
        )
    }
}

fn body_field(body: &str, field: &str) -> String {
    use miniserde::json::Value;
    let Ok(Value::Object(top)) = miniserde::json::from_str::<Value>(body)
    else {
        return String::new();
    };
    match top.get(field) {
        Some(Value::String(value)) => value.clone(),
        _ => String::new(),
    }
}

/// The keys of a credentials body: `credentials` (POST) or `keys` (DELETE).
fn body_keys(body: &str, field: &str) -> Vec<String> {
    use miniserde::json::Value;
    let Ok(Value::Object(top)) = miniserde::json::from_str::<Value>(body)
    else {
        return Vec::new();
    };
    match top.get(field) {
        Some(Value::Object(map)) => map.keys().cloned().collect(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(key) => Some(key.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn serve(
    stream: TcpStream,
    fake: &Path,
    recorded: &Mutex<Vec<Request>>,
    vault: &mut Stored,
) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let (mut length, mut auth) = (0, None);
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':').unwrap();
        let value = value.trim().to_string();
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.parse().unwrap(),
            "authorization" => auth = Some(value),
            _ => {}
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let body = String::from_utf8(body).unwrap();

    let (status, reply) = match (method.as_str(), path.as_str()) {
        ("GET", "/health") if fake.join("hangar-vault").exists() => {
            (200, "{}".to_string())
        }
        ("GET", "/health") => (503, "{}".to_string()),
        ("PATCH", "/v1/vaults/default/settings") => {
            let deny = body.contains(r#""unmatched_host_policy":"deny""#)
                && !fake.join("fail-deny").exists();
            vault.policy = if deny { "deny" } else { "allow" }.to_string();
            (200, "{}".to_string())
        }
        ("GET", "/v1/vaults/default/settings") => (
            200,
            format!(r#"{{"unmatched_host_policy":"{}"}}"#, vault.policy),
        ),
        ("POST", "/v1/auth/register" | "/v1/auth/login") => {
            let action = &path["/v1/auth/".len()..];
            let password = body_field(&body, "password");
            fs::write(fake.join(format!("owner-{action}")), password).unwrap();
            if fake.join(format!("fail-{action}")).exists() {
                (500, format!(r#"{{"error":"{action} failed"}}"#))
            } else {
                (200, r#"{"token":"session-token"}"#.to_string())
            }
        }
        ("POST", "/v1/agents") => {
            (200, r#"{"av_agent_token":"agent-token-1"}"#.to_string())
        }
        ("PUT", "/v1/vaults/default/services") => {
            fs::write(fake.join("services.json"), &body).unwrap();
            (200, "{}".to_string())
        }
        ("GET", "/v1/mitm/ca.pem") => (200, "FAKE-CA\n".to_string()),
        ("GET", "/v1/credentials?vault=default") => (200, vault.list()),
        ("POST", "/v1/credentials/oauth/connect") => {
            (200, vault.connect(&body))
        }
        ("POST", "/v1/credentials") => {
            vault.keys.extend(body_keys(&body, "credentials"));
            (200, "{}".to_string())
        }
        ("DELETE", "/v1/credentials") => {
            for key in body_keys(&body, "keys") {
                vault.keys.remove(&key);
                vault.oauth.remove(&key);
            }
            (200, "{}".to_string())
        }
        _ => (404, "{}".to_string()),
    };
    recorded.lock().unwrap().push(Request {
        method,
        path,
        auth,
        body,
    });
    let mut stream = reader.into_inner();
    let _ = write!(
        stream,
        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
        reply.len()
    );
}
