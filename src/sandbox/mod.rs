//! What hangar needs from a sandbox, whatever runs it: the contract in
//! docs/sandbox-backends.md. Each backend is one submodule, and only that
//! module knows its CLI, flags and vocabulary.

#[cfg(test)]
pub(crate) mod fake;
mod msb;

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::{Context, Result, bail};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Egress {
    Open,
    HostPort(u16),
    OpenAndHostPort(u16),
    Remote(String, u16),
}

impl Egress {
    pub(crate) fn host_port(&self) -> Option<u16> {
        match self {
            Self::HostPort(port) => Some(*port),
            Self::Open | Self::OpenAndHostPort(_) | Self::Remote(..) => None,
        }
    }

    /// The one address a bay behind this egress can reach.
    pub(crate) fn proxy(&self) -> Option<(&str, u16)> {
        match self {
            Self::HostPort(port) => Some(("127.0.0.1", *port)),
            Self::Remote(host, port) => Some((host, *port)),
            Self::Open | Self::OpenAndHostPort(_) => None,
        }
    }

    pub(crate) fn proxy_address(&self) -> Option<String> {
        self.proxy().map(|(host, port)| format!("{host}:{port}"))
    }

    /// A remote proxy by IP: an msb rule by name breaks a CONNECT through
    /// it, and the bay gets no DNS. Resolved once per `up`; the record
    /// keeps the IP, so a move recreates the bay.
    pub(crate) fn resolved(&self) -> Result<Self> {
        let Self::Remote(host, port) = self else {
            return Ok(self.clone());
        };
        let addresses: Vec<SocketAddr> = (host.as_str(), *port)
            .to_socket_addrs()
            .context(format!("proxy host {host}"))?
            .collect();
        let Some(ipv4) = addresses.iter().find(|address| address.is_ipv4())
        else {
            bail!("proxy host {host}: no IPv4 address");
        };
        Ok(Self::Remote(ipv4.ip().to_string(), *port))
    }

    pub(crate) fn record(&self) -> String {
        match self {
            Self::Open => "open".into(),
            Self::HostPort(port) => format!("host:{port}"),
            Self::OpenAndHostPort(port) => format!("open+host:{port}"),
            Self::Remote(host, port) => format!("remote:{host}:{port}"),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublishedPort {
    pub(crate) app: String,
    pub(crate) name: String,
    pub(crate) host: u16,
    pub(crate) vm: u16,
    pub(crate) purpose: String,
    pub(crate) http: bool,
}

pub(crate) fn data_volume(bay: &str) -> String {
    format!("hangar-bay-{bay}-data")
}

pub(crate) struct Mount<'a> {
    pub(crate) host: &'a Path,
    pub(crate) guest: &'a str,
    pub(crate) mode: MountMode,
    pub(crate) owner: Option<(u32, u32)>,
    pub(crate) file: bool,
}

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
    pub(crate) disk: Option<&'a str>,
    pub(crate) entrypoint: Option<&'a str>,
    /// Folders that need a filesystem of their own, never an overlay
    /// (Docker's storage stacks overlays); created by `ensure_volume`.
    pub(crate) volumes: &'a [Volume<'a>],
    pub(crate) init: Option<&'a str>,
    pub(crate) egress: Egress,
    pub(crate) publish: &'a [(u16, u16)],
    pub(crate) mounts: &'a [Mount<'a>],
    /// Ends up on the host's command line: non-secret values only.
    pub(crate) env: &'a [&'a str],
}

pub(crate) trait Sandbox {
    fn describe(&self, vm: &str) -> Result<(BoxState, String)>;

    fn state(&self, vm: &str) -> Result<BoxState> {
        self.describe(vm).map(|(state, _)| state)
    }

    fn create(&self, spec: &VmSpec) -> Result<()>;
    fn start(&self, vm: &str) -> Result<()>;
    fn stop(&self, vm: &str) -> Result<()>;
    fn remove(&self, vm: &str) -> Result<()>;

    fn stop_if_running(&self, vm: &str) -> Result<bool> {
        let running = self.state(vm)? == BoxState::Running;
        if running {
            self.stop(vm)?;
        }
        Ok(running)
    }

    /// Removes `vm` if it exists. Trap: removing is a hard kill that loses
    /// unsynced writes, so a running VM is stopped first.
    fn discard(&self, vm: &str) -> Result<()> {
        let state = self.state(vm)?;
        if state == BoxState::Running {
            self.stop(vm)?;
        }
        if state != BoxState::Missing {
            self.remove(vm)?;
        }
        Ok(())
    }

    fn ensure_volume(&self, name: &str, size: &str) -> Result<bool>;

    fn remove_volume(&self, name: &str) -> Result<()>;

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

    fn shell(
        &self,
        vm: &str,
        user: &str,
        workdir: &str,
        args: &[String],
        tty: bool,
    ) -> Command;

    fn image_present(&self, image: &str) -> bool;

    fn load_image(&self, tag: &str, archive: Stdio) -> Result<()>;

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
    fn egress_records_name_each_kind_and_its_port() {
        assert_eq!(Egress::Open.record(), "open");
        assert_eq!(Egress::HostPort(5).record(), "host:5");
        assert_eq!(Egress::OpenAndHostPort(5).record(), "open+host:5");
        assert_eq!(Egress::HostPort(5).host_port(), Some(5));
        assert_eq!(Egress::OpenAndHostPort(5).host_port(), None);
        let remote = Egress::Remote("192.0.2.7".into(), 5);
        assert_eq!(remote.record(), "remote:192.0.2.7:5");
        assert_eq!(remote.proxy_address().as_deref(), Some("192.0.2.7:5"));
    }

    #[test]
    fn a_remote_proxy_resolves_to_its_ipv4_address() {
        let named = Egress::Remote("localhost".into(), 14322);
        assert_eq!(
            named.resolved().unwrap(),
            Egress::Remote("127.0.0.1".into(), 14322)
        );
        let pinned = Egress::Remote("192.0.2.7".into(), 1);
        assert_eq!(pinned.resolved().unwrap(), pinned);
        assert_eq!(
            Egress::HostPort(3).resolved().unwrap(),
            Egress::HostPort(3)
        );
        assert!(Egress::Remote("nope.invalid".into(), 1).resolved().is_err());
    }
}
