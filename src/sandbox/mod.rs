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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Egress {
    Open,
    /// Only this port on the host: a bay, to its proxy.
    HostPort(u16),
    /// The internet plus this port on the host: a managed proxy whose
    /// upstream runs on this machine.
    OpenAndHostPort(u16),
}

impl Egress {
    pub(crate) fn record(self) -> String {
        match self {
            Self::Open => "open".into(),
            Self::HostPort(port) => format!("host:{port}"),
            Self::OpenAndHostPort(port) => format!("open+host:{port}"),
        }
    }

    pub(crate) fn from_record(text: &str) -> Option<Self> {
        let port = |port: &str| {
            port.bytes()
                .all(|b| b.is_ascii_digit())
                .then(|| port.parse().ok())
                .flatten()
        };
        match text.split_once(':') {
            None if text == "open" => Some(Self::Open),
            Some(("host", text)) => port(text).map(Self::HostPort),
            Some(("open+host", text)) => port(text).map(Self::OpenAndHostPort),
            _ => None,
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
    /// The app that brings it.
    pub(crate) app: String,
    pub(crate) name: String,
    pub(crate) host: u16,
    pub(crate) vm: u16,
    pub(crate) purpose: String,
    /// Probed with an HTTP GET; a TCP connect otherwise.
    pub(crate) http: bool,
}

/// A bay's named data volume: outlives the VM it is mounted in.
pub(crate) fn data_volume(bay: &str) -> String {
    format!("hangar-bay-{bay}-data")
}

pub(crate) struct Mount<'a> {
    pub(crate) host: &'a Path,
    pub(crate) guest: &'a str,
    pub(crate) mode: MountMode,
    /// `(uid, gid)` the files appear as in the VM.
    pub(crate) owner: Option<(u32, u32)>,
    /// A single file, not a folder.
    pub(crate) file: bool,
}

/// A disk volume that outlives the VM it is mounted in.
pub(crate) struct Volume<'a> {
    pub(crate) name: &'a str,
    pub(crate) guest: &'a str,
    pub(crate) size: &'a str,
}

pub(crate) struct VmSpec<'a> {
    pub(crate) name: &'a str,
    pub(crate) image: &'a str,
    pub(crate) cpus: u32,
    pub(crate) memory: &'a str,
    /// Persistent root disk size; the image's layers when `None`.
    pub(crate) disk: Option<&'a str>,
    /// Replaces the image's entrypoint.
    pub(crate) entrypoint: Option<&'a str>,
    /// Folders that need a filesystem of their own, never an overlay
    /// (Docker's storage stacks overlays); created by `ensure_volume`.
    pub(crate) volumes: &'a [Volume<'a>],
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

    /// Creates the disk volume `name` unless it exists; true when created
    /// now.
    fn ensure_volume(&self, name: &str, size: &str) -> Result<bool>;

    /// A missing volume is fine.
    fn remove_volume(&self, name: &str) -> Result<()>;

    /// Copies `guest` out of the running `vm` to `host`.
    fn copy_out(&self, vm: &str, guest: &str, host: &Path) -> Result<()>;

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

#[cfg(test)]
mod tests {
    use super::{Egress, data_volume};

    #[test]
    fn data_volume_is_named_after_the_bay() {
        assert_eq!(data_volume("work"), "hangar-bay-work-data");
    }

    #[test]
    fn egress_record_round_trips() {
        assert_eq!(Egress::Open.record(), "open");
        assert_eq!(Egress::HostPort(5).record(), "host:5");
        assert_eq!(Egress::OpenAndHostPort(5).record(), "open+host:5");
        for egress in [
            Egress::Open,
            Egress::HostPort(5),
            Egress::OpenAndHostPort(5),
        ] {
            assert_eq!(Egress::from_record(&egress.record()), Some(egress));
        }
        for text in [
            "host:x",
            "host:+5",
            "host:",
            "remote:a:1",
            "",
            "host",
            "open+host:x",
            "open+",
        ] {
            assert_eq!(Egress::from_record(text), None, "{text}");
        }
    }
}
