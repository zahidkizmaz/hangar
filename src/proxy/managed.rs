//! Runs a managed proxy in its own VM: created, started, recreated or
//! kept, whatever the kind.

use std::fs;
use std::path::{Component, PathBuf};
use std::time::Duration;

use super::{Health, Managed, ManagedVm, Proxy, proxy_vm};
use crate::bay::{Probe, remove_if_present, wait};
use crate::error::{Context, Error, Result, bail};
use crate::hangar::Hangar;
use crate::lifecycle::{self, Outcome, Plan, step};
use crate::mounts;
use crate::sandbox::{BoxState, Mount, MountMode, VmSpec};
use crate::state::write_private;
use crate::vm_record::VmRecord;

pub(crate) const PROXY_WAIT: Duration = Duration::from_secs(60);
pub(crate) const PROXY_PAUSE: Duration = Duration::from_millis(500);

type Mounts = Vec<(PathBuf, String, MountMode)>;

pub(crate) fn up(hangar: &Hangar, name: &str, proxy: &dyn Proxy) -> Outcome {
    up_within(hangar, name, proxy, PROXY_WAIT, PROXY_PAUSE)
}

fn up_within(
    hangar: &Hangar,
    name: &str,
    proxy: &dyn Proxy,
    timeout: Duration,
    pause: Duration,
) -> Outcome {
    let result = match proxy.managed() {
        Some(managed) => {
            reconcile(hangar, name, proxy, managed, timeout, pause)
        }
        None => check_external(name, proxy),
    };
    result.unwrap_or_else(Outcome::Failed)
}

fn check_external(name: &str, proxy: &dyn Proxy) -> Result<Outcome> {
    match proxy.health() {
        Health::Up => Ok(Outcome::Unchanged),
        Health::Down(why) => {
            bail!("proxy {name} unreachable: {why}")
        }
    }
}

pub(crate) fn down(
    hangar: &Hangar,
    name: &str,
    proxy: &dyn Proxy,
) -> Result<Outcome> {
    let sandbox = hangar.sandbox.as_ref();
    let vm = proxy_vm(name);
    if proxy.managed().is_none() || sandbox.state(&vm)? != BoxState::Running {
        return Ok(Outcome::Unchanged);
    }
    sandbox.stop(&vm)?;
    Ok(Outcome::Stopped)
}

fn reconcile(
    hangar: &Hangar,
    name: &str,
    proxy: &dyn Proxy,
    managed: &dyn Managed,
    timeout: Duration,
    pause: Duration,
) -> Result<Outcome> {
    let owner = format!("proxy {name}");
    let sandbox = hangar.sandbox.as_ref();
    let wanted_vm = ManagedVm {
        mounts: resolve_mounts(hangar, name, &managed.vm().mounts)?,
        ..managed.vm()
    };
    let fingerprint = step(&owner, "token", || managed.fingerprint())?;
    let wanted = wanted_record(&wanted_vm, &fingerprint);
    let created = step(&owner, "vm", || {
        ensure_vm(hangar, name, &wanted_vm, &wanted)
    })?;
    if matches!(proxy.health(), Health::Up) {
        return Ok(created.unwrap_or(Outcome::Unchanged));
    }
    step(&owner, "boot", || wait_exec(hangar, name, timeout, pause))?;
    step(&owner, "start", || managed.start(sandbox, &proxy_vm(name)))?;
    let answer = wait(timeout, pause, || match proxy.health() {
        Health::Up => Probe::Done,
        Health::Down(why) => Probe::Pending(why),
    });
    if answer != Probe::Done {
        return Err(Error::new(format!(
            "proxy {name} didn't come up; see 'hangar logs {name}'"
        )));
    }
    Ok(created.unwrap_or_else(started))
}

fn started() -> Outcome {
    Outcome::Updated(vec!["started".into()])
}

/// Mount sources live under the proxy's own folder, so a kind can't
/// mount anything else. Each is checked before it is created 0700 (a
/// symlink planted in the folder must not make hangar create outside it)
/// and again once resolved.
fn resolve_mounts(
    hangar: &Hangar,
    name: &str,
    mounts: &Mounts,
) -> Result<Mounts> {
    let root = hangar.state.proxies().join(name);
    mounts::create(&root).context(root.display())?;
    let root = fs::canonicalize(&root).context(root.display())?;
    mounts
        .iter()
        .map(|(host, guest, mode)| {
            let outside = || {
                Error::new(format!(
                    "proxy {name}: mount {} is outside its data dir",
                    host.display()
                ))
            };
            let lexical = hangar.state.proxies().join(name);
            let climbs = host.components().any(|c| c == Component::ParentDir);
            if climbs || !host.starts_with(&lexical) {
                return Err(outside());
            }
            let existing = host.ancestors().find(|path| path.exists());
            let existing = existing.unwrap_or(host);
            let existing =
                fs::canonicalize(existing).context(existing.display())?;
            if !existing.starts_with(&root) {
                return Err(outside());
            }
            mounts::create(host).context(host.display())?;
            let host = fs::canonicalize(host).context(host.display())?;
            if !host.starts_with(&root) {
                return Err(outside());
            }
            Ok((host, guest.clone(), *mode))
        })
        .collect()
}

/// Sorted, so the same proxy always records the same VM. A changed
/// entrypoint rides in the fingerprint: it recreates like any other.
pub(crate) fn wanted_record(vm: &ManagedVm, fingerprint: &str) -> VmRecord {
    let (host, guest) = vm.publish;
    let mut mounts: Vec<String> = vm
        .mounts
        .iter()
        .map(|(host, guest, mode)| {
            let writable = *mode == MountMode::ReadWrite;
            mounts::line(guest, &host.display().to_string(), writable)
        })
        .collect();
    mounts.sort();
    let fingerprint = match &vm.entrypoint {
        Some(entrypoint) => format!("{fingerprint} entrypoint={entrypoint}"),
        None => fingerprint.to_string(),
    };
    VmRecord {
        image: Some(vm.image.clone()),
        egress: Some(vm.egress().record()),
        resources: Some(lifecycle::resources(vm.cpus, &vm.memory, None)),
        fingerprint: Some(fingerprint),
        ports: vec![("proxy".into(), host, guest)],
        mounts,
    }
}

/// The VM's outcome when it was created, recreated or started; `None`
/// when the running one is used.
fn ensure_vm(
    hangar: &Hangar,
    name: &str,
    vm: &ManagedVm,
    wanted: &VmRecord,
) -> Result<Option<Outcome>> {
    let sandbox = hangar.sandbox.as_ref();
    let vm_name = proxy_vm(name);
    let record = hangar.state.proxy(name).vm();
    let recorded = VmRecord::load(&record);
    match lifecycle::plan(sandbox.state(&vm_name)?, recorded.as_ref(), wanted) {
        Plan::Create => {
            create(hangar, name, vm, wanted)?;
            Ok(Some(Outcome::Created))
        }
        Plan::Recreate(reasons) => {
            remove_if_present(&record)?;
            // Trap: remove is a hard kill that loses unsynced writes.
            if sandbox.state(&vm_name)? == BoxState::Running {
                sandbox.stop(&vm_name)?;
            }
            sandbox.remove(&vm_name)?;
            if reasons.contains(&"fingerprint") {
                empty_mounts(name, vm)?;
            }
            create(hangar, name, vm, wanted)?;
            let reasons = reasons.into_iter().map(String::from).collect();
            Ok(Some(Outcome::Recreated(reasons)))
        }
        Plan::Start => {
            sandbox.start(&vm_name)?;
            Ok(Some(started()))
        }
        Plan::Keep => Ok(None),
    }
}

/// What a proxy saved belongs to the token it enrolled with: a new token
/// starts from empty mounts. They are the resolved ones, inside the
/// proxy's own folder.
fn empty_mounts(name: &str, vm: &ManagedVm) -> Result<()> {
    log::info!("proxy {name}: new fingerprint, emptying its data");
    for (host, _, _) in &vm.mounts {
        fs::remove_dir_all(host).context(host.display())?;
        mounts::create(host).context(host.display())?;
    }
    Ok(())
}

/// A VM that fails to create is left without a record, so the next `up`
/// recreates it.
fn create(
    hangar: &Hangar,
    name: &str,
    vm: &ManagedVm,
    wanted: &VmRecord,
) -> Result<()> {
    let mounts: Vec<Mount> = vm
        .mounts
        .iter()
        .map(|(host, guest, mode)| Mount {
            host,
            guest,
            mode: *mode,
            owner: None,
            file: false,
        })
        .collect();
    hangar.sandbox.create(&VmSpec {
        name: &proxy_vm(name),
        image: &vm.image,
        cpus: vm.cpus,
        memory: &vm.memory,
        disk: None,
        entrypoint: vm.entrypoint.as_deref(),
        volumes: &[],
        init: None,
        egress: vm.egress(),
        publish: &[vm.publish],
        mounts: &mounts,
        env: &[],
    })?;
    write_private(&hangar.state.proxy(name).vm(), wanted.render().as_bytes())
}

/// A fresh VM isn't exec-ready for a few seconds.
fn wait_exec(
    hangar: &Hangar,
    name: &str,
    timeout: Duration,
    pause: Duration,
) -> Result<()> {
    let vm = proxy_vm(name);
    let ready = || match hangar.sandbox.exec(
        &vm,
        None,
        &["/bin/sh", "-c", "true"],
        None,
    ) {
        Ok(_) => Probe::Done,
        Err(error) => Probe::Pending(error.to_string()),
    };
    match wait(timeout, pause, ready) {
        Probe::Pending(last) | Probe::Failed(last) => {
            bail!("{vm} didn't answer exec: {last}")
        }
        Probe::Done => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::{down, up_within};
    use crate::lifecycle::Outcome;
    use crate::proxy::fake::{FakeManaged, FakeProxy};
    use crate::proxy::{Health, Login, Managed, Proxy, proxy_vm};
    use crate::sandbox::fake::FakeSandbox;
    use crate::sandbox::{BoxState, Egress, MountMode};
    use crate::testing::{DIRECT, hangar_with, scratch_dir};
    use std::cell::Cell;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;
    use std::rc::Rc;
    use std::time::Duration;

    fn managed(port: u16) -> FakeProxy {
        FakeProxy {
            managed: Some(FakeManaged::new(port)),
            ..FakeProxy::at(port)
        }
    }

    fn starts(proxy: &FakeProxy) -> u32 {
        proxy.managed.as_ref().unwrap().starts.get()
    }

    fn run(
        state: &Path,
        sandbox: &Rc<FakeSandbox>,
        proxy: &FakeProxy,
    ) -> String {
        let hangar = hangar_with(DIRECT, state, sandbox.clone());
        let outcome =
            up_within(&hangar, "p", proxy, Duration::ZERO, Duration::ZERO);
        format!("{outcome:?}")
    }

    fn with_mount(host: &Path) -> FakeProxy {
        let mut proxy = managed(8080);
        let vm = &mut proxy.managed.as_mut().unwrap().vm;
        vm.mounts = vec![(host.into(), "/data".into(), MountMode::ReadWrite)];
        proxy
    }

    #[test]
    fn a_managed_proxy_that_needs_a_host_port_gets_it_besides_the_internet() {
        let state = scratch_dir("managed-host-port");
        let sandbox = Rc::new(FakeSandbox::default());
        let mut proxy = managed(8080);
        proxy.managed.as_mut().unwrap().vm.host_port = Some(9000);
        assert_eq!(run(&state, &sandbox, &proxy), "Created");
        assert_eq!(
            sandbox.created.borrow()[0].egress,
            Egress::OpenAndHostPort(9000)
        );
        proxy.managed.as_mut().unwrap().vm.host_port = None;
        assert_eq!(run(&state, &sandbox, &proxy), r#"Recreated(["proxy"])"#);
        assert_eq!(sandbox.created.borrow()[1].egress, Egress::Open);
    }

    #[test]
    fn managed_proxy_vm_has_open_egress_and_loopback_publish() {
        let state = scratch_dir("managed-create");
        let sandbox = Rc::new(FakeSandbox::default());
        let proxy = managed(8080);
        assert_eq!(run(&state, &sandbox, &proxy), "Created");
        let created = sandbox.created.borrow();
        assert_eq!(created[0].name, proxy_vm("p"));
        assert_eq!(created[0].egress, Egress::Open);
        assert_eq!(created[0].publish, [(8080, 8080)]);
        assert_eq!(created[0].entrypoint, None);
        assert_eq!(created[0].volumes, Vec::<String>::new());
    }

    #[test]
    fn the_entrypoint_reaches_the_vm_and_a_change_recreates() {
        let state = scratch_dir("managed-entrypoint");
        let sandbox = Rc::new(FakeSandbox::default());
        let mut proxy = managed(8080);
        proxy.managed.as_mut().unwrap().vm.entrypoint = Some("/run".into());
        assert_eq!(run(&state, &sandbox, &proxy), "Created");
        assert_eq!(
            sandbox.created.borrow()[0].entrypoint.as_deref(),
            Some("/run")
        );
        proxy.managed.as_mut().unwrap().vm.entrypoint = Some("/go".into());
        assert_eq!(
            run(&state, &sandbox, &proxy),
            r#"Recreated(["fingerprint"])"#
        );
    }

    #[test]
    fn mounts_outside_the_proxy_dir_are_refused() {
        let state = scratch_dir("managed-outside");
        let sandbox = Rc::new(FakeSandbox::default());
        let proxy = with_mount(Path::new("/etc"));
        assert_eq!(
            run(&state, &sandbox, &proxy),
            r#"Failed(Error { message: "proxy p: mount /etc is outside its data dir", hint: None })"#
        );
        assert!(sandbox.created.borrow().is_empty());
    }

    #[test]
    fn dotdot_and_symlinks_cannot_escape_the_proxy_dir() {
        let state = scratch_dir("managed-escape");
        let outside = scratch_dir("managed-escape-target");
        let root = state.join("proxies/p");
        fs::create_dir_all(&root).unwrap();
        symlink(&outside, root.join("link")).unwrap();
        for host in [root.join("../q"), root.join("link")] {
            let sandbox = Rc::new(FakeSandbox::default());
            let result = run(&state, &sandbox, &with_mount(&host));
            assert!(result.contains("is outside its data dir"), "{result}");
            assert!(sandbox.created.borrow().is_empty());
        }
    }

    #[test]
    fn nothing_is_created_through_a_symlink_out_of_the_proxy_dir() {
        let state = scratch_dir("managed-escape-create");
        let outside = scratch_dir("managed-escape-create-target");
        let root = state.join("proxies/p");
        fs::create_dir_all(&root).unwrap();
        symlink(&outside, root.join("link")).unwrap();
        let sandbox = Rc::new(FakeSandbox::default());
        let host = root.join("link/new/deeper");
        let result = run(&state, &sandbox, &with_mount(&host));
        assert!(result.contains("is outside its data dir"), "{result}");
        assert!(!outside.join("new").exists());
    }

    #[test]
    fn mount_sources_are_created_private_and_recorded_sorted() {
        let state = scratch_dir("managed-mounts");
        let data = state.join("proxies/p/data");
        let sandbox = Rc::new(FakeSandbox::default());
        let mut proxy = with_mount(&data);
        let vm = &mut proxy.managed.as_mut().unwrap().vm;
        vm.mounts.push((
            state.join("proxies/p/a"),
            "/a".into(),
            MountMode::ReadOnly,
        ));
        assert_eq!(run(&state, &sandbox, &proxy), "Created");
        let mode = |path: &Path| {
            fs::metadata(path).unwrap().permissions().mode() & 0o777
        };
        assert_eq!(mode(&data), 0o700);
        let record = fs::read_to_string(state.join("proxies/p/vm")).unwrap();
        let mounts: Vec<&str> = record
            .lines()
            .filter_map(|line| line.strip_prefix("mount\t"))
            .collect();
        let mut sorted = mounts.clone();
        sorted.sort_unstable();
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts, sorted);
        assert_eq!(run(&state, &sandbox, &proxy), "Unchanged");
    }

    #[test]
    fn start_is_called_only_when_unhealthy() {
        let state = scratch_dir("managed-start");
        let sandbox = Rc::new(FakeSandbox::default());
        let healthy = managed(8080);
        assert_eq!(run(&state, &sandbox, &healthy), "Created");
        assert_eq!(starts(&healthy), 0);
        let mut down = managed(8080);
        down.up = false;
        let result = run(&state, &sandbox, &down);
        assert_eq!(starts(&down), 1);
        assert!(result.starts_with("Failed"), "{result}");
    }

    #[test]
    fn a_stopped_vm_started_is_reported_started_even_when_already_up() {
        let state = scratch_dir("managed-stopped");
        let sandbox = Rc::new(FakeSandbox::default());
        let proxy = managed(8080);
        assert_eq!(run(&state, &sandbox, &proxy), "Created");
        sandbox.set(&proxy_vm("p"), BoxState::Stopped);
        assert_eq!(run(&state, &sandbox, &proxy), r#"Updated(["started"])"#);
        assert_eq!(starts(&proxy), 0);
        assert_eq!(sandbox.states.borrow()[&proxy_vm("p")], BoxState::Running);
    }

    #[test]
    fn fingerprint_change_recreates_proxy() {
        let state = scratch_dir("managed-fingerprint");
        let data = state.join("proxies/p/data");
        let sandbox = Rc::new(FakeSandbox::default());
        let proxy = with_mount(&data);
        assert_eq!(run(&state, &sandbox, &proxy), "Created");
        fs::write(data.join("old"), "x").unwrap();
        fs::create_dir(data.join("dir")).unwrap();
        *proxy.managed.as_ref().unwrap().fingerprint.borrow_mut() =
            "other".into();
        sandbox.calls.borrow_mut().clear();
        assert_eq!(
            run(&state, &sandbox, &proxy),
            r#"Recreated(["fingerprint"])"#
        );
        let changes = sandbox.changes();
        let at = |call: String| {
            changes.iter().position(|change| *change == call).unwrap()
        };
        let vm = proxy_vm("p");
        assert!(at(format!("stop {vm}")) < at(format!("remove {vm}")));
        assert!(at(format!("remove {vm}")) < at(format!("create {vm}")));
        assert!(fs::read_dir(&data).unwrap().next().is_none());
        let mode = fs::metadata(&data).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn other_changes_keep_the_proxy_data() {
        let state = scratch_dir("managed-keep-data");
        let data = state.join("proxies/p/data");
        let sandbox = Rc::new(FakeSandbox::default());
        let mut proxy = with_mount(&data);
        assert_eq!(run(&state, &sandbox, &proxy), "Created");
        fs::write(data.join("keep"), "x").unwrap();
        proxy.managed.as_mut().unwrap().vm.image = "fake:2".into();
        assert_eq!(run(&state, &sandbox, &proxy), r#"Recreated(["image"])"#);
        assert!(data.join("keep").exists());
    }

    #[test]
    fn a_fingerprint_that_cannot_be_read_fails_before_any_vm_change() {
        let state = scratch_dir("managed-no-fingerprint");
        let sandbox = Rc::new(FakeSandbox::default());
        let hangar = hangar_with(DIRECT, &state, sandbox.clone());
        let base = managed(8080);
        let proxy = NoToken(&base);
        let outcome =
            up_within(&hangar, "p", &proxy, Duration::ZERO, Duration::ZERO);
        assert!(format!("{outcome:?}").contains("no token"), "{outcome:?}");
        assert!(sandbox.created.borrow().is_empty());
    }

    struct NoToken<'a>(&'a FakeProxy);

    impl Proxy for NoToken<'_> {
        fn kind(&self) -> &'static str {
            "fake"
        }
        fn egress(&self) -> Egress {
            self.0.egress
        }
        fn ca(&self) -> crate::error::Result<Option<Vec<u8>>> {
            Ok(None)
        }
        fn login(&self, _: &str) -> crate::error::Result<Option<Login>> {
            Ok(None)
        }
        fn health(&self) -> Health {
            Health::Down("down".into())
        }
        fn managed(&self) -> Option<&dyn Managed> {
            Some(self)
        }
    }

    impl Managed for NoToken<'_> {
        fn vm(&self) -> crate::proxy::ManagedVm {
            self.0.managed.as_ref().unwrap().vm.clone()
        }
        fn fingerprint(&self) -> crate::error::Result<String> {
            crate::error::bail!("no token")
        }
        fn start(
            &self,
            _: &dyn crate::sandbox::Sandbox,
            _: &str,
        ) -> crate::error::Result<()> {
            Ok(())
        }
        fn log_file(&self) -> std::path::PathBuf {
            "p.log".into()
        }
    }

    #[test]
    fn a_failed_recreate_leaves_no_record() {
        let state = scratch_dir("managed-failed-create");
        let sandbox = Rc::new(FakeSandbox::default());
        let mut proxy = managed(8080);
        assert_eq!(run(&state, &sandbox, &proxy), "Created");
        proxy.managed.as_mut().unwrap().vm.image = "fake:2".into();
        sandbox.fail_create.set(true);
        assert!(run(&state, &sandbox, &proxy).starts_with("Failed"));
        assert!(!state.join("proxies/p/vm").exists());
    }

    #[test]
    fn external_proxy_unreachable_fails_with_reason() {
        let state = scratch_dir("managed-external");
        let sandbox = Rc::new(FakeSandbox::default());
        let mut proxy = FakeProxy::at(3128);
        assert_eq!(run(&state, &sandbox, &proxy), "Unchanged");
        proxy.up = false;
        assert_eq!(
            run(&state, &sandbox, &proxy),
            r#"Failed(Error { message: "proxy p unreachable: down", hint: None })"#
        );
        assert!(sandbox.created.borrow().is_empty());
    }

    #[test]
    fn never_up_times_out_with_logs_hint() {
        let state = scratch_dir("managed-never-up");
        let sandbox = Rc::new(FakeSandbox::default());
        let mut proxy = managed(8080);
        proxy.up = false;
        assert_eq!(
            run(&state, &sandbox, &proxy),
            r#"Failed(Error { message: "proxy p didn't come up; see 'hangar logs p'", hint: None })"#
        );
    }

    #[test]
    fn a_vm_that_never_answers_exec_fails_before_the_proxy_starts() {
        let state = scratch_dir("managed-no-exec");
        let sandbox = Rc::new(FakeSandbox::default());
        sandbox.reply_once("/bin/sh -c true", None);
        let mut proxy = managed(8080);
        proxy.up = false;
        let result = run(&state, &sandbox, &proxy);
        assert!(result.contains("boot"), "{result}");
        assert_eq!(starts(&proxy), 0);
    }

    struct ComesUp<'a>(&'a FakeProxy, Cell<bool>);

    impl Proxy for ComesUp<'_> {
        fn kind(&self) -> &'static str {
            "fake"
        }
        fn egress(&self) -> Egress {
            self.0.egress
        }
        fn ca(&self) -> crate::error::Result<Option<Vec<u8>>> {
            Ok(None)
        }
        fn login(&self, _: &str) -> crate::error::Result<Option<Login>> {
            Ok(None)
        }
        fn health(&self) -> Health {
            if self.1.get() {
                Health::Up
            } else {
                Health::Down("starting".into())
            }
        }
        fn managed(&self) -> Option<&dyn Managed> {
            Some(self)
        }
    }

    impl Managed for ComesUp<'_> {
        fn vm(&self) -> crate::proxy::ManagedVm {
            self.0.managed.as_ref().unwrap().vm.clone()
        }
        fn fingerprint(&self) -> crate::error::Result<String> {
            Ok("fake".into())
        }
        fn start(
            &self,
            _: &dyn crate::sandbox::Sandbox,
            _: &str,
        ) -> crate::error::Result<()> {
            self.1.set(true);
            Ok(())
        }
        fn log_file(&self) -> std::path::PathBuf {
            "p.log".into()
        }
    }

    #[test]
    fn starting_the_process_of_a_kept_vm_is_reported_started() {
        let state = scratch_dir("managed-started");
        let sandbox = Rc::new(FakeSandbox::default());
        let base = managed(8080);
        assert_eq!(run(&state, &sandbox, &base), "Created");
        let hangar = hangar_with(DIRECT, &state, sandbox.clone());
        let proxy = ComesUp(&base, Cell::new(false));
        let outcome =
            up_within(&hangar, "p", &proxy, Duration::ZERO, Duration::ZERO);
        assert_eq!(format!("{outcome:?}"), r#"Updated(["started"])"#);
    }

    #[test]
    fn down_stops_only_a_running_managed_vm() {
        let state = scratch_dir("managed-down");
        let sandbox = Rc::new(FakeSandbox::default());
        let hangar = hangar_with(DIRECT, &state, sandbox.clone());
        let proxy = managed(8080);
        assert!(matches!(
            down(&hangar, "p", &proxy).unwrap(),
            Outcome::Unchanged
        ));
        assert_eq!(run(&state, &sandbox, &proxy), "Created");
        assert!(matches!(
            down(&hangar, "p", &FakeProxy::at(1)).unwrap(),
            Outcome::Unchanged
        ));
        assert!(matches!(
            down(&hangar, "p", &proxy).unwrap(),
            Outcome::Stopped
        ));
    }
}
