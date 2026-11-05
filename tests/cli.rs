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

const BAY_VM: &str = "hangar-bay-default";
const WEB_APP: &str = r#", "apps": {"web": {"run": "web-serve",
    "setup": {"command": "web-login", "check": "web-ok"}}}"#;

/// A machine whose config has one direct bay `default` on `image`, plus
/// `bay` (JSON fields) and top-level `rest`.
fn with_bay(name: &str, bay: &str, rest: &str) -> Machine {
    let machine = Machine::new(name);
    set_bay(&machine, "example/bay:1", bay, rest);
    machine
}

fn set_bay(machine: &Machine, image: &str, bay: &str, rest: &str) {
    let config = format!(
        r#"{{"bays": [{{"name": "default", "proxy": null,
            "image": "{image}"{bay}}}]{rest}}}"#
    );
    let path = default_config(machine);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, config).unwrap();
}

fn run(machine: &Machine, args: &[&str]) -> String {
    stdout(ok(&machine.hangar(args, &[])))
}

fn data(machine: &Machine) -> PathBuf {
    machine.home.join(".local/share/hangar")
}

fn calls(machine: &Machine, prefix: &str) -> Vec<String> {
    machine
        .msb_log()
        .lines()
        .filter(|line| line.starts_with(prefix))
        .map(|line| line.trim_end().to_string())
        .collect()
}

#[test]
fn up_creates_a_direct_bay_and_reports_created() {
    let machine = with_bay("up", "", "");
    assert_eq!(run(&machine, &["up"]), "default: created\n");
    let created = calls(&machine, "create ");
    assert_eq!(created.len(), 1, "{created:?}");
    assert!(
        created[0]
            .starts_with("create example/bay:1 --name hangar-bay-default"),
        "{created:?}"
    );
    assert!(!created[0].contains("--no-net"), "{created:?}");
    assert_eq!(
        calls(&machine, "volume create"),
        ["volume create --kind disk --size 40G -q hangar-bay-default-data"]
    );
}

#[test]
fn second_up_reports_unchanged() {
    let machine = with_bay("up-again", "", "");
    run(&machine, &["up"]);
    assert_eq!(run(&machine, &["up", "default"]), "default: unchanged\n");
    assert_eq!(calls(&machine, "create ").len(), 1);
    assert_eq!(calls(&machine, "volume create").len(), 1);
}

#[test]
fn up_after_image_change_reports_recreated_with_reason() {
    let machine = with_bay("up-image", "", "");
    run(&machine, &["up"]);
    set_bay(&machine, "example/bay:2", "", "");
    assert_eq!(run(&machine, &["up"]), "default: recreated (image)\n");
    let log = machine.msb_log();
    let stop = log.find("stop -q hangar-bay-default").unwrap();
    let remove = log.find("rm -f -q hangar-bay-default").unwrap();
    assert!(stop < remove, "{log}");
    assert!(log.contains("create example/bay:2"), "{log}");
}

#[test]
fn up_reports_a_failed_bay_and_exits_1() {
    let machine = with_bay("up-fail", "", "");
    machine.fail("create", "error: out of disk");
    let output = machine.hangar(&["up"], &[]);
    assert_eq!(output.status.code(), Some(1));
    let out = stdout(&output);
    assert!(
        out.starts_with("default: failed: bay default: vm: msb create failed"),
        "{out}"
    );
    assert!(out.contains("out of disk"), "{out}");
}

#[test]
fn down_stops_and_reports_stopped() {
    let machine = with_bay("down", "", "");
    run(&machine, &["up"]);
    assert_eq!(run(&machine, &["down"]), "default: stopped\n");
    assert_eq!(machine.fake_file(&format!("vm-{BAY_VM}")), "Stopped\n");
    assert_eq!(run(&machine, &["down", "default"]), "default: unchanged\n");
}

#[test]
fn restart_is_down_then_up() {
    let machine = with_bay("restart", "", "");
    run(&machine, &["up"]);
    assert_eq!(
        run(&machine, &["restart", "default"]),
        "default: stopped\ndefault: unchanged\n"
    );
    let log = machine.msb_log();
    let stop = log.find("stop -q hangar-bay-default").unwrap();
    let start = log.find("start -q hangar-bay-default").unwrap();
    assert!(stop < start, "{log}");
}

#[test]
fn rm_keeps_home_rm_data_deletes_it_with_yes() {
    let machine = with_bay("rm", "", "");
    run(&machine, &["up"]);
    let notes = data(&machine).join("bays/default/home/notes");
    fs::write(&notes, "keep me").unwrap();
    assert_eq!(run(&machine, &["rm", "default"]), "default: removed\n");
    assert_eq!(machine.fake_file(&format!("vm-{BAY_VM}")), "");
    assert_eq!(read(&notes), "keep me");
    assert_eq!(calls(&machine, "volume rm"), [""; 0]);

    run(&machine, &["up"]);
    let error = failed(&machine.hangar(&["rm", "default", "--data"], &[]));
    assert!(error.contains("add --yes"), "{error}");
    assert!(notes.exists());

    assert_eq!(
        run(&machine, &["rm", "default", "--data", "--yes"]),
        "default: removed\n"
    );
    assert!(!data(&machine).join("bays/default").exists());
    assert_eq!(
        calls(&machine, "volume rm"),
        ["volume rm hangar-bay-default-data"]
    );
    assert!(machine.home.join(".cache/hangar/bays/default").exists());
}

#[test]
fn rm_removes_a_leftover_bay() {
    let machine = with_bay("rm-leftover", "", "");
    fs::create_dir_all(data(&machine).join("bays/old/home")).unwrap();
    assert_eq!(
        run(&machine, &["rm", "old", "--data", "--yes"]),
        "old: removed\n"
    );
    assert!(!data(&machine).join("bays/old").exists());
    assert_eq!(
        calls(&machine, "volume rm"),
        ["volume rm hangar-bay-old-data"]
    );
}

#[test]
fn rm_unknown_name_lists_bays_and_proxies() {
    let machine = with_bay("rm-unknown", "", "");
    let error = failed(&machine.hangar(&["rm", "nope"], &[]));
    assert!(error.contains("no bay or proxy nope"), "{error}");
    assert!(error.contains("bays: default; proxies: none"), "{error}");
}

#[test]
fn ps_lists_bays_with_state_proxy_image() {
    let machine = with_bay("ps", "", "");
    let rows = |machine: &Machine| -> Vec<Vec<String>> {
        run(machine, &["ps"])
            .lines()
            .map(|line| line.split_whitespace().map(String::from).collect())
            .collect()
    };
    assert_eq!(
        rows(&machine),
        [
            ["NAME", "TYPE", "STATE", "PROXY", "IMAGE"],
            ["default", "bay", "missing", "direct", "example/bay:1"]
        ]
    );
    run(&machine, &["up"]);
    fs::create_dir_all(data(&machine).join("bays/old")).unwrap();
    assert_eq!(
        rows(&machine)[1..],
        [
            ["default", "bay", "running", "direct", "example/bay:1"],
            ["old", "bay", "leftover", "-", "-"]
        ]
    );
}

#[test]
fn ps_says_unknown_when_the_sandbox_cant_tell() {
    let machine = with_bay("ps-unknown", "", "");
    machine.fail("inspect", "error: msb is broken");
    let output = machine.hangar(&["ps"], &[]);
    let output = ok(&output);
    assert!(
        stdout(output).contains("default  bay   unknown"),
        "{}",
        stdout(output)
    );
    assert!(
        stderr(output).contains("msb is broken"),
        "{}",
        stderr(output)
    );
}

#[test]
fn inspect_json_has_version_1_and_mounts() {
    let machine = with_bay(
        "inspect",
        r#", "mounts": {"~/src": {"host": "~/src", "mode": "ro"}}"#,
        "",
    );
    let json = run(&machine, &["inspect", "default", "--json"]);
    assert!(json.contains(r#""version":1"#), "{json}");
    assert!(json.contains(r#""mounts":[{"#), "{json}");
    assert!(json.contains(r#""vm":"/home/pilot""#), "{json}");
    assert!(json.contains(r#""kind":"user","mode":"ro""#), "{json}");
    assert!(
        json.contains(r#""volume":"hangar-bay-default-data""#),
        "{json}"
    );
    let text = run(&machine, &["inspect", "default"]);
    assert!(text.contains("state: missing"), "{text}");
    assert!(text.contains("/home/pilot"), "{text}");
}

#[test]
fn exec_runs_a_login_shell_as_pilot() {
    let machine = with_bay("exec", "", "");
    let error = failed(&machine.hangar(&["exec", "default"], &[]));
    assert!(error.contains("hangar up default"), "{error}");

    run(&machine, &["up"]);
    run(&machine, &["exec", "default"]);
    let shells = calls(&machine, "exec --no-tty --user pilot --workdir");
    assert!(shells.last().unwrap().ends_with("-- sh -l"), "{shells:?}");
    let out = run(&machine, &["exec", "default", "git", "-C", "repo", "log"]);
    assert!(
        out.contains(r#"sh -lc exec "$@" sh git -C repo log"#),
        "{out}"
    );
}

#[test]
fn cp_into_bay_runs_copy_checks() {
    let machine = with_bay("cp-in", "", "");
    let src = machine.home.join("notes.txt");
    fs::write(&src, "hello").unwrap();
    let src = src.display().to_string();
    let error = failed(&machine.hangar(&["cp", &src, "default:~/n"], &[]));
    assert!(error.contains("hangar up default"), "{error}");

    run(&machine, &["up"]);
    let out = run(&machine, &["cp", &src, "default:~/n.txt"]);
    assert_eq!(out, "copied /home/pilot/n.txt\n");
    assert_eq!(machine.fake_file("vmfs/home/pilot/n.txt"), "hello");
    let error = failed(&machine.hangar(&["cp", &src, "default:/etc/n"], &[]));
    assert!(error.contains("must be in ~"), "{error}");
}

#[test]
fn cp_needs_exactly_one_bay_side() {
    let machine = with_bay("cp-sides", "", "");
    for (src, dest) in [("a", "b"), ("default:a", "default:b")] {
        let error = failed(&machine.hangar(&["cp", src, dest], &[]));
        assert!(error.contains("exactly one"), "{error}");
    }
}

#[test]
fn cp_out_of_bay_uses_sandbox_copy() {
    let machine = with_bay("cp-out", "", "");
    let dest = fs::canonicalize(&machine.home).unwrap().join("got.txt");
    let dest_arg = dest.display().to_string();
    let error =
        failed(&machine.hangar(&["cp", "default:~/out.txt", &dest_arg], &[]));
    assert!(error.contains("hangar up default"), "{error}");
    assert_eq!(calls(&machine, "copy"), [""; 0]);

    run(&machine, &["up"]);
    let guest = machine.fake.join("vmfs/home/pilot");
    fs::create_dir_all(&guest).unwrap();
    fs::write(guest.join("out.txt"), "from the bay").unwrap();
    let out = run(&machine, &["cp", "default:~/out.txt", &dest_arg]);
    assert_eq!(out, format!("copied {dest_arg}\n"));
    assert_eq!(read(&dest), "from the bay");
    assert_eq!(
        calls(&machine, "copy"),
        [format!(
            "copy -q hangar-bay-default:/home/pilot/out.txt {dest_arg}"
        )]
    );

    let error =
        failed(&machine.hangar(&["cp", "default:~/out.txt", &dest_arg], &[]));
    assert!(error.contains("exists"), "{error}");
    let missing = machine.home.join("missing.txt").display().to_string();
    let error = failed(
        &machine.hangar(&["cp", "default:/home/pilot/gone", &missing], &[]),
    );
    assert!(error.contains("error: stat /home/pilot/gone"), "{error}");
    let into_state = data(&machine).join("x").display().to_string();
    let error =
        failed(&machine.hangar(&["cp", "default:~/out.txt", &into_state], &[]));
    assert!(error.contains("hangar's own state"), "{error}");
}

#[test]
fn setup_needs_a_running_bay() {
    let machine = with_bay("setup", r#", "apps": ["web"]"#, WEB_APP);
    let error = failed(&machine.hangar(&["setup", "default", "web"], &[]));
    assert!(error.contains("hangar up default"), "{error}");

    run(&machine, &["up"]);
    let error = failed(&machine.hangar(&["setup", "default", "web"], &[]));
    assert!(error.contains("setup check still fails"), "{error}");
    assert!(error.contains("hangar setup default web"), "{error}");
    assert!(machine.msb_log().contains("sh -lc web-login"));

    fs::write(machine.fake.join("setup-check"), "done").unwrap();
    let said = stderr(ok(&machine.hangar(&["setup", "default", "web"], &[])));
    assert!(said.contains("already set up"), "{said}");
    let error = failed(&machine.hangar(&["setup", "default", "nope"], &[]));
    assert!(error.contains("nope is not an enabled app"), "{error}");
}

#[test]
fn logs_show_the_hangar_log_a_bay_or_one_app() {
    let machine = with_bay("logs", r#", "apps": ["web"]"#, WEB_APP);
    let log = machine.home.join(".local/state/hangar/hangar.log");
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    let lines: Vec<String> = (1..=250).map(|n| format!("line {n}")).collect();
    fs::write(&log, lines.join("\n") + "\n").unwrap();
    let out = run(&machine, &["logs"]);
    assert_eq!(out.lines().count(), 200);
    assert!(out.starts_with("line 51\n"), "{out}");

    run(&machine, &["up"]);
    let out = run(&machine, &["logs", "default"]);
    assert!(out.contains("journalctl --user -u hangar-run-*"), "{out}");
    let out = run(&machine, &["logs", "default", "web"]);
    assert!(out.contains("-u hangar-run-web.service"), "{out}");
    let error = failed(&machine.hangar(&["logs", "default", "nope"], &[]));
    assert!(error.contains("nope"), "{error}");
}

#[test]
fn bays_are_positional_no_bay_flag() {
    let machine = with_bay("positional", "", "");
    for args in [
        &["up", "--bay", "default"][..],
        &["logs", "--bay", "default"],
        &["exec", "--bay", "default"],
    ] {
        let error = usage_error(&machine.hangar(args, &[]));
        assert!(error.contains("--bay"), "{error}");
    }
}

#[test]
fn json_on_every_command() {
    let machine = with_bay("json", "", "");
    let src = machine.home.join("a.txt");
    fs::write(&src, "a").unwrap();
    let src = src.display().to_string();
    let commands: [&[&str]; 7] = [
        &["up"],
        &["ps"],
        &["inspect", "default"],
        &["cp", &src, "default:~/a.txt"],
        &["restart"],
        &["down"],
        &["rm", "default"],
    ];
    for args in commands {
        let args = [args, &["--json"]].concat();
        let out = run(&machine, &args);
        assert_eq!(out.lines().count(), 1, "{args:?}: {out}");
        assert!(out.starts_with('{'), "{args:?}: {out}");
        assert!(out.contains(r#""version":1"#), "{args:?}: {out}");
    }
    let error = failed(&machine.hangar(&["rm", "nope", "--json"], &[]));
    assert_eq!(
        error,
        "{\"error\":\"no bay or proxy nope\",\
         \"hint\":\"bays: default; proxies: none\"}\n"
    );
    let out = run(&machine, &["up", "--json"]);
    assert_eq!(
        out,
        "{\"changes\":[{\"error\":null,\"name\":\"default\",\
         \"outcome\":\"created\",\"reasons\":[],\"type\":\"bay\"}],\
         \"version\":1}\n"
    );
}
