//! An in-process sandbox for orchestration tests: VM states in memory,
//! every call recorded.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, Stdio};

use super::{BoxState, Egress, Sandbox, VmSpec};
use crate::error::Result;
use crate::process;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Created {
    pub(crate) name: String,
    pub(crate) init: Option<String>,
    pub(crate) entrypoint: Option<String>,
    pub(crate) volumes: Vec<String>,
    pub(crate) egress: Egress,
    pub(crate) publish: Vec<(u16, u16)>,
    pub(crate) mounts: Vec<String>,
}

#[derive(Default)]
pub(crate) struct FakeSandbox {
    pub(crate) states: RefCell<BTreeMap<String, BoxState>>,
    pub(crate) calls: RefCell<Vec<String>>,
    pub(crate) stdins: RefCell<Vec<Vec<u8>>>,
    pub(crate) created: RefCell<Vec<Created>>,
    pub(crate) images: RefCell<Vec<String>>,
    pub(crate) volumes: RefCell<BTreeSet<String>>,
    pub(crate) replies: RefCell<Vec<(String, String)>>,
    pub(crate) replies_once: RefCell<Vec<(String, Option<String>)>>,
    pub(crate) shell_succeeds: Cell<bool>,
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

    pub(crate) fn reply(&self, pattern: &str, reply: &str) {
        self.replies
            .borrow_mut()
            .push((pattern.to_string(), reply.to_string()));
    }

    pub(crate) fn reply_once(&self, pattern: &str, reply: Option<&str>) {
        self.replies_once
            .borrow_mut()
            .push((pattern.to_string(), reply.map(String::from)));
    }

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
            entrypoint: spec.entrypoint.map(String::from),
            volumes: spec
                .volumes
                .iter()
                .map(|v| format!("{}:{}:{}", v.name, v.guest, v.size))
                .collect(),
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
                    let file = if mount.file { ":file" } else { "" };
                    format!(
                        "{}:{}:{:?}{owner}{file}",
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

    fn ensure_volume(&self, name: &str, size: &str) -> Result<bool> {
        let created = self.volumes.borrow_mut().insert(name.to_string());
        if created {
            self.record(format!("volume create {name} {size}"));
        }
        Ok(created)
    }

    fn remove_volume(&self, name: &str) -> Result<()> {
        self.record(format!("volume rm {name}"));
        self.volumes.borrow_mut().remove(name);
        Ok(())
    }

    fn copy_out(&self, vm: &str, guest: &str, host: &Path) -> Result<()> {
        self.record(format!("copy {vm}:{guest} {}", host.display()));
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

#[cfg(test)]
mod tests {
    use super::FakeSandbox;
    use crate::sandbox::Sandbox;
    use std::path::Path;

    #[test]
    fn records_volumes_and_copy_out() {
        let fake = FakeSandbox::default();
        assert!(fake.ensure_volume("v", "40G").unwrap());
        assert!(!fake.ensure_volume("v", "40G").unwrap());
        fake.copy_out("vm", "/a", Path::new("/tmp/a")).unwrap();
        fake.remove_volume("v").unwrap();
        assert_eq!(
            fake.changes(),
            ["volume create v 40G", "copy vm:/a /tmp/a", "volume rm v"]
        );
        assert!(fake.ensure_volume("v", "40G").unwrap());
    }
}
