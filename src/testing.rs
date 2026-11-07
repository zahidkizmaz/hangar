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

pub(crate) fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join(format!("hangar-test-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

pub(crate) fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub(crate) fn resolved(config: &str) -> Result<Settings> {
    resolve(defaults()?, json::parse(config)?)
}

pub(crate) fn settings(config: &str) -> Settings {
    resolved(config).unwrap()
}

pub(crate) fn config_error(config: &str) -> String {
    resolved(config).unwrap_err().to_string()
}

pub(crate) const DIRECT: &str =
    r#"{"bays": [{"name": "default", "proxy": null}]}"#;

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

pub(crate) fn hangar_with(
    config: &str,
    state: &Path,
    sandbox: Rc<dyn Sandbox>,
) -> Hangar {
    hangar_holding(config, state, sandbox, no_keychain())
}

pub(crate) fn hangar_holding(
    config: &str,
    state: &Path,
    sandbox: Rc<dyn Sandbox>,
    keychain: Rc<dyn crate::keychain::Keychain>,
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
    let proxies =
        build_proxies(&settings, &state_dir, &host_home, &keychain).unwrap();
    Hangar {
        settings,
        state: state_dir,
        cache_root: sibling("-cache"),
        state_root: sibling("-state"),
        host_home,
        sandbox,
        keychain,
        proxies,
    }
}

pub(crate) fn no_keychain() -> Rc<dyn crate::keychain::Keychain> {
    Rc::new(crate::keychain::fake::FakeKeychain::default())
}

pub(crate) fn settled() -> Rc<FakeSandbox> {
    let sandbox = Rc::new(FakeSandbox::default());
    sandbox.reply("umask 077", "same\n");
    sandbox.reply("hangar-packages /", "{}");
    sandbox.reply("hangar-profile", r#"{"elements":{}}"#);
    sandbox.reply("hangar-setup-check", "done");
    sandbox
}

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
                while !request.contains("\r\n\r\n") {
                    let read = stream.read(&mut buffer).unwrap();
                    request.push_str(&String::from_utf8_lossy(&buffer[..read]));
                }
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                request
            })
            .collect()
    });
    (port, server)
}

pub(crate) fn build_proxy(
    build: fn(&crate::proxy::Spec) -> Result<Box<dyn crate::proxy::Proxy>>,
    name: &str,
    fields: &str,
    location: crate::proxy::Location,
    data: &Path,
    keychain: Rc<crate::keychain::fake::FakeKeychain>,
) -> Result<Box<dyn crate::proxy::Proxy>> {
    let value = json::parse(fields).unwrap();
    let fields =
        crate::config::Fields::new(&value, &format!("proxies.{name}"))?;
    let spec = crate::proxy::Spec {
        secrets: crate::proxy::Secrets::new(name, keychain, &fields)?,
        fields,
        location,
        home: Path::new("/home/u"),
        data: data.to_path_buf(),
    };
    build(&spec)
}
