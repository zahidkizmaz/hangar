//! Test doubles: a fake `msb` and a fake keychain tool (shell scripts,
//! state in files).

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const FAKE_MSB: &str = include_str!("msb.sh");
const FAKE_KEYCHAIN: &str = include_str!("keychain.sh");
const _: () = assert!(
    cfg!(debug_assertions),
    "the CLI tests need a debug build (the fake keychain)"
);

pub struct Machine {
    pub home: PathBuf,
    pub fake: PathBuf,
}

impl Machine {
    pub fn new(name: &str) -> Self {
        let home = std::env::temp_dir()
            .join(format!("hangar-cli-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let machine = Self {
            fake: home.join("fake"),
            home,
        };
        fs::create_dir_all(machine.home.join("bin")).unwrap();
        fs::create_dir_all(&machine.fake).unwrap();
        machine.script("msb", FAKE_MSB);
        // `security` exits 44 for a missing item, `secret-tool` 1.
        let missing = if cfg!(target_os = "macos") { 44 } else { 1 };
        machine
            .script("keychain", &format!("missing={missing}\n{FAKE_KEYCHAIN}"));
        machine
    }

    fn keychain_file(&self, account: &str) -> PathBuf {
        self.fake.join("keychain").join(account.replace('/', "%"))
    }

    pub fn keychain_set(&self, account: &str, token: &str) {
        fs::create_dir_all(self.fake.join("keychain")).unwrap();
        fs::write(self.keychain_file(account), format!("{token}\n")).unwrap();
    }

    pub fn keychain_get(&self, account: &str) -> Option<String> {
        let text = fs::read_to_string(self.keychain_file(account)).ok()?;
        Some(text.trim_end_matches('\n').to_string())
    }

    pub fn script(&self, name: &str, body: &str) {
        let path = self.home.join("bin").join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    pub fn fail(&self, what: &str, why: &str) {
        fs::write(self.fake.join(format!("fail-{what}")), why).unwrap();
    }

    pub fn msb_log(&self) -> String {
        fs::read_to_string(self.fake.join("msb.log")).unwrap_or_default()
    }

    pub fn fake_file(&self, name: &str) -> String {
        fs::read_to_string(self.fake.join(name)).unwrap_or_default()
    }

    pub fn hangar(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        self.command(args, env).output().unwrap()
    }

    pub fn hangar_with_stdin(&self, args: &[&str], input: &str) -> Output {
        let mut child = self
            .command(args, &[])
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

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let path = tools_path(self.home.join("bin"));
        let mut command = Command::new(env!("CARGO_BIN_EXE_hangar"));
        command
            .args(args)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", path)
            .env("HANGAR_FAKE", &self.fake)
            .env("HANGAR_KEYCHAIN_SERVICE", "hangar-test")
            .env("HANGAR_TEST_KEYCHAIN_TOOL", self.home.join("bin/keychain"))
            .envs(env.iter().copied());
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
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    if text.starts_with("NAME")
        && text.lines().next().unwrap().contains("RESULT")
    {
        change_lines(&text)
    } else {
        text
    }
}

fn change_lines(table: &str) -> String {
    let mut lines = table.lines();
    let header = lines.next().unwrap();
    let at = |name: &str| header.find(name);
    let result = at("RESULT").unwrap();
    let column = |line: &str, from: usize, to: usize| {
        line.get(from..to.min(line.len()))
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let mut out = String::new();
    for line in lines.filter(|line| !line.starts_with(' ')) {
        let name = column(line, 0, at("TYPE").unwrap());
        let entry = match at("DETAILS") {
            None => column(line, result, usize::MAX),
            Some(details) => {
                let word = column(line, result, details);
                let what = column(line, details, usize::MAX);
                match (word.as_str(), what.is_empty()) {
                    (_, true) => word,
                    ("failed", false) => format!("failed: {what}"),
                    (_, false) => format!("{word} ({what})"),
                }
            }
        };
        out.push_str(&name);
        out.push_str(": ");
        out.push_str(&entry);
        out.push('\n');
    }
    out
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
