//! Test-only helpers shared across modules.

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::broker::Broker;
use crate::broker::fake::FakeBroker;
use crate::config::{Settings, defaults, resolve};
use crate::error::Result;
use crate::hangar::Hangar;
use crate::json;
use crate::sandbox::Sandbox;
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
/// `<state>-home`, next to it), `sandbox` in place of a real backend and a
/// [`FakeBroker`].
pub(crate) fn hangar_with(
    config: &str,
    state: &Path,
    sandbox: Rc<dyn Sandbox>,
) -> Hangar {
    hangar_with_broker(config, state, sandbox, Box::new(FakeBroker::default()))
}

pub(crate) fn hangar_with_broker(
    config: &str,
    state: &Path,
    sandbox: Rc<dyn Sandbox>,
    broker: Box<dyn Broker>,
) -> Hangar {
    let settings = settings(config);
    let sibling = |suffix: &str| {
        let mut path = state.as_os_str().to_owned();
        path.push(suffix);
        PathBuf::from(path)
    };
    let host_home = sibling("-home");
    fs::create_dir_all(&host_home).unwrap();
    Hangar {
        settings,
        state: StateDir::new(state.to_path_buf()),
        cache_root: sibling("-cache"),
        host_home,
        sandbox,
        broker,
    }
}
