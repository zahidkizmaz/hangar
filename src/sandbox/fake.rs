//! An in-process sandbox for orchestration tests: VM states in memory,
//! every call recorded.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::process::{Command, Stdio};

use super::{BoxState, Egress, Sandbox, VmSpec};
use crate::error::Result;
use crate::process;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Created {
    pub(crate) name: String,
    pub(crate) init: Option<String>,
    pub(crate) native_fs: Vec<String>,
    pub(crate) egress: Egress,
    pub(crate) publish: Vec<(u16, u16)>,
    pub(crate) mounts: Vec<String>,
}

#[derive(Default)]
pub(crate) struct FakeSandbox {
    pub(crate) states: RefCell<BTreeMap<String, BoxState>>,
    /// `create NAME`, `exec NAME COMMAND…`, …; never stdin.
    pub(crate) calls: RefCell<Vec<String>>,
    /// What each `exec` got on stdin, in call order.
    pub(crate) stdins: RefCell<Vec<Vec<u8>>>,
    pub(crate) created: RefCell<Vec<Created>>,
    pub(crate) images: RefCell<Vec<String>>,
    /// `exec` prints the reply of the first pattern its call contains; a
    /// VM has booted unless one says otherwise.
    pub(crate) replies: RefCell<Vec<(String, String)>>,
    /// Used up one by one before `replies`: `None` fails the exec.
    pub(crate) replies_once: RefCell<Vec<(String, Option<String>)>>,
    /// `shell` commands exit 0 (else 1).
    pub(crate) shell_succeeds: Cell<bool>,
    /// `create` leaves the VM behind, then fails.
    pub(crate) fail_create: Cell<bool>,
}

impl FakeSandbox {
    pub(crate) fn with(vms: &[(&str, BoxState)]) -> Self {
        let fake = Self::default();
        for (vm, state) in vms {
            fake.set(vm, *state);
        }
        fake
    }

    /// Answers `exec` calls containing `pattern` with `reply`.
    pub(crate) fn reply(&self, pattern: &str, reply: &str) {
        self.replies
            .borrow_mut()
            .push((pattern.to_string(), reply.to_string()));
    }

    /// Answers the next `exec` containing `pattern` with `reply`, or fails
    /// it when `None`.
    pub(crate) fn reply_once(&self, pattern: &str, reply: Option<&str>) {
        self.replies_once
            .borrow_mut()
            .push((pattern.to_string(), reply.map(String::from)));
    }

    /// Calls that change a VM or run something in it.
    pub(crate) fn changes(&self) -> Vec<String> {
        self.calls
            .borrow()
            .iter()
            .filter(|call| !call.starts_with("describe "))
            .cloned()
            .collect()
    }

    pub(crate) fn set(&self, vm: &str, state: BoxState) {
        self.states.borrow_mut().insert(vm.to_string(), state);
    }

    fn record(&self, call: String) {
        self.calls.borrow_mut().push(call);
    }
}

fn booted(call: &str) -> Vec<u8> {
    if call.contains("hangar-booted") {
        b"running\n".to_vec()
    } else {
        Vec::new()
    }
}

/// `user@vm`, or just `vm` for the image's default user.
fn target(vm: &str, user: Option<&str>) -> String {
    user.map_or_else(|| vm.to_string(), |user| format!("{user}@{vm}"))
}

impl Sandbox for FakeSandbox {
    fn describe(&self, vm: &str) -> Result<(BoxState, String)> {
        self.record(format!("describe {vm}"));
        let state = self
            .states
            .borrow()
            .get(vm)
            .copied()
            .unwrap_or(BoxState::Missing);
        Ok((state, format!("{state:?}")))
    }

    fn create(&self, spec: &VmSpec) -> Result<()> {
        self.record(format!("create {}", spec.name));
        self.created.borrow_mut().push(Created {
            name: spec.name.into(),
            init: spec.init.map(String::from),
            native_fs: spec.native_fs.iter().map(|&path| path.into()).collect(),
            egress: spec.egress,
            publish: spec.publish.to_vec(),
            mounts: spec
                .mounts
                .iter()
                .map(|mount| {
                    let owner = mount
                        .owner
                        .map(|(uid, gid)| format!(":{uid}:{gid}"))
                        .unwrap_or_default();
                    format!(
                        "{}:{}:{:?}{owner}",
                        mount.host.display(),
                        mount.guest,
                        mount.mode
                    )
                })
                .collect(),
        });
        self.set(spec.name, BoxState::Running);
        if self.fail_create.get() {
            crate::error::bail!("create {}: boot timed out", spec.name);
        }
        Ok(())
    }

    fn start(&self, vm: &str) -> Result<()> {
        self.record(format!("start {vm}"));
        self.set(vm, BoxState::Running);
        Ok(())
    }

    fn stop(&self, vm: &str) -> Result<()> {
        self.record(format!("stop {vm}"));
        self.set(vm, BoxState::Stopped);
        Ok(())
    }

    fn remove(&self, vm: &str) -> Result<()> {
        self.record(format!("remove {vm}"));
        self.states.borrow_mut().remove(vm);
        Ok(())
    }

    fn exec(
        &self,
        vm: &str,
        user: Option<&str>,
        command: &[&str],
        stdin: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        self.stdins
            .borrow_mut()
            .push(stdin.unwrap_or_default().to_vec());
        let call = format!("exec {} {}", target(vm, user), command.join(" "));
        let once = self
            .replies_once
            .borrow()
            .iter()
            .position(|(pattern, _)| call.contains(pattern.as_str()));
        if let Some(at) = once {
            let (_, reply) = self.replies_once.borrow_mut().remove(at);
            self.record(call.clone());
            return match reply {
                Some(reply) => Ok(reply.into_bytes()),
                None => crate::error::bail!("{call}: failed"),
            };
        }
        let reply = self
            .replies
            .borrow()
            .iter()
            .find(|(pattern, _)| call.contains(pattern.as_str()))
            .map_or_else(
                || booted(&call),
                |(_, reply)| reply.clone().into_bytes(),
            );
        self.record(call);
        Ok(reply)
    }

    fn shell(
        &self,
        vm: &str,
        user: &str,
        _workdir: &str,
        args: &[String],
        _tty: bool,
    ) -> Command {
        let target = target(vm, Some(user));
        self.record(format!("shell {target} {}", args.join(" ")));
        process::command(if self.shell_succeeds.get() {
            "true"
        } else {
            "false"
        })
    }

    fn image_present(&self, image: &str) -> bool {
        self.images.borrow().iter().any(|loaded| loaded == image)
    }

    fn load_image(&self, tag: &str, _archive: Stdio) -> Result<()> {
        self.record(format!("load {tag}"));
        self.images.borrow_mut().push(tag.to_string());
        Ok(())
    }

    fn host_address(&self) -> &'static str {
        "host.fake"
    }
}
