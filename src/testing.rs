//! Test-only helpers shared across modules.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::thread::{self, JoinHandle};

use crate::config::{Settings, defaults, resolve};
use crate::error::Result;
use crate::hangar::{Hangar, build_proxies};
use crate::json;
use crate::mounts::Roots;
use crate::sandbox::Sandbox;
use crate::sandbox::fake::FakeSandbox;
use crate::state::StateDir;

/// An empty directory unique to this test process and `name`.
pub(crate) fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join(format!("hangar-test-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A port nothing listens on.
pub(crate) fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// `config` (a JSON text) merged over the defaults.
pub(crate) fn resolved(config: &str) -> Result<Settings> {
    resolve(defaults()?, json::parse(config)?)
}

pub(crate) fn settings(config: &str) -> Settings {
    resolved(config).unwrap()
}

pub(crate) fn config_error(config: &str) -> String {
    resolved(config).unwrap_err().to_string()
}

/// One bay `default` on the open internet.
pub(crate) const DIRECT: &str =
    r#"{"bays": [{"name": "default", "proxy": null}]}"#;

/// A config with one bay `default` holding `bay` (a JSON fragment, e.g.
/// `"apps": ["web"]`), plus top-level `rest`. The bay is direct unless
/// `bay` sets its own `proxy`.
pub(crate) fn one_bay(bay: &str, rest: &str) -> String {
    let proxy = if bay.contains(r#""proxy""#) {
        ""
    } else {
        r#", "proxy": null"#
    };
    let bay = if bay.is_empty() {
        proxy.to_string()
    } else {
        format!("{proxy}, {bay}")
    };
    let rest = if rest.is_empty() {
        String::new()
    } else {
        format!(", {rest}")
    };
    format!(r#"{{"bays": [{{"name": "default"{bay}}}]{rest}}}"#)
}

/// Roots for bay `default` in a scratch home, with `state` (created) and
/// `cache` under it.
pub(crate) fn roots(home: &Path, state: &str, cache: &str) -> Roots {
    let state = home.join(state);
    fs::create_dir_all(&state).unwrap();
    let cache = home.join(cache);
    Roots::new(
        home,
        &state,
        &cache,
        &state.join("bays/default/home"),
        &cache.join("bays/default"),
    )
    .unwrap()
}

/// An environment of just `pairs`, for code that takes a `Var`.
pub(crate) fn vars(
    pairs: &[(&str, &str)],
) -> impl Fn(&str) -> Option<String> + use<> {
    let pairs: Vec<(String, String)> = pairs
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect();
    move |name| {
        pairs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    }
}

/// A `Hangar` on `config` (merged over the defaults), with its state in
/// `state` (the package caches in `<state>-cache` and the user's home in
/// `<state>-home`, next to it) and `sandbox` in place of a real backend.
pub(crate) fn hangar_with(
    config: &str,
    state: &Path,
    sandbox: Rc<dyn Sandbox>,
) -> Hangar {
    let settings = settings(config);
    let sibling = |suffix: &str| {
        let mut path = state.as_os_str().to_owned();
        path.push(suffix);
        PathBuf::from(path)
    };
    let host_home = sibling("-home");
    fs::create_dir_all(&host_home).unwrap();
    let state_dir = StateDir::new(state.to_path_buf());
    let proxies = build_proxies(&settings, &state_dir, &host_home).unwrap();
    Hangar {
        settings,
        state: state_dir,
        cache_root: sibling("-cache"),
        state_root: sibling("-state"),
        host_home,
        sandbox,
        proxies,
    }
}

/// A sandbox whose bays answer like ones that are already in line.
pub(crate) fn settled() -> Rc<FakeSandbox> {
    let sandbox = Rc::new(FakeSandbox::default());
    sandbox.reply("umask 077", "same\n");
    sandbox.reply("hangar-packages /", "{}");
    sandbox.reply("hangar-profile", r#"{"elements":{}}"#);
    sandbox.reply("hangar-setup-check", "done");
    sandbox
}

/// Answers one request per `(status, body)`, in order, and returns
/// what it received.
pub(crate) fn serve_each(
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
                    request.push_str(&String::from_utf8_lossy(&buffer[..read]));
                }
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                // A client may hang up before it has read everything.
                let _ = stream.write_all(response.as_bytes());
                request
            })
            .collect()
    });
    (port, server)
}

fn complete(request: &str) -> bool {
    let Some((head, body)) = request.split_once("\r\n\r\n") else {
        return false;
    };
    let length = head
        .to_ascii_lowercase()
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))
        .and_then(|length| length.parse::<usize>().ok())
        .unwrap_or(0);
    body.len() >= length
}
