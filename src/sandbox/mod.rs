//! What hangar needs from a sandbox, whatever runs it: the contract in
//! docs/sandbox-backends.md. Each backend is one submodule, and only that
//! module knows its CLI, flags and vocabulary.

#[cfg(test)]
pub(crate) mod fake;
mod msb;

use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::Result;

pub(crate) use msb::Msb;

/// The default tower's VM: it keeps the name an existing install has.
pub(crate) const TOWER_VM: &str = "hangar-tower";

/// A tower's VM. The default tower keeps [`TOWER_VM`], so its record and
/// name don't move; named towers get `hangar-tower-<name>`.
pub(crate) fn tower_vm(tower: &str) -> String {
    if tower == crate::config::DEFAULT_TOWER {
        TOWER_VM.to_string()
    } else {
        format!("hangar-tower-{tower}")
    }
}

/// A bay's VM: the prefix keeps every bay name clear of `TOWER_VM`.
pub(crate) fn bay_vm(bay: &str) -> String {
    format!("hangar-bay-{bay}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoxState {
    Missing,
    Stopped,
    Running,
}

impl BoxState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Stopped => "stopped",
            Self::Running => "running",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Egress {
    /// Only this port on the host: a bay, to a local tower's proxy.
    OnlyHostPort(u16),
    /// One host and port: a bay, to a remote tower's proxy.
    Host { host: String, port: u16 },
    /// Anything: the tower needs the internet.
    Open,
}

impl Egress {
    /// How a `VmRecord` writes this to the state dir; `None` for `Open`,
    /// which is never recorded.
    pub(crate) fn record(&self) -> Option<String> {
        match self {
            Self::OnlyHostPort(port) => Some(format!("only:{port}")),
            Self::Host { host, port } => Some(format!("host:{host}:{port}")),
            Self::Open => None,
        }
    }

    /// The inverse of [`Egress::record`]; `None` for an unknown value.
    pub(crate) fn from_record(value: &str) -> Option<Self> {
        if let Some(port) = value.strip_prefix("only:") {
            return Some(Self::OnlyHostPort(port.parse().ok()?));
        }
        let rest = value.strip_prefix("host:")?;
        let (host, port) = rest.rsplit_once(':')?;
        Some(Self::Host {
            host: host.to_string(),
            port: port.parse().ok()?,
        })
    }

    /// The host port a bay reaches, for `access` cross-checks.
    pub(crate) fn port(&self) -> Option<u16> {
        match self {
            Self::OnlyHostPort(port) | Self::Host { port, .. } => Some(*port),
            Self::Open => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountMode {
    ReadOnly,
    ReadWrite,
}

impl MountMode {
    pub(crate) fn writable(writable: bool) -> Self {
        if writable {
            Self::ReadWrite
        } else {
            Self::ReadOnly
        }
    }
}

/// A port a VM publishes on the host's 127.0.0.1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublishedPort {
    /// The app that brings it; `None` for the tower's own.
    pub(crate) app: Option<String>,
    /// The bay that publishes it; `None` for the tower's own.
    pub(crate) bay: Option<String>,
    pub(crate) name: String,
    pub(crate) host: u16,
    pub(crate) vm: u16,
    pub(crate) purpose: String,
    /// Probed with an HTTP GET; a TCP connect otherwise.
    pub(crate) http: bool,
}

impl PublishedPort {
    /// `bay/name` for a bay's port, so two bays' ports never look alike.
    pub(crate) fn owner(&self) -> String {
        match &self.bay {
            Some(bay) => format!("{bay}/{}", self.name),
            None => self.name.clone(),
        }
    }
}

pub(crate) struct Mount<'a> {
    pub(crate) host: &'a Path,
    pub(crate) guest: &'a str,
    pub(crate) mode: MountMode,
    /// `(uid, gid)` the files appear as in the VM.
    pub(crate) owner: Option<(u32, u32)>,
}

pub(crate) struct VmSpec<'a> {
    pub(crate) name: &'a str,
    pub(crate) image: &'a str,
    pub(crate) cpus: u32,
    pub(crate) memory: &'a str,
    /// Persistent root disk size; the image's layers when `None`. Each
    /// `native_fs` folder gets a disk of the same size.
    pub(crate) disk: Option<&'a str>,
    /// Folders that need a filesystem of their own, never an overlay
    /// (Docker's storage stacks overlays).
    pub(crate) native_fs: &'a [&'a str],
    /// Takes over as PID 1 once the VM is set up, on a `/run` of the
    /// sandbox's own; `None` runs the image's entrypoint.
    pub(crate) init: Option<&'a str>,
    pub(crate) egress: Egress,
    /// `(host, guest)` ports, published on the host's loopback only.
    pub(crate) publish: &'a [(u16, u16)],
    /// Applied in order, so enclosing mounts come first.
    pub(crate) mounts: &'a [Mount<'a>],
    /// Ends up on the host's command line: non-secret values only.
    pub(crate) env: &'a [&'a str],
}

pub(crate) trait Sandbox {
    /// The state plus the backend's own words for it, for `status`.
    fn describe(&self, vm: &str) -> Result<(BoxState, String)>;

    fn state(&self, vm: &str) -> Result<BoxState> {
        self.describe(vm).map(|(state, _)| state)
    }

    fn create(&self, spec: &VmSpec) -> Result<()>;
    fn start(&self, vm: &str) -> Result<()>;
    fn stop(&self, vm: &str) -> Result<()>;
    fn remove(&self, vm: &str) -> Result<()>;

    /// Runs `command` in `vm` as `user` (`None`: the image's default);
    /// returns its stdout. Secrets go in `stdin` only, and a failure's
    /// error carries the command's stderr.
    fn exec(
        &self,
        vm: &str,
        user: Option<&str>,
        command: &[&str],
        stdin: Option<&[u8]>,
    ) -> Result<Vec<u8>>;

    /// A shell in `vm` as `user`, starting in `workdir`: interactive
    /// without `args`, else runs them as one command with their quoting
    /// intact.
    fn shell(
        &self,
        vm: &str,
        user: &str,
        workdir: &str,
        args: &[String],
        tty: bool,
    ) -> Command;

    fn image_present(&self, image: &str) -> bool;

    /// Imports an image archive streamed on `archive` under `tag`.
    fn load_image(&self, tag: &str, archive: Stdio) -> Result<()>;

    /// How a VM reaches its host.
    fn host_address(&self) -> &str;
}
