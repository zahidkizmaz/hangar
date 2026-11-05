//! Test doubles: a fake `msb` (shell script, state in files).

#![expect(dead_code, reason = "rewrite in progress; removed in Task 16")]

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};

const FAKE_MSB: &str = include_str!("msb.sh");

/// One isolated machine: HOME, the hangar state, the fakes' files and bin.
pub struct Machine {
    pub home: PathBuf,
    pub state: PathBuf,
    pub fake: PathBuf,
}

impl Machine {
    pub fn new(name: &str) -> Self {
        let home = std::env::temp_dir()
            .join(format!("hangar-cli-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let machine = Self {
            state: home.join("state"),
            fake: home.join("fake"),
            home,
        };
        fs::create_dir_all(machine.home.join("bin")).unwrap();
        fs::create_dir_all(&machine.fake).unwrap();
        machine.script("msb", FAKE_MSB);
        machine
    }

    /// An executable in the machine's bin, first on PATH.
    pub fn script(&self, name: &str, body: &str) {
        let path = self.home.join("bin").join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Makes the fake msb fail at `what` with `why` (see msb.sh).
    pub fn fail(&self, what: &str, why: &str) {
        fs::write(self.fake.join(format!("fail-{what}")), why).unwrap();
    }

    pub fn config(&self, json: &str) -> PathBuf {
        let path = self.home.join("hangar.json");
        fs::write(&path, json).unwrap();
        path
    }

    pub fn msb_log(&self) -> String {
        fs::read_to_string(self.fake.join("msb.log")).unwrap_or_default()
    }

    pub fn fake_file(&self, name: &str) -> String {
        fs::read_to_string(self.fake.join(name)).unwrap_or_default()
    }

    /// Runs hangar with a clean environment: only HOME, PATH (machine bin
    /// first), the fakes' and coverage variables, plus `env`.
    pub fn hangar(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        self.command(args, env).output().unwrap()
    }

    /// Like `hangar`, with `input` on stdin.
    pub fn hangar_with_stdin(
        &self,
        args: &[&str],
        env: &[(&str, &str)],
        input: &str,
    ) -> Output {
        let mut child = self
            .command(args, env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /// Like `hangar`, running in the background with stderr piped.
    pub fn spawn(&self, args: &[&str], env: &[(&str, &str)]) -> Child {
        self.command(args, env)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let path = tools_path(self.home.join("bin"));
        let mut command = Command::new(env!("CARGO_BIN_EXE_hangar"));
        command
            .args(args)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", path)
            .env("HANGAR_FAKE", &self.fake)
            .envs(env.iter().copied());
        // Lets `cargo llvm-cov` count the binary's runs.
        if let Ok(profile) = std::env::var("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        command
    }
}

/// The caller's PATH (the fake scripts need coreutils, which the Nix build
/// sandbox has only there) minus any directory holding a real `msb`, so a
/// test can never drive real VMs.
fn tools_path(fakes: PathBuf) -> OsString {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tools =
        std::env::split_paths(&path).filter(|dir| !dir.join("msb").exists());
    std::env::join_paths(std::iter::once(fakes).chain(tools)).unwrap()
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
