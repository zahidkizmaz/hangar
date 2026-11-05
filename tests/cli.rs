#![expect(clippy::unwrap_used, reason = "test helpers panic on setup errors")]

mod fakes;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use fakes::{Machine, stderr, stdout};

fn default_config(machine: &Machine) -> PathBuf {
    machine.home.join(".config/hangar/hangar.json")
}

/// clap's usage errors: exit 2, the message on stderr.
fn usage_error(output: &Output) -> String {
    assert_eq!(output.status.code(), Some(2), "stdout: {}", stdout(output));
    stderr(output)
}

fn ok(output: &Output) -> &Output {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(output),
        stderr(output)
    );
    output
}

fn failed(output: &Output) -> String {
    assert!(!output.status.success(), "stdout: {}", stdout(output));
    stderr(output)
}

fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path).unwrap()
}

#[test]
fn init_writes_a_minimal_config_once() {
    let machine = Machine::new("init");
    ok(&machine.hangar(&["init"], &[]));
    let config = read(default_config(&machine));
    assert_eq!(
        config,
        "{\n  \"proxies\": {},\n  \"bays\": \
         [{ \"name\": \"default\", \"proxy\": null, \"apps\": [] }]\n}\n"
    );

    let error = failed(&machine.hangar(&["init"], &[]));
    assert!(error.contains("exists"), "{error}");
    ok(&machine.hangar(&["init", "--force"], &[]));
    let error = usage_error(&machine.hangar(&["init", "--nope"], &[]));
    assert!(error.contains("--nope"), "{error}");
}

#[test]
fn init_refuses_when_the_nix_module_owns_the_config() {
    let machine = Machine::new("init-nix");
    let output = machine.hangar(
        &["init"],
        &[("HANGAR_NIX_CONFIG", "/nix/store/x-hangar.json")],
    );
    assert!(failed(&output).contains("Nix module"));
    assert!(!default_config(&machine).exists());
}

#[test]
fn usage_without_or_with_an_unknown_command() {
    let machine = Machine::new("usage");
    let error = usage_error(&machine.hangar(&[], &[]));
    assert!(error.contains("COMMAND"), "{error}");
    let error = usage_error(&machine.hangar(&["fly"], &[]));
    assert!(error.contains("fly"), "{error}");
}

#[test]
fn removed_commands_are_unknown() {
    let machine = Machine::new("removed");
    for command in ["credential", "vault-ui", "status", "destroy", "shell"] {
        let error = usage_error(&machine.hangar(&[command], &[]));
        assert!(error.contains("unrecognized subcommand"), "{error}");
    }
}

#[test]
fn help_and_version_are_printed() {
    let machine = Machine::new("help");
    let help = stdout(ok(&machine.hangar(&["--help"], &[])));
    assert!(
        help.contains("Exit codes: 0 ok, 1 error, 2 usage"),
        "{help}"
    );
    assert!(help.contains("--json"), "{help}");
    let help = stdout(ok(&machine.hangar(&["init", "--help"], &[])));
    assert!(help.contains("Usage: hangar init"), "{help}");
    let version = stdout(ok(&machine.hangar(&["--version"], &[])));
    assert!(version.contains(env!("CARGO_PKG_VERSION")), "{version}");
}
