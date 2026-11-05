#![expect(clippy::unwrap_used, reason = "test helpers panic on setup errors")]

mod fakes;

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::thread;

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
        "default: stopped\ndefault: updated (started)\n"
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
        stdout(&machine.hangar(&["ps"], &[]))
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
    for (src, dest, said) in [
        ("/a", "/b", "exactly one"),
        ("default:a", "default:b", "exactly one"),
        ("/a", "typo:b", "no bay or proxy typo"),
        ("/a", "default:", "default: has no path"),
    ] {
        let error = failed(&machine.hangar(&["cp", src, dest], &[]));
        assert!(error.contains(said), "{error}");
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

fn hangar_log(machine: &Machine) -> String {
    read(machine.home.join(".local/state/hangar/hangar.log"))
}

#[test]
fn log_file_gets_every_line_timestamped() {
    let machine = with_bay("log-file", "", "");
    run(&machine, &["up", "-q"]);
    let log = hangar_log(&machine);
    assert!(log.lines().count() >= 2, "{log}");
    for line in log.lines() {
        let (stamp, rest) = line.split_once(' ').unwrap();
        assert!(stamp.ends_with('Z') && stamp.len() == 20, "{line}");
        assert!(rest.starts_with("info ") || rest.starts_with("warn "));
    }
    assert!(log.contains("default: vm"), "{log}");
    assert!(log.contains("default: created"), "{log}");
}

#[test]
fn ps_exits_3_on_event_and_logs_it() {
    let machine = with_bay("ps-event", r#", "apps": ["web"]"#, WEB_APP);
    fs::write(machine.fake.join("setup-check"), "done").unwrap();
    run(&machine, &["up"]);
    assert!(run(&machine, &["ps", "-q"]).contains("running"));

    fs::remove_file(machine.fake.join("hangar-run-web.service")).unwrap();
    let output = machine.hangar(&["ps", "-q"], &[]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(stdout(&output).contains("default  bay   running"));
    assert!(
        hangar_log(&machine).contains(
            "warn bay default: app web isn't running; \
             see 'hangar logs default web'"
        ),
        "{}",
        hangar_log(&machine)
    );
}

#[test]
fn ps_points_at_setup_for_an_app_that_needs_it() {
    let machine = with_bay("ps-setup", r#", "apps": ["web"]"#, WEB_APP);
    run(&machine, &["up"]);
    let output = machine.hangar(&["ps"], &[]);
    assert_eq!(output.status.code(), Some(3));
    assert!(
        stderr(&output)
            .contains("app web needs setup; run 'hangar setup default web'"),
        "{}",
        stderr(&output)
    );
    assert!(!stderr(&output).contains("isn't running"));
}

#[test]
fn json_errors_reach_the_log_and_a_planted_log_symlink_is_ignored() {
    let machine = with_bay("log-json-err", "", "");
    let log = machine.home.join(".local/state/hangar/hangar.log");
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    let victim = machine.home.join("victim");
    fs::write(&victim, "keep").unwrap();
    std::os::unix::fs::symlink(&victim, &log).unwrap();
    let output = machine.hangar(&["rm", "nope", "--json"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(read(&victim), "keep");
    fs::remove_file(&log).unwrap();
    failed(&machine.hangar(&["rm", "nope", "--json"], &[]));
    assert!(hangar_log(&machine).contains("error no bay or proxy nope"));
}

#[test]
fn ps_exits_3_for_a_leftover() {
    let machine = with_bay("ps-leftover", "", "");
    fs::create_dir_all(data(&machine).join("bays/old")).unwrap();
    let output = machine.hangar(&["ps", "--json"], &[]);
    assert_eq!(output.status.code(), Some(3));
    assert!(stdout(&output).contains(r#""leftovers":[{"name":"old""#));
    assert!(stderr(&output).contains("hangar rm old --data"));
}

#[test]
fn logs_without_name_prints_the_log() {
    let machine = with_bay("logs-own", "", "");
    run(&machine, &["up"]);
    let out = run(&machine, &["logs"]);
    assert!(out.contains(" info default: created\n"), "{out}");
}

#[test]
fn secrets_never_in_log() {
    let machine = with_bay("no-secrets", "", "");
    run(&machine, &["up"]);
    let token = machine.home.join("token");
    fs::write(&token, "tok-SECRET-1").unwrap();
    let token = token.display().to_string();
    let output = machine.hangar(&["cp", &token, "default:~/t", "-vv"], &[]);
    let said = stderr(ok(&output));
    assert_eq!(machine.fake_file("vmfs/home/pilot/t"), "tok-SECRET-1");
    assert!(said.contains("run: msb"), "{said}");
    let log = hangar_log(&machine);
    assert!(log.contains("run: msb"), "{log}");
    assert!(!log.contains("SECRET") && !said.contains("SECRET"), "{log}");
}

fn proxied_machine(name: &str, proxy: &str) -> Machine {
    let machine = Machine::new(name);
    let config = format!(
        r#"{{"proxies": {{"p": {proxy}}},
            "bays": [{{"name": "default", "proxy": "p",
                       "image": "example/bay:1"}}]}}"#
    );
    let path = default_config(&machine);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, config).unwrap();
    machine
}

fn listener() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

#[test]
fn bay_through_http_proxy_has_one_egress_port() {
    let (_proxy, port) = listener();
    let machine = proxied_machine(
        "http-proxy",
        &format!(r#"{{"kind": "http", "url": "http://127.0.0.1:{port}"}}"#),
    );
    assert_eq!(run(&machine, &["up"]), "p: unchanged\ndefault: created\n");
    let created = calls(&machine, "create ");
    assert!(
        created[0].contains(&format!("allow@host:tcp:{port}")),
        "{created:?}"
    );
    let env = machine.fake_file("hangar-proxy-env");
    assert!(
        env.contains(&format!(
            "HTTPS_PROXY='http://host.microsandbox.internal:{port}'"
        )),
        "{env}"
    );
}

#[test]
fn switching_bay_proxy_recreates() {
    let (_first, first) = listener();
    let (_second, second) = listener();
    let proxy = |port| {
        format!(r#"{{"kind": "http", "url": "http://127.0.0.1:{port}"}}"#)
    };
    let machine = proxied_machine("http-switch", &proxy(first));
    run(&machine, &["up"]);
    let config = read(default_config(&machine))
        .replace(&first.to_string(), &second.to_string());
    fs::write(default_config(&machine), config).unwrap();
    assert_eq!(
        run(&machine, &["up"]),
        "p: unchanged\ndefault: recreated (proxy)\n"
    );
    let created = calls(&machine, "create ");
    assert!(
        created[1].contains(&format!("allow@host:tcp:{second}")),
        "{created:?}"
    );
}

#[test]
fn bay_through_agent_vault_gets_token_url_on_stdin_only() {
    let (_proxy, port) = listener();
    let machine = Machine::new("agent-vault");
    let token = machine.home.join("av-token");
    fs::write(&token, "av_SECRET-9\n").unwrap();
    let ca = concat!(env!("CARGO_MANIFEST_DIR"), "/src/proxy/testdata/ca.pem");
    let proxy = format!(
        r#"{{"kind": "agent-vault", "url": "http://127.0.0.1:{port}",
            "ca": {{"file": "{ca}"}}, "tokenFile": "{}"}}"#,
        token.display()
    );
    let config = format!(
        r#"{{"proxies": {{"p": {proxy}}}, "bays": [{{"name": "default",
            "proxy": "p", "image": "example/bay:1"}}]}}"#
    );
    let path = default_config(&machine);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, config).unwrap();

    let output = machine.hangar(&["up", "-vv"], &[]);
    let said = stderr(ok(&output));
    let env = machine.fake_file("hangar-proxy-env");
    assert!(
        env.contains(&format!(
            "HTTPS_PROXY='http://av_SECRET-9:default@\
             host.microsandbox.internal:{port}'"
        )),
        "{env}"
    );
    let log = hangar_log(&machine);
    for text in [machine.msb_log(), log, said] {
        assert!(!text.contains("SECRET"), "{text}");
    }
}

/// An Infisical proxy's CA endpoint on `port`: 503 until the fake msb has
/// started the proxy in `vm`, then the CA as JSON.
fn infisical_ca_server(listener: TcpListener, up: PathBuf) {
    let pem = include_str!("../src/proxy/testdata/ca.pem").replace('\n', "\\n");
    let body = format!(r#"{{"certificate": "{pem}"}}"#);
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = stream.read(&mut [0; 4096]);
            let (status, body) = if up.exists() {
                ("200 OK", body.as_str())
            } else {
                ("503 Starting", "")
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
}

/// Proxy `infi` (managed infisical) and bay `work` running app `web`
/// through it; returns the machine and the proxy's host port.
fn infisical_machine(name: &str) -> (Machine, u16) {
    let machine = Machine::new(name);
    let (listener, port) = listener();
    infisical_ca_server(
        listener,
        machine.fake.join("proxy-up-hangar-proxy-infi"),
    );
    fs::write(machine.home.join("proxy-token"), "pt-SECRET-1\n").unwrap();
    fs::write(machine.home.join("session-token"), "st-SECRET-2\n").unwrap();
    fs::write(machine.fake.join("setup-check"), "done").unwrap();
    let config = format!(
        r#"{{"proxies": {{"infi": {{"kind": "infisical", "run": "managed",
              "proxyTokenFile": "~/proxy-token",
              "sessionTokenFile": "~/session-token", "hostPort": {port}}}}},
            "bays": [{{"name": "work", "proxy": "infi",
                       "image": "example/bay:1", "apps": ["web"]}}]{WEB_APP}}}"#
    );
    let path = default_config(&machine);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, config).unwrap();
    (machine, port)
}

/// Runs `up -vv`; its stdout, after checking no token reached argv, a log
/// or stderr.
fn up_keeping_tokens_secret(machine: &Machine) -> String {
    let output = machine.hangar(&["up", "-vv"], &[]);
    let said = stderr(ok(&output));
    for text in [machine.msb_log(), hangar_log(machine), said] {
        assert!(!text.contains("SECRET"), "{text}");
    }
    stdout(&output)
}

#[test]
fn managed_infisical_then_bay() {
    let (machine, port) = infisical_machine("infisical");
    assert_eq!(
        up_keeping_tokens_secret(&machine),
        "infi: created\nwork: created\n"
    );
    let created = calls(&machine, "create ");
    assert_eq!(created.len(), 2, "{created:?}");
    assert!(
        created[0].contains("--name hangar-proxy-infi"),
        "{created:?}"
    );
    assert!(created[0].contains("--entrypoint /bin/sh"), "{created:?}");
    assert!(!created[0].contains("--no-net"), "{created:?}");
    assert!(created[1].contains("--name hangar-bay-work"), "{created:?}");
    assert!(
        created[1].contains(&format!("allow@host:tcp:{port}")),
        "{created:?}"
    );
    let started = calls(&machine, "exec --no-tty hangar-proxy-infi");
    assert!(
        started.last().unwrap().ends_with(" hangar-infisical"),
        "{started:?}"
    );
    assert_eq!(machine.fake_file("hangar-infisical"), "pt-SECRET-1\n");
    let env = machine.fake_file("hangar-proxy-env");
    assert!(
        env.contains(&format!(
            "HTTPS_PROXY='http://x-agent-vault:st-SECRET-2@\
             host.microsandbox.internal:{port}'"
        )),
        "{env}"
    );
    assert!(machine.fake_file("hangar-ca").contains("BEGIN CERTIFICATE"));
}

#[test]
fn renewed_session_token_updates_bays_in_place() {
    let (machine, _) = infisical_machine("infisical-session");
    up_keeping_tokens_secret(&machine);
    fs::write(machine.home.join("session-token"), "st-SECRET-3\n").unwrap();
    assert_eq!(
        up_keeping_tokens_secret(&machine),
        "infi: unchanged\nwork: updated (proxy login; restarted web)\n"
    );
    assert_eq!(calls(&machine, "create ").len(), 2);
    assert!(
        machine
            .fake_file("hangar-proxy-env")
            .contains("st-SECRET-3")
    );
}

#[test]
fn rotated_proxy_token_recreates_the_proxy_only() {
    let (machine, _) = infisical_machine("infisical-rotate");
    up_keeping_tokens_secret(&machine);
    let data = data(&machine).join("proxies/infi/data/data");
    fs::write(data.join("kept"), "x").unwrap();
    fs::write(machine.home.join("proxy-token"), "pt-SECRET-4\n").unwrap();
    assert_eq!(
        up_keeping_tokens_secret(&machine),
        "infi: recreated (fingerprint)\nwork: unchanged\n"
    );
    let created = calls(&machine, "create ");
    assert_eq!(created.len(), 3, "{created:?}");
    assert!(
        created[2].contains("--name hangar-proxy-infi"),
        "{created:?}"
    );
    assert_eq!(machine.fake_file("hangar-infisical"), "pt-SECRET-4\n");
    assert!(data.join("kept").exists());
}

#[test]
fn up_loads_a_nix_built_image_through_its_loader() {
    let machine = with_bay("loader", r#", "imageLoader": "image-loader""#, "");
    machine.script("image-loader", "exit 0");
    assert_eq!(run(&machine, &["up"]), "default: created\n");
    assert_eq!(calls(&machine, "load "), ["load -q --tag example/bay:1"]);
}

#[test]
fn up_reports_a_failed_image_load() {
    let machine =
        with_bay("loader-fail", r#", "imageLoader": "image-loader""#, "");
    machine.script("image-loader", "exit 0");
    machine.fail("load", "error: corrupt archive");
    let output = machine.hangar(&["up"], &[]);
    assert_eq!(output.status.code(), Some(1));
    let out = stdout(&output);
    assert!(out.contains("msb load example/bay:1 failed"), "{out}");
}

#[test]
fn up_reports_a_volume_msb_cannot_inspect() {
    let machine = with_bay("volume-inspect", "", "");
    machine.fail("volume", "error: database locked");
    let output = machine.hangar(&["up"], &[]);
    assert_eq!(output.status.code(), Some(1));
    let out = stdout(&output);
    assert!(out.contains("msb volume inspect"), "{out}");
    assert!(out.contains("database locked"), "{out}");
}

#[test]
fn rm_data_reports_a_volume_msb_cannot_remove() {
    let machine = with_bay("volume-rm", "", "");
    run(&machine, &["up"]);
    machine.fail("volume", "error: volume busy");
    let output = machine.hangar(&["rm", "default", "--data", "--yes"], &[]);
    assert_eq!(output.status.code(), Some(1));
    let out = stdout(&output);
    assert!(
        out.contains("msb volume rm hangar-bay-default-data"),
        "{out}"
    );
    assert!(out.contains("volume busy"), "{out}");
}

#[test]
fn a_missing_config_points_at_init() {
    let machine = Machine::new("no-config");
    let error = failed(&machine.hangar(&["ps"], &[]));
    assert!(error.contains("no config at"), "{error}");
    assert!(error.contains("hangar init"), "{error}");
}
