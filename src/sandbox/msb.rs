//! The msb (microsandbox) backend: the only module that knows its CLI.

use std::path::Path;
use std::process::{Command, Stdio};

use super::{BoxState, Egress, Mount, MountMode, Sandbox, VmSpec, Volume};
use crate::error::{Result, bail};
use crate::{json, process};

const HOST_ALIAS: &str = "host.microsandbox.internal";

pub(crate) struct Msb;

impl Sandbox for Msb {
    /// msb's status (`Running`, `Stopped`, …), or `missing`.
    fn describe(&self, vm: &str) -> Result<(BoxState, String)> {
        Ok(match status(vm)? {
            None => (BoxState::Missing, "missing".into()),
            Some(status) if status == "Running" => (BoxState::Running, status),
            Some(status) => (BoxState::Stopped, status),
        })
    }

    fn create(&self, spec: &VmSpec) -> Result<()> {
        let args = create_args(spec);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        run(&args).map(drop)
    }

    fn start(&self, vm: &str) -> Result<()> {
        run(&["start", "-q", vm]).map(drop)
    }

    fn stop(&self, vm: &str) -> Result<()> {
        run(&["stop", "-q", vm]).map(drop)
    }

    fn remove(&self, vm: &str) -> Result<()> {
        run(&["rm", "-f", "-q", vm]).map(drop)
    }

    fn ensure_volume(&self, name: &str, size: &str) -> Result<bool> {
        let mut inspect = process::command("msb");
        inspect.args(["volume", "inspect", name]);
        let output = process::output(&mut inspect, None)?;
        if output.status.success() {
            return Ok(false);
        }
        let stderr = process::stderr(&output);
        if !is_volume_not_found(&stderr) {
            bail!("msb volume inspect {name}: {stderr}");
        }
        run(&volume_create_args(name, size)).map(|_| true)
    }

    fn remove_volume(&self, name: &str) -> Result<()> {
        let mut remove = process::command("msb");
        remove.args(["volume", "rm", name]);
        let output = process::output(&mut remove, None)?;
        let stderr = process::stderr(&output);
        if output.status.success() || is_volume_not_found(&stderr) {
            return Ok(());
        }
        bail!("msb volume rm {name}: {stderr}")
    }

    fn copy_out(&self, vm: &str, guest: &str, host: &Path) -> Result<()> {
        let args = copy_out_args(vm, guest, host);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        call(&args, None, &format!("copy {vm}:{guest}")).map(drop)
    }

    fn exec(
        &self,
        vm: &str,
        user: Option<&str>,
        command: &[&str],
        stdin: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        let mut args = exec_args(vm, user, "--no-tty");
        args.extend_from_slice(command);
        let what = command.get(..2).unwrap_or(command).join(" ");
        call(&args, stdin, &format!("exec {vm}: {what}"))
    }

    fn shell(
        &self,
        vm: &str,
        user: &str,
        workdir: &str,
        args: &[String],
        tty: bool,
    ) -> Command {
        let mut shell = process::command("msb");
        shell.args(shell_args(vm, user, workdir, args, tty));
        shell
    }

    fn image_present(&self, image: &str) -> bool {
        run(&["image", "inspect", image]).is_ok()
    }

    fn load_image(&self, tag: &str, archive: Stdio) -> Result<()> {
        let mut load = process::command("msb");
        load.args(["load", "-q", "--tag", tag]).stdin(archive);
        if !process::status(&mut load)?.success() {
            bail!("msb load {tag} failed");
        }
        Ok(())
    }

    fn host_address(&self) -> &str {
        HOST_ALIAS
    }
}

/// `Ok(None)` when the sandbox doesn't exist; an error when msb can't run.
fn status(vm: &str) -> Result<Option<String>> {
    let mut inspect = process::command("msb");
    inspect.args(["inspect", vm, "--format", "json"]);
    let output = process::output(&mut inspect, None)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_not_found(&stderr) {
            return Ok(None);
        }
        bail!("msb inspect {vm}: {}", stderr.trim());
    }
    let info: Inspect =
        json::from_str(&String::from_utf8_lossy(&output.stdout))?;
    Ok(Some(info.status))
}

/// Only "doesn't exist" means missing; any other failure is an error, so
/// `destroy` never mistakes a broken msb for an already-removed VM.
fn is_not_found(stderr: &str) -> bool {
    stderr.contains("sandbox not found")
}

fn is_volume_not_found(stderr: &str) -> bool {
    stderr.contains("volume not found")
}

fn volume_create_args<'a>(name: &'a str, size: &'a str) -> Vec<&'a str> {
    vec![
        "volume", "create", "--kind", "disk", "--size", size, "-q", name,
    ]
}

fn copy_out_args(vm: &str, guest: &str, host: &Path) -> Vec<String> {
    vec![
        "copy".into(),
        "-q".into(),
        format!("{vm}:{guest}"),
        host.display().to_string(),
    ]
}

#[derive(miniserde::Deserialize)]
struct Inspect {
    status: String,
}

fn exec_args<'a>(
    vm: &'a str,
    user: Option<&'a str>,
    tty: &'a str,
) -> Vec<&'a str> {
    let mut args = vec!["exec", tty];
    if let Some(user) = user {
        args.extend(["--user", user]);
    }
    args.extend([vm, "--"]);
    args
}

/// The arguments stay separate words: `"$@"` keeps their quoting intact.
fn shell_args(
    vm: &str,
    user: &str,
    workdir: &str,
    args: &[String],
    tty: bool,
) -> Vec<String> {
    let tty = if tty { "-t" } else { "--no-tty" };
    let mut exec = Vec::from(
        [
            "exec",
            tty,
            "--user",
            user,
            "--workdir",
            workdir,
            vm,
            "--",
            "sh",
        ]
        .map(String::from),
    );
    if args.is_empty() {
        exec.push("-l".into());
    } else {
        exec.extend(["-lc", r#"exec "$@""#, "sh"].map(String::from));
        exec.extend_from_slice(args);
    }
    exec
}

fn create_args(spec: &VmSpec) -> Vec<String> {
    let mut args = Vec::from(
        [
            "create",
            spec.image,
            "--name",
            spec.name,
            "-q",
            "--replace",
            "--cpus",
        ]
        .map(String::from),
    );
    args.push(spec.cpus.to_string());
    args.extend(["--memory".into(), spec.memory.into()]);
    if let Some(entrypoint) = spec.entrypoint {
        args.extend(["--entrypoint".into(), entrypoint.into()]);
    }
    if let Some(init) = spec.init {
        // Trap: else systemd mounts its own tmpfs over /run, hiding the
        // mounts under it.
        args.extend(["--init", init, "--tmpfs", "/run"].map(String::from));
    }
    if let Some(disk) = spec.disk {
        args.extend(["--root-disk".into(), disk.into()]);
    }
    if let Egress::HostPort(port) = spec.egress {
        args.push("--no-net".into());
        args.extend(["--net-rule".into(), format!("allow@host:tcp:{port}")]);
        // Trap: forwarded connections don't count as `host`, so ingress to
        // published ports must be `any`; they still bind to host loopback.
        for (_, guest) in spec.publish {
            args.push("--net-rule".into());
            args.push(format!("allow:ingress@any:tcp:{guest}"));
        }
    }
    for (host, guest) in spec.publish {
        args.extend(["-p".into(), format!("127.0.0.1:{host}:{guest}")]);
    }
    for mount in spec.mounts {
        let mut source = format!("{}:{}", mount.host.display(), mount.guest);
        let options = mount_options(mount);
        if !options.is_empty() {
            source = format!("{source}:{options}");
        }
        let flag = if mount.file {
            "--mount-file"
        } else {
            "--mount-dir"
        };
        args.extend([flag.into(), source]);
    }
    for volume in spec.volumes {
        args.extend(["--mount-named".into(), named_volume(volume)]);
    }
    for var in spec.env {
        args.extend(["-e".into(), (*var).into()]);
    }
    args
}

fn named_volume(volume: &Volume) -> String {
    let Volume { name, guest, size } = volume;
    format!("{name}:{guest}:kind=disk,size={size}")
}

fn mount_options(mount: &Mount) -> String {
    let mut options = Vec::new();
    if mount.mode == MountMode::ReadOnly {
        options.push("ro".to_string());
    }
    if let Some((uid, gid)) = mount.owner {
        options.push(format!("uid={uid},gid={gid}"));
    }
    options.join(",")
}

fn run(args: &[&str]) -> Result<Vec<u8>> {
    call(args, None, args.first().copied().unwrap_or_default())
}

fn call(args: &[&str], stdin: Option<&[u8]>, what: &str) -> Result<Vec<u8>> {
    let mut msb = process::command("msb");
    msb.args(args);
    let output = process::output(&mut msb, stdin)?;
    if !output.status.success() {
        bail!("msb {what} failed: {}", process::stderr(&output));
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::{
        copy_out_args, create_args, exec_args, is_not_found,
        is_volume_not_found, shell_args, volume_create_args,
    };
    use crate::sandbox::{Egress, Mount, MountMode, VmSpec, Volume};
    use std::path::Path;

    #[test]
    fn create_args_host_port_egress() {
        let spec = VmSpec {
            name: "hangar-bay-default",
            image: "img",
            cpus: 4,
            memory: "6G",
            disk: Some("40G"),
            entrypoint: None,
            volumes: &[Volume {
                name: "hangar-bay-work-data",
                guest: "/var/lib/pilot",
                size: "40G",
            }],
            init: Some("/sbin/init"),
            egress: Egress::HostPort(14322),
            publish: &[(3100, 3100)],
            mounts: &[Mount {
                host: Path::new("/state/guest"),
                guest: "/run/hangar",
                mode: MountMode::ReadOnly,
                owner: None,
                file: false,
            }],
            env: &[],
        };
        assert_eq!(
            create_args(&spec),
            [
                "create",
                "img",
                "--name",
                "hangar-bay-default",
                "-q",
                "--replace",
                "--cpus",
                "4",
                "--memory",
                "6G",
                "--init",
                "/sbin/init",
                "--tmpfs",
                "/run",
                "--root-disk",
                "40G",
                "--no-net",
                "--net-rule",
                "allow@host:tcp:14322",
                "--net-rule",
                "allow:ingress@any:tcp:3100",
                "-p",
                "127.0.0.1:3100:3100",
                "--mount-dir",
                "/state/guest:/run/hangar:ro",
                "--mount-named",
                "hangar-bay-work-data:/var/lib/pilot:kind=disk,size=40G",
            ]
        );
    }

    #[test]
    fn create_args_open_egress_has_no_net_flags() {
        let spec = VmSpec {
            name: "hangar-proxy-p",
            image: "proxy",
            cpus: 1,
            memory: "512M",
            disk: None,
            entrypoint: None,
            volumes: &[],
            init: None,
            egress: Egress::Open,
            publish: &[(14321, 14321), (14322, 14322)],
            mounts: &[Mount {
                host: Path::new("/state/p"),
                guest: "/data",
                mode: MountMode::ReadWrite,
                owner: Some((65532, 65532)),
                file: false,
            }],
            env: &["HOME=/data"],
        };
        assert_eq!(
            create_args(&spec),
            [
                "create",
                "proxy",
                "--name",
                "hangar-proxy-p",
                "-q",
                "--replace",
                "--cpus",
                "1",
                "--memory",
                "512M",
                "-p",
                "127.0.0.1:14321:14321",
                "-p",
                "127.0.0.1:14322:14322",
                "--mount-dir",
                "/state/p:/data:uid=65532,gid=65532",
                "-e",
                "HOME=/data",
            ]
        );
    }

    #[test]
    fn create_args_entrypoint_and_file_mount() {
        let spec = VmSpec {
            name: "n",
            image: "img",
            cpus: 1,
            memory: "1G",
            disk: None,
            entrypoint: Some("/bin/proxy"),
            volumes: &[],
            init: None,
            egress: Egress::Open,
            publish: &[],
            mounts: &[Mount {
                host: Path::new("/h/.gitconfig"),
                guest: "/home/pilot/.gitconfig",
                mode: MountMode::ReadOnly,
                owner: Some((1000, 1000)),
                file: true,
            }],
            env: &[],
        };
        let args = create_args(&spec);
        let has = |pair: [&str; 2]| args.windows(2).any(|w| w == pair);
        assert!(has(["--entrypoint", "/bin/proxy"]));
        assert!(has([
            "--mount-file",
            "/h/.gitconfig:/home/pilot/.gitconfig:ro,uid=1000,gid=1000"
        ]));
    }

    #[test]
    fn volume_and_copy_args() {
        assert_eq!(
            volume_create_args("hangar-bay-work-data", "40G"),
            [
                "volume",
                "create",
                "--kind",
                "disk",
                "--size",
                "40G",
                "-q",
                "hangar-bay-work-data"
            ]
        );
        assert_eq!(
            copy_out_args(
                "hangar-bay-work",
                "/home/pilot/x",
                Path::new("/tmp/x")
            ),
            ["copy", "-q", "hangar-bay-work:/home/pilot/x", "/tmp/x"]
        );
    }

    #[test]
    fn shell_keeps_arguments_and_follows_the_terminal() {
        assert_eq!(
            shell_args("hangar-bay-default", "pilot", "/w", &[], true),
            [
                "exec",
                "-t",
                "--user",
                "pilot",
                "--workdir",
                "/w",
                "hangar-bay-default",
                "--",
                "sh",
                "-l"
            ]
        );
        let args = ["git".into(), "commit".into(), "-m".into(), "a b".into()];
        assert_eq!(
            shell_args(
                "hangar-bay-default",
                "pilot",
                "/home/pilot",
                &args,
                false
            ),
            [
                "exec",
                "--no-tty",
                "--user",
                "pilot",
                "--workdir",
                "/home/pilot",
                "hangar-bay-default",
                "--",
                "sh",
                "-lc",
                r#"exec "$@""#,
                "sh",
                "git",
                "commit",
                "-m",
                "a b"
            ]
        );
    }

    #[test]
    fn exec_runs_as_the_given_user_or_the_images_default() {
        assert_eq!(
            exec_args("hangar-bay-default", Some("root"), "--no-tty"),
            [
                "exec",
                "--no-tty",
                "--user",
                "root",
                "hangar-bay-default",
                "--"
            ]
        );
        assert_eq!(
            exec_args("hangar-proxy-p", None, "--no-tty"),
            ["exec", "--no-tty", "hangar-proxy-p", "--"]
        );
    }

    #[test]
    fn only_a_missing_sandbox_counts_as_not_found() {
        assert!(is_not_found("error: sandbox not found: hangar\n"));
        assert!(!is_not_found("error: volume not found: v\n"));
        assert!(is_volume_not_found("error: volume not found: v\n"));
        assert!(!is_volume_not_found("error: sandbox not found: v\n"));
        assert!(!is_not_found("error: permission denied"));
        assert!(!is_not_found(""));
    }
}
