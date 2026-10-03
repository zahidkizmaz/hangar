#![expect(clippy::unwrap_used, reason = "test helpers panic on setup errors")]

mod fakes;

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

use fakes::{FakeVault, Machine, stderr, stdout};

const PASSWORD: &str = "master-password-1";
const GITHUB_TOKEN: &str = "dummy-github-token";

fn default_config(machine: &Machine) -> PathBuf {
    machine.home.join(".config/hangar/hangar.json")
}

fn write_default_config(machine: &Machine, json: &str) {
    fs::write(default_config(machine), json).unwrap();
}

/// hangar against the fakes: a scratch config and state, and the master
/// password from the environment (never the keychain).
fn run(machine: &Machine, config: &Path, args: &[&str]) -> Output {
    machine.hangar(args, &env(machine, config))
}

fn run_with_stdin(
    machine: &Machine,
    config: &Path,
    args: &[&str],
    input: &str,
) -> Output {
    machine.hangar_with_stdin(args, &env(machine, config), input)
}

fn env<'a>(machine: &'a Machine, config: &'a Path) -> [(&'a str, &'a str); 3] {
    [
        ("HANGAR_CONFIG", config.to_str().unwrap()),
        ("HANGAR_STATE_DIR", machine.state.to_str().unwrap()),
        ("HANGAR_MASTER_PASSWORD", PASSWORD),
    ]
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

/// `status` ran, but something isn't healthy: exit 3.
fn unhealthy(output: &Output) -> &Output {
    assert_eq!(output.status.code(), Some(3), "stderr: {}", stderr(output));
    output
}

fn failed(output: &Output) -> String {
    assert!(!output.status.success(), "stdout: {}", stdout(output));
    stderr(output)
}

/// A config pointing hangar at the fake vault, with extra top-level keys
/// and keys of bay `default` spliced in.
fn vault_config(vault_port: u16, top: &str, bay: &str) -> String {
    tower_config(vault_port, "", top, bay)
}

/// Like [`vault_config`], with keys of the tower spliced in too. The bay
/// gets the package hosts (`nix` and GitHub, with the token when the tower
/// has one) unless it names its own apps.
fn tower_config(vault_port: u16, tower: &str, top: &str, bay: &str) -> String {
    let github = if tower.contains("GITHUB_TOKEN") {
        "github-token"
    } else {
        "github"
    };
    let apps = if bay.contains(r#""apps""#) {
        String::new()
    } else {
        format!(r#", "apps": ["nix", "{github}"]"#)
    };
    format!(
        r#"{{"tower": {{"agentVault": {{"adminPort": {vault_port}}}{tower}}},
            "bays": [{{"name": "default", "image": "example/agent:1"{apps}{bay}}}]{top}}}"#
    )
}

/// A port nothing listens on, so a real agent-vault is never reached.
fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn token_file(machine: &Machine, name: &str, value: &str) -> String {
    let path = machine.home.join(name);
    fs::write(&path, format!("{value}\n")).unwrap();
    path.display().to_string()
}

fn lines_with<'a>(log: &'a str, needle: &str) -> Vec<&'a str> {
    log.lines().filter(|line| line.contains(needle)).collect()
}

/// msb calls of one subcommand (log lines starting with `prefix`).
fn calls(log: &str, prefix: &str) -> usize {
    log.lines().filter(|line| line.starts_with(prefix)).count()
}

/// The names in directory `dir`, sorted.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into())
        .collect();
    names.sort();
    names
}

fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path).unwrap()
}

#[test]
fn init_writes_a_minimal_config_once() {
    let machine = Machine::new("init");
    ok(&machine.hangar(&["init"], &[]));
    let config = read(default_config(&machine));
    assert!(config.contains(r#""packages": []"#), "{config}");
    assert!(!config.contains("image"), "{config}");
    assert!(!config.contains("vault"), "{config}");

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
fn up_without_config_explains_init() {
    let machine = Machine::new("no-config");
    let error = failed(&machine.hangar(&["up"], &[]));
    assert!(error.contains("run 'hangar init'"), "{error}");
}

#[test]
fn an_invalid_config_names_the_file() {
    let machine = Machine::new("bad-config");
    let config = machine.config("{not json");
    let error = failed(&run(&machine, &config, &["status"]));
    assert!(error.contains("invalid config"), "{error}");
    assert!(error.contains(&config.display().to_string()), "{error}");

    let config = machine.config(r#"{"sandbox": {"backend": "docker"}}"#);
    let error = failed(&run(&machine, &config, &["status"]));
    assert!(
        error.contains(
            r#"sandbox.backend: unknown backend "docker"; known: msb"#
        ),
        "{error}"
    );

    for (config, why) in [
        (
            r#"{"tower": {"backend": "other"}}"#,
            r#"tower.backend: unknown backend "other"; known: agent-vault"#,
        ),
        (
            r#"{"proxyPort": 15000}"#,
            "config.proxyPort: unknown setting",
        ),
        (
            r#"{"tower": {"routes": [{"name": "x", "host": "x.example",
                "auth": {"type": "passthrough"}, "extra": {"host": "y"}}]}}"#,
            "config.tower.routes.x.extra.host: set by hangar",
        ),
    ] {
        let error =
            failed(&run(&machine, &machine.config(config), &["status"]));
        assert!(error.contains(why), "{error}");
    }
}

#[test]
fn up_stops_early_without_a_password() {
    let machine = Machine::new("up-early");
    ok(&machine.hangar(&["init"], &[]));
    let password = machine.home.join("secret");
    write_default_config(
        &machine,
        &format!(
            r#"{{"bays": [{{"name": "default", "image": "example/agent:1"}}],
                "tower": {{"masterPasswordFile": "{}"}}}}"#,
            password.display()
        ),
    );
    let error = failed(&machine.hangar(&["up"], &[]));
    assert!(
        error.contains("master password file not readable"),
        "{error}"
    );

    fs::write(&password, "\n").unwrap();
    let error = failed(&machine.hangar(&["up"], &[]));
    assert!(error.contains("master password is empty"), "{error}");
    assert!(!machine.home.join(".local/share/hangar").exists());
}

#[test]
fn existing_vault_without_password_refuses_to_generate() {
    let machine = Machine::new("no-password");
    ok(&machine.hangar(&["init"], &[]));
    write_default_config(
        &machine,
        r#"{"bays": [{"name": "default", "image": "example/agent:1"}]}"#,
    );
    let vault = machine.home.join(".local/share/hangar/vault/.agent-vault");
    fs::create_dir_all(&vault).unwrap();
    let error = failed(&machine.hangar(&["up"], &[]));
    assert!(error.contains("but broker data exists in"), "{error}");
}

#[test]
fn a_keychain_service_name_with_odd_characters_is_refused() {
    let machine = Machine::new("bad-service");
    ok(&machine.hangar(&["init"], &[]));
    write_default_config(
        &machine,
        r#"{"bays": [{"name": "default", "image": "example/agent:1"}]}"#,
    );
    let output =
        machine.hangar(&["up"], &[("HANGAR_KEYCHAIN_SERVICE", "a b;rm")]);
    assert!(failed(&output).contains("only letters, digits"));
}

/// Secrets reach the VMs on stdin only, never in argv.
fn assert_secrets_on_stdin_only(machine: &Machine, log: &str) {
    let owner = read(machine.state.join("owner-password"));
    assert_eq!(machine.fake_file("vault-password"), PASSWORD);
    assert_eq!(machine.fake_file("owner-register"), owner);
    assert_eq!(
        machine.fake_file("proxy-url"),
        "http://agent-token-1:default@host.microsandbox.internal:14322\n"
    );
    for secret in [PASSWORD, GITHUB_TOKEN, owner.as_str(), "agent-token-1"] {
        assert!(!log.contains(secret), "{secret} in argv:\n{log}");
    }
}

/// Deny before any credential, a bay's first token replaces any vault
/// agent of its name, and every call carries the session token.
fn assert_first_up_admin_calls(vault: &FakeVault) {
    let admin = vault.admin_requests();
    assert_eq!(admin[0].method, "PATCH");
    assert!(admin[0].body.contains(r#""unmatched_host_policy":"deny""#));
    let posts: Vec<_> = admin
        .iter()
        .filter(|r| r.method == "POST" && r.path == "/v1/credentials")
        .collect();
    assert_eq!(posts.len(), 2);
    assert!(posts.iter().any(|r| r.body.contains(GITHUB_TOKEN)));
    assert!(posts.iter().any(|r| r.body.contains("x-access-token")));
    let replaced: Vec<&str> = admin
        .iter()
        .filter(|r| r.path.starts_with("/v1/agents/"))
        .map(|r| r.path.as_str())
        .collect();
    assert_eq!(replaced, ["/v1/agents/hangar-default/delete"]);
    for request in &admin {
        assert_eq!(request.auth.as_deref(), Some("Bearer session-token"));
    }
}

#[test]
fn up_builds_both_vms_once_and_keeps_secrets_off_the_command_line() {
    let machine = Machine::new("up");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    let token = token_file(&machine, "gh-token", GITHUB_TOKEN);
    let web = closed_port();
    let config = machine.config(&tower_config(
        vault.port,
        &format!(r#", "credentialFiles": {{"GITHUB_TOKEN": "{token}"}}"#),
        &format!(
            r#", "appDefinitions": {{"web": {{"ports": [
                 {{"name": "web", "vm": 3100, "host": {web},
                   "purpose": "Web UI"}}]}}}}"#
        ),
        r#", "apps": ["nix", "github-token", "web"],
           "packages": ["nixpkgs#rtk"]"#,
    ));

    let first = stderr(ok(&run(&machine, &config, &["up"])));
    assert!(first.contains("==> creating hangar-tower"), "{first}");
    assert!(
        first.contains("==> creating hangar-bay-default\n"),
        "{first}"
    );
    assert!(first.contains("==> installing nixpkgs#rtk"), "{first}");
    assert!(first.ends_with("==> ready: hangar shell\n"), "{first}");

    let log = machine.msb_log();
    let bay =
        lines_with(&log, "create example/agent:1 --name hangar-bay-default ");
    assert_eq!(bay.len(), 1, "{log}");
    assert!(bay[0].contains("--no-net --net-rule allow@host:tcp:14322"));
    // Deny-all plus that one port: every other rule is ingress only.
    let rules: Vec<&str> = bay[0]
        .split(" --net-rule ")
        .skip(1)
        .map(|rest| rest.split(' ').next().unwrap())
        .collect();
    assert_eq!(
        rules,
        ["allow@host:tcp:14322", "allow:ingress@any:tcp:3100"]
    );
    assert!(bay[0].contains(&format!("-p 127.0.0.1:{web}:3100")));
    assert_eq!(
        read(machine.state.join("bays/default/vm")),
        format!(
            "image\texample/agent:1\negress\t14322\n\
             port\tweb\t{web}\t3100\n\
             mount\t/home/pilot\t{home}/state/bays/default/home\trw\n\
             mount\t/var/cache/hangar\t{home}/.cache/hangar/bays/default\trw\n",
            home = fs::canonicalize(&machine.home).unwrap().display(),
        )
    );
    assert!(bay[0].contains(&format!(
        "--mount-dir {}/guest:/run/hangar:ro",
        machine.state.display()
    )));
    let vault_vm = lines_with(&log, "--name hangar-tower");
    assert_eq!(vault_vm.len(), 1, "{log}");
    assert_eq!(
        read(machine.state.join("tower-vm")),
        format!(
            "port\tvault-ui\t{}\t14321\nport\tproxy\t14322\t14322\n",
            vault.port
        )
    );
    assert!(vault_vm[0].contains(&format!("-p 127.0.0.1:{}:", vault.port)));
    let start = "exec --no-tty --user root hangar-bay-default -- \
                 /run/current-system/sw/bin/hangar-start";
    assert!(log.lines().any(|line| line.trim_end() == start), "{log}");

    assert_secrets_on_stdin_only(&machine, &log);

    assert_eq!(mode(&machine.state), 0o700);
    assert_eq!(mode(&machine.state.join("owner-password")), 0o600);
    let agent_token = machine.state.join("agent-tokens/default");
    assert_eq!(read(&agent_token), "agent-token-1");
    assert_eq!(
        lines_with(&log, "agent-vault agent create hangar-default ").len(),
        1,
        "{log}"
    );
    assert_eq!(mode(&agent_token), 0o600);
    // The VM's mount holds the CA, nothing secret.
    assert_eq!(entries(&machine.state.join("guest")), ["ca.pem"]);
    assert_eq!(read(machine.state.join("guest/ca.pem")), "FAKE-CA\n");

    let services = machine.fake_file("services.json");
    assert!(services.contains(r#""name":"github-api""#), "{services}");
    assert!(services.contains(r#""token":"GITHUB_TOKEN""#), "{services}");

    assert_first_up_admin_calls(&vault);
    assert_eq!(
        read(machine.state.join("credential-keys")),
        "GITHUB_GIT_USER\nGITHUB_TOKEN\n"
    );
    assert_eq!(machine.fake_file("tracked"), r#"{"nixpkgs#rtk":"rtk"}"#);

    // A second run creates nothing and reuses the owner and agent token.
    let second = stderr(ok(&run(&machine, &config, &["up"])));
    assert!(!second.contains("creating"), "{second}");
    assert!(!second.contains("installing"), "{second}");
    let log = machine.msb_log();
    assert_eq!(calls(&log, "create "), 2, "{log}");
    assert_eq!(lines_with(&log, "hangar-tower -- sh -c").len(), 1, "{log}");
    assert_eq!(lines_with(&log, "agent create").len(), 1, "{log}");
    assert_eq!(
        machine.fake_file("owner-login"),
        read(machine.state.join("owner-password"))
    );
}

#[test]
fn status_down_and_restart() {
    let machine = Machine::new("lifecycle");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    let config = machine.config(&vault_config(vault.port, "", ""));
    ok(&run(&machine, &config, &["up"]));

    let status = stdout(ok(&run(&machine, &config, &["status"])));
    assert!(
        status.starts_with(
            "hangar-tower: Running\nvault: healthy, unlisted hosts: deny\n\
             hangar-bay-default: Running\n"
        ),
        "{status}"
    );
    let vault_ui = format!("http://127.0.0.1:{} ", vault.port);
    let vault_ui = lines_with(&status, &vault_ui);
    assert!(vault_ui[0].starts_with("vault-ui "), "{status}");
    assert!(vault_ui[0].contains("  reachable  "), "{status}");
    assert!(
        status.contains("proxy     127.0.0.1:14322"),
        "the proxy isn't probed: {status}"
    );
    assert!(status.contains("-            the bays' only way out"));
    assert!(status.ends_with("all ports bind to 127.0.0.1 only\n"));

    let down = stderr(ok(&run(&machine, &config, &["down"])));
    assert_eq!(
        down,
        "==> stopping hangar-bay-default\n==> stopping hangar-tower\n"
    );
    let status = stdout(unhealthy(&run(&machine, &config, &["status"])));
    assert!(
        status.starts_with(
            "hangar-tower: Stopped\nvault: healthy, unlisted hosts: deny\n\
             hangar-bay-default: Stopped\n"
        ),
        "{status}"
    );
    assert_eq!(stderr(ok(&run(&machine, &config, &["down"]))), "");

    let up = stderr(ok(&run(&machine, &config, &["up"])));
    assert!(up.contains("==> starting hangar-tower"), "{up}");
    assert!(up.contains("==> starting hangar-bay-default\n"), "{up}");
}

#[test]
fn a_second_hangar_waits_before_touching_anything() {
    let machine = Machine::new("lock");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    let config = machine.config(&vault_config(vault.port, "", ""));
    let lock = machine.home.join(".local/state/hangar/lock");
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    let held = fs::File::create(&lock).unwrap();
    held.lock().unwrap();

    // The password comes from the keychain fake, so its first lookup and
    // store would race another hangar's without the lock.
    let mut up = machine.spawn(
        &["up"],
        &[
            ("HANGAR_CONFIG", config.to_str().unwrap()),
            ("HANGAR_STATE_DIR", machine.state.to_str().unwrap()),
            ("HANGAR_KEYCHAIN_SERVICE", "hangar-lock-test"),
        ],
    );
    let mut log = BufReader::new(up.stderr.take().unwrap());
    let mut first = String::new();
    log.read_line(&mut first).unwrap();
    assert_eq!(first, "==> waiting for another hangar\n");
    assert!(!machine.state.exists(), "nothing happens before the lock");
    assert_eq!(machine.msb_log(), "");
    assert_eq!(machine.fake_file("keychain.log"), "");

    drop(held);
    let mut rest = String::new();
    log.read_to_string(&mut rest).unwrap();
    assert!(up.wait().unwrap().success(), "{rest}");
    assert!(machine.state.exists());
    #[cfg(target_os = "macos")]
    let calls = "find-generic-password -s hangar-lock-test \
                 -a master-password -w\n-i\n";
    #[cfg(not(target_os = "macos"))]
    let calls = "lookup service hangar-lock-test key master-password\n\
                 store --label=hangar master password service \
                 hangar-lock-test key master-password\n";
    assert_eq!(machine.fake_file("keychain.log"), calls);
}

/// Two bays, `work` and `oss`, against the fake vault: no apps, so no
/// packages and no routes.
fn two_bays(vault_port: u16, oss: &str) -> String {
    format!(
        r#"{{"tower": {{"agentVault": {{"adminPort": {vault_port}}}}},
            "bays": [{{"name": "work", "image": "example/agent:1"}},
                     {{"name": "oss", "image": "example/agent:1"{oss}}}]}}"#
    )
}

#[test]
fn bays_come_up_apart_and_are_picked_by_name() {
    let machine = Machine::new("two-bays");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    let config = machine.config(&two_bays(vault.port, ""));

    let up = stderr(ok(&run(&machine, &config, &["up"])));
    assert!(up.contains("==> creating hangar-bay-work\n"), "{up}");
    assert!(up.contains("==> creating hangar-bay-oss\n"), "{up}");
    assert!(up.contains("==> ready: hangar shell --bay work\n"), "{up}");
    assert!(up.contains("==> ready: hangar shell --bay oss\n"), "{up}");
    for bay in ["work", "oss"] {
        assert!(machine.state.join(format!("bays/{bay}/vm")).exists());
        assert!(machine.state.join(format!("bays/{bay}/home")).is_dir());
        let token = machine.state.join(format!("agent-tokens/{bay}"));
        assert_eq!(mode(&token), 0o600);
        let create = lines_with(
            &machine.msb_log(),
            &format!("--name hangar-bay-{bay} "),
        )
        .into_iter()
        .find(|line| line.starts_with("create "))
        .unwrap()
        .to_string();
        let home = format!("bays/{bay}/home:/home/pilot:uid=1000,gid=1000 ");
        assert!(create.contains(&home), "{create}");
        assert!(
            create.contains(&format!(
                ".cache/hangar/bays/{bay}:/var/cache/hangar"
            ))
        );
    }
    let status = stdout(ok(&run(&machine, &config, &["status"])));
    let work = status.find("hangar-bay-work: Running").unwrap();
    let oss = status.find("hangar-bay-oss: Running").unwrap();
    assert!(work < oss, "config order: {status}");

    // One-bay commands want --bay once there are several.
    let error = failed(&run(&machine, &config, &["shell", "true"]));
    assert!(
        error.contains("several bays: pass --bay (work, oss)"),
        "{error}"
    );
    ok(&run(&machine, &config, &["shell", "--bay", "oss", "true"]));
    assert!(
        machine
            .msb_log()
            .contains("exec --no-tty --user pilot --workdir /home/pilot hangar-bay-oss -- sh -lc")
    );
    let error =
        failed(&run(&machine, &config, &["shell", "-b", "nope", "true"]));
    assert!(error.contains("no bay named nope"), "{error}");

    // `up oss` touches the tower and oss only; `down oss` keeps the tower.
    let before = machine.msb_log().len();
    ok(&run(&machine, &config, &["up", "oss"]));
    let log = machine.msb_log();
    let touched: Vec<&str> = changes(&log[before..])
        .into_iter()
        .filter(|line| line.contains("hangar-bay-work"))
        .collect();
    assert_eq!(touched, Vec::<&str>::new());
    ok(&run(&machine, &config, &["down", "oss"]));
    assert_eq!(read(machine.fake.join("vm-hangar-bay-oss")), "Stopped\n");
    assert_eq!(read(machine.fake.join("vm-hangar-tower")), "Running\n");
    assert_eq!(read(machine.fake.join("vm-hangar-bay-work")), "Running\n");
    unhealthy(&run(&machine, &config, &["status"]));
    let error = failed(&run(&machine, &config, &["up", "nope"]));
    assert!(
        error.contains("no bay named nope: bays: work, oss"),
        "{error}"
    );

    // destroy oss --state keeps work's home and every cache.
    fs::create_dir_all(machine.home.join(".cache/hangar/bays/oss")).unwrap();
    ok(&run(
        &machine,
        &config,
        &["destroy", "oss", "--state", "--yes"],
    ));
    assert!(!machine.fake.join("vm-hangar-bay-oss").exists());
    assert!(!machine.state.join("bays/oss").exists());
    assert!(machine.state.join("bays/work/home").is_dir());
    assert!(machine.home.join(".cache/hangar/bays/oss").is_dir());
    assert!(machine.fake.join("vm-hangar-tower").exists());
}

#[test]
fn a_bay_left_out_of_the_config_is_a_leftover_until_destroyed() {
    let machine = Machine::new("leftover");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    ok(&run(
        &machine,
        &machine.config(&two_bays(vault.port, "")),
        &["up"],
    ));
    let work_only = machine.config(&vault_config(vault.port, "", ""));
    let work_only = {
        let text = read(&work_only)
            .replace(r#""name": "default""#, r#""name": "work""#);
        machine.config(&text)
    };

    let up = stderr(ok(&run(&machine, &work_only, &["up"])));
    assert!(
        up.contains(
            "warning: bay oss is not in the config: hangar destroy oss\n"
        ),
        "{up}"
    );
    assert!(!machine.state.join("agent-tokens/oss").exists());
    assert!(
        vault
            .admin_requests()
            .iter()
            .any(|r| r.path == "/v1/agents/hangar-oss/delete")
    );
    let status = stdout(&run(&machine, &work_only, &["status"]));
    assert!(
        status.contains("hangar-bay-oss: Running (bay oss is not in the config: hangar destroy oss)"),
        "{status}"
    );
    let error = failed(&run(&machine, &work_only, &["up", "oss"]));
    assert!(error.contains("bay oss is not in the config"), "{error}");

    ok(&run(&machine, &work_only, &["destroy", "oss"]));
    let up = stderr(ok(&run(&machine, &work_only, &["up"])));
    assert!(
        up.contains("warning: bay oss is not in the config: hangar destroy oss --state\n"),
        "{up}"
    );
    ok(&run(
        &machine,
        &work_only,
        &["destroy", "oss", "--state", "--yes"],
    ));
    let up = stderr(ok(&run(&machine, &work_only, &["up"])));
    assert!(!up.contains("not in the config"), "{up}");
}

#[test]
fn a_failing_bay_does_not_stop_the_others() {
    let machine = Machine::new("bay-fails");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    // oss mounts hangar's own state: refused, so only oss fails.
    let mounts = format!(
        r#", "mounts": {{"~/data": {{"host": "{}"}}}}"#,
        machine.state.join("vault").display()
    );
    let config = machine.config(&two_bays(vault.port, &mounts));
    let up = run(&machine, &config, &["--json", "up"]);
    assert_eq!(up.status.code(), Some(1), "{}", stderr(&up));
    assert!(
        stderr(&up).contains("warning: bay oss: "),
        "{}",
        stderr(&up)
    );
    assert!(!stderr(&up).contains("--bay oss"), "no ready line for oss");
    let report = json_out(&up);
    assert_eq!(text(at(&report, &["failed", "0", "name"])), "oss");
    assert_eq!(text(at(&report, &["bays", "0", "name"])), "work");
    assert_eq!(text(at(&report, &["bays", "0", "state"])), "running");
    assert_eq!(text(at(&report, &["bays", "1", "state"])), "missing");
}

#[test]
fn a_credential_file_no_route_uses_is_warned_about() {
    let machine = Machine::new("unused-credential");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    let file = token_file(&machine, "unused", "dummy-unused-value");
    let config = machine.config(&tower_config(
        vault.port,
        &format!(r#", "credentialFiles": {{"UNUSED_KEY": "{file}"}}"#),
        "",
        "",
    ));
    let status = run(&machine, &config, &["status"]);
    assert!(
        stderr(&status).contains(
            "warning: credentialFiles UNUSED_KEY is used by no route\n"
        ),
        "{}",
        stderr(&status)
    );
}

#[test]
fn zero_routes_still_deny_and_send_an_empty_set() {
    let machine = Machine::new("no-routes");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    let config =
        machine.config(&vault_config(vault.port, "", r#", "apps": []"#));
    ok(&run(&machine, &config, &["up"]));
    let admin = vault.admin_requests();
    assert_eq!(admin[0].method, "PATCH");
    assert!(admin[0].body.contains(r#""unmatched_host_policy":"deny""#));
    assert_eq!(machine.fake_file("services.json"), r#"{"services":[]}"#);
}

#[test]
fn status_reports_missing_vms_and_an_unreachable_vault() {
    let machine = Machine::new("status-missing");
    machine.install_fake_msb();
    let config = machine.config(&format!(
        r#"{{"tower": {{"agentVault": {{"adminPort": {}}}}}}}"#,
        closed_port()
    ));
    let status = stdout(unhealthy(&run(&machine, &config, &["status"])));
    assert!(
        status.starts_with(
            "hangar-tower: missing\nvault: unreachable\n\
             hangar-bay-default: missing\nmount /home/pilot <- "
        ),
        "{status}"
    );
    assert!(
        status.contains(
            "/state/bays/default/home (rw, home)\nmount /var/cache/hangar <- "
        ),
        "{status}"
    );
    assert!(
        status.contains(
            "/.cache/hangar/bays/default (rw, cache)\npackage cache: 0 B ("
        ),
        "{status}"
    );
    assert!(
        status.contains("/.cache/hangar/bays/default)\nvault-ui "),
        "{status}"
    );
    assert!(!status.contains("run "), "no run entries without a VM");

    let status =
        json_out(unhealthy(&run(&machine, &config, &["status", "--json"])));
    assert_eq!(text(at(&status, &["tower", "vm"])), "missing");
    assert_eq!(text(at(&status, &["bays", "0", "state"])), "missing");
    assert!(!flag(at(&status, &["tower", "reachable"])));
    assert!(matches!(
        at(&status, &["tower", "unlistedHosts"]),
        miniserde::json::Value::Null
    ));
}

#[test]
fn status_shows_why_msb_failed() {
    let machine = Machine::new("status-broken");
    machine.install_fake_msb();
    machine.fail("inspect");
    let config = machine.config(&format!(
        r#"{{"tower": {{"agentVault": {{"adminPort": {}}}}}}}"#,
        closed_port()
    ));
    let status = stdout(unhealthy(&run(&machine, &config, &["status"])));
    assert!(
        status.starts_with(
            "hangar-tower: unknown (msb inspect hangar-tower: error: \
             permission denied)"
        ),
        "{status}"
    );
    let status =
        json_out(unhealthy(&run(&machine, &config, &["status", "--json"])));
    assert_eq!(text(at(&status, &["tower", "vm"])), "unknown");

    // An msb that can't be executed at all.
    machine.script("msb", "");
    fs::set_permissions(
        machine.home.join("bin/msb"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let status = stdout(unhealthy(&run(&machine, &config, &["status"])));
    assert!(status.contains("could not run msb"), "{status}");
}

#[test]
fn up_without_msb_says_so() {
    let machine = Machine::new("no-msb");
    let config = machine.config(
        r#"{"bays": [{"name": "default", "image": "example/agent:1"}]}"#,
    );
    let error = failed(&run(&machine, &config, &["up"]));
    assert!(error.contains("msb not found on PATH"), "{error}");
}

#[test]
fn removed_config_entries_are_removed_but_hand_added_ones_stay() {
    let machine = Machine::new("reconcile");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    let token = token_file(&machine, "gh-token", GITHUB_TOKEN);
    // Installed by hand in the VM: not hangar's to remove.
    fs::create_dir_all(machine.fake.join("profile")).unwrap();
    let both = machine.config(&tower_config(
        vault.port,
        &format!(r#", "credentialFiles": {{"GITHUB_TOKEN": "{token}"}}"#),
        "",
        r#", "packages": ["nixpkgs#rtk", "llm#codex"]"#,
    ));
    ok(&run(&machine, &both, &["up"]));
    // Installed by hand in the running VM: not hangar's to remove.
    fs::write(machine.fake.join("profile/htop"), "").unwrap();

    let only_rtk = machine.config(&vault_config(
        vault.port,
        "",
        r#", "packages": ["nixpkgs#rtk"]"#,
    ));
    let output = stderr(ok(&run(&machine, &only_rtk, &["up"])));
    assert!(output.contains("==> removing codex"), "{output}");

    let deletes: Vec<_> = vault
        .admin_requests()
        .into_iter()
        .filter(|r| r.method == "DELETE")
        .collect();
    assert_eq!(deletes.len(), 1);
    assert!(
        deletes[0]
            .body
            .contains(r#""keys":["GITHUB_GIT_USER","GITHUB_TOKEN"]"#),
        "{}",
        deletes[0].body
    );
    let profile = machine.fake.join("profile");
    assert!(profile.join("htop").exists());
    assert!(profile.join("rtk").exists());
    assert!(!profile.join("codex").exists());
    assert_eq!(machine.fake_file("tracked"), r#"{"nixpkgs#rtk":"rtk"}"#);
    assert_eq!(read(machine.state.join("credential-keys")), "\n");
}

#[test]
fn a_failed_registration_forgets_its_password_and_the_next_up_finishes() {
    let machine = Machine::new("register-retry");
    machine.install_fake_msb();
    machine.fail("register");
    let vault = FakeVault::start(&machine.fake);
    let config = machine.config(&vault_config(vault.port, "", ""));
    let error = failed(&run(&machine, &config, &["up"]));
    assert!(error.contains("register failed"), "{error}");
    assert!(!machine.state.join("owner-password").exists());
    assert!(machine.state.join("tower-vm").exists());

    fs::remove_file(machine.fake.join("fail-register")).unwrap();
    ok(&run(&machine, &config, &["up"]));
    assert_eq!(calls(&machine.msb_log(), "create "), 2);
}

#[test]
fn a_vault_that_keeps_allowing_unlisted_hosts_gets_no_routes_or_credentials() {
    let machine = Machine::new("deny-ignored");
    machine.install_fake_msb();
    machine.fail("deny");
    let vault = FakeVault::start(&machine.fake);
    let token = token_file(&machine, "gh-token", GITHUB_TOKEN);
    let config = machine.config(&tower_config(
        vault.port,
        &format!(r#", "credentialFiles": {{"GITHUB_TOKEN": "{token}"}}"#),
        "",
        "",
    ));
    let error = failed(&run(&machine, &config, &["up"]));
    assert!(error.contains("does not deny unlisted hosts"), "{error}");
    let requests: Vec<_> = vault
        .admin_requests()
        .into_iter()
        .map(|request| format!("{} {}", request.method, request.path))
        .collect();
    assert_eq!(
        requests,
        [
            "PATCH /v1/vaults/default/settings",
            "GET /v1/vaults/default/settings",
        ]
    );
    assert_eq!(machine.fake_file("services.json"), "");
}

#[test]
fn a_failing_bay_start_is_reported() {
    let machine = Machine::new("start-fails");
    machine.install_fake_msb();
    machine.fail("hangar-start");
    let vault = FakeVault::start(&machine.fake);
    let config = machine.config(&vault_config(vault.port, "", ""));
    let error = failed(&run(&machine, &config, &["up"]));
    assert!(
        error.contains(
            "msb exec hangar-bay-default: \
             /run/current-system/sw/bin/hangar-start"
        ),
        "{error}"
    );
}

/// Vault data and an agent token next to running VMs that have no
/// records of how they were created.
fn unrecorded_vms(machine: &Machine) {
    fs::create_dir_all(machine.state.join("vault/.agent-vault")).unwrap();
    fs::create_dir_all(machine.state.join("agent-tokens")).unwrap();
    fs::create_dir_all(machine.state.join("bays/default")).unwrap();
    for (file, contents) in [
        ("owner-password", "owner-1"),
        ("credential-keys", "GITHUB_TOKEN\n"),
        ("agent-tokens/default", "agent-token-0"),
    ] {
        fs::write(machine.state.join(file), contents).unwrap();
    }
    for vm in ["hangar-bay-default", "hangar-tower"] {
        fs::write(machine.fake.join(format!("vm-{vm}")), "Running").unwrap();
    }
}

/// msb calls that change a VM or run something in it.
fn changes(log: &str) -> Vec<&str> {
    log.lines()
        .filter(|line| !line.starts_with("inspect "))
        .collect()
}

#[test]
fn a_bay_vm_without_a_record_needs_only_destroy_and_up() {
    let machine = Machine::new("bay-unrecorded");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    unrecorded_vms(&machine);
    fs::write(
        machine.state.join("tower-vm"),
        format!(
            "port\tvault-ui\t{}\t14321\nport\tproxy\t14322\t14322\n",
            vault.port
        ),
    )
    .unwrap();
    let config = machine.config(&vault_config(vault.port, "", ""));

    // The bay's egress is unknown: refuse before touching it.
    let error = failed(&run(&machine, &config, &["up"]));
    assert!(
        error.contains(
            "bay default: bay has no record of how it was created: run \
             'hangar destroy default && hangar up default' (home is kept)"
        ),
        "{error}"
    );
    let log = machine.msb_log();
    let bay: Vec<&str> = changes(&log)
        .into_iter()
        .filter(|line| line.contains("hangar-bay-default"))
        .collect();
    assert_eq!(bay, Vec::<&str>::new());
    let status = stdout(&run(&machine, &config, &["status"]));
    assert!(status.contains("unknown (no record of the VM)"), "{status}");

    ok(&run(&machine, &config, &["destroy"]));
    ok(&run(&machine, &config, &["up"]));
    assert!(machine.state.join("bays/default/vm").exists());
    assert!(machine.state.join("tower-vm").exists());
    // The vault keeps its data, owner and agent token.
    assert_eq!(machine.fake_file("owner-login"), "owner-1");
    assert_eq!(
        read(machine.state.join("agent-tokens/default")),
        "agent-token-0"
    );
    assert_eq!(entries(&machine.state.join("guest")), ["ca.pem"]);
    assert_eq!(
        machine.fake_file("proxy-url"),
        "http://agent-token-0:default@host.microsandbox.internal:14322\n"
    );
    assert_eq!(
        calls(
            &machine.msb_log(),
            "exec --no-tty hangar-tower -- agent-vault agent"
        ),
        0
    );
}

#[test]
fn a_tower_vm_without_a_record_needs_only_destroy_and_up() {
    let machine = Machine::new("broker-unrecorded");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    unrecorded_vms(&machine);
    fs::write(
        machine.state.join("bays/default/vm"),
        "image\texample/agent:1\negress\t14322\n",
    )
    .unwrap();
    let config = machine.config(&vault_config(vault.port, "", ""));

    let error = failed(&run(&machine, &config, &["up"]));
    assert!(
        error.contains(
            "tower has no record of how it was created: run 'hangar \
             destroy && hangar up' (home is kept)"
        ),
        "{error}"
    );
    assert_eq!(changes(&machine.msb_log()), Vec::<&str>::new());

    ok(&run(&machine, &config, &["destroy"]));
    ok(&run(&machine, &config, &["up"]));
    assert_eq!(
        read(machine.state.join("tower-vm")),
        format!(
            "port\tvault-ui\t{}\t14321\nport\tproxy\t14322\t14322\n",
            vault.port
        )
    );
    // The existing vault data unlocks: same owner, same agent token.
    assert_eq!(machine.fake_file("owner-login"), "owner-1");
    assert_eq!(machine.fake_file("owner-register"), "");
    assert_eq!(
        read(machine.state.join("agent-tokens/default")),
        "agent-token-0"
    );
}

#[test]
fn a_changed_proxy_port_refuses_until_the_vm_is_recreated() {
    let machine = Machine::new("egress-drift");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    ok(&run(
        &machine,
        &machine.config(&vault_config(vault.port, "", "")),
        &["up"],
    ));
    let before = machine.msb_log();

    let moved = machine.config(&format!(
        r#"{{"tower": {{"agentVault": {{"adminPort": {}, "proxyPort": 15000}}}},
            "bays": [{{"name": "default", "image": "example/agent:1"}}]}}"#,
        vault.port
    ));
    let error = failed(&run(&machine, &moved, &["up"]));
    assert!(
        error.contains(
            "tower was created with proxy port 14322: run 'hangar \
             destroy && hangar up' (home is kept)"
        ),
        "{error}"
    );
    let after = machine.msb_log();
    assert_eq!(changes(&after[before.len()..]), Vec::<&str>::new());

    ok(&run(&machine, &moved, &["destroy"]));
    ok(&run(&machine, &moved, &["up"]));
    let log = machine.msb_log();
    assert!(
        log.contains("--no-net --net-rule allow@host:tcp:15000"),
        "{log}"
    );
}

fn mounts_config(vault_port: u16, mounts: &str) -> String {
    vault_config(vault_port, "", &format!(r#", "mounts": {{{mounts}}}"#))
}

#[test]
fn mounts_are_passed_when_the_vm_is_created_and_changes_warn() {
    let machine = Machine::new("mounts");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    fs::create_dir_all(machine.home.join("work/paperclip")).unwrap();
    fs::create_dir_all(machine.home.join("skills")).unwrap();
    let home = fs::canonicalize(&machine.home).unwrap();
    let config = machine.config(&mounts_config(
        vault.port,
        r#""~/.paperclip": {"host": "~/work/paperclip", "writable": true},
           "~/skills": {"host": "~/skills"}"#,
    ));
    let output = stderr(ok(&run(&machine, &config, &["up"])));
    assert!(!output.contains("warning"), "{output}");
    let log = machine.msb_log();
    let create =
        lines_with(&log, "create example/agent:1 --name hangar-bay-default ");
    assert_eq!(create.len(), 1, "{log}");
    let paperclip = format!(
        "--mount-dir {}:/home/pilot/.paperclip:uid=1000,gid=1000 ",
        home.join("work/paperclip").display()
    );
    let skills = format!(
        "--mount-dir {}:/home/pilot/skills:ro,uid=1000,gid=1000",
        home.join("skills").display()
    );
    assert!(create[0].contains(&paperclip), "{}", create[0]);
    assert!(create[0].contains(&skills), "{}", create[0]);

    let status = stdout(&run(&machine, &config, &["status", "--json"]));
    assert!(status.contains(r#""applied":true"#), "{status}");
    assert!(!status.contains(r#""applied":false"#), "{status}");

    // Read-only now: only a new VM picks that up.
    let changed = machine.config(&mounts_config(
        vault.port,
        r#""~/.paperclip": {"host": "~/work/paperclip"}"#,
    ));
    let output = stderr(ok(&run(&machine, &changed, &["up"])));
    assert!(
        output.contains(
            "warning: hangar-bay-default was created with different mounts; \
             run 'hangar destroy default && hangar up default' to apply"
        ),
        "{output}"
    );
    assert_eq!(calls(&machine.msb_log(), "create "), 2);
    let status = stdout(&run(&machine, &changed, &["status"]));
    assert!(
        status.contains(
            "(ro), not in the VM: hangar destroy default && hangar up default"
        ),
        "{status}"
    );
}

#[test]
fn unsafe_mount_sources_stop_up_before_anything_is_created() {
    let machine = Machine::new("mount-rules");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    fs::create_dir_all(machine.home.join(".ssh")).unwrap();
    std::os::unix::fs::symlink(
        machine.home.join(".ssh"),
        machine.home.join("keys"),
    )
    .unwrap();
    // The rules are mounts.rs's; here, a refused source is never created
    // and no VM either.
    for (mounts, why) in [
        (
            r#""~/x": {"host": "~/keys/new"}"#,
            "holds credentials or keys",
        ),
        (
            r#""~/x": {"host": "~/.config/new", "writable": true}"#,
            "is where host programs keep config",
        ),
    ] {
        let config = machine.config(&mounts_config(vault.port, mounts));
        let error = failed(&run(&machine, &config, &["up"]));
        assert!(error.contains(why), "{mounts}: {error}");
        let log = machine.msb_log();
        assert!(
            lines_with(&log, "--name hangar-bay-default ").is_empty(),
            "{mounts}: {log}"
        );
    }
    assert!(!machine.home.join(".ssh/new").exists());
    assert!(!machine.home.join(".config").exists());
}

#[test]
fn the_vm_home_is_kept_in_hangars_data_dir_and_packages_on_the_vm_disk() {
    use std::os::unix::fs::PermissionsExt as _;
    let machine = Machine::new("bay-home");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    fs::write(machine.home.join("CLAUDE.md"), "rules").unwrap();
    let config = machine.config(&vault_config(
        vault.port,
        "",
        r#", "packages": ["llm#codex"],
            "files": {"~/.claude/CLAUDE.md": "~/CLAUDE.md"}"#,
    ));
    let output = stderr(ok(&run(&machine, &config, &["up"])));
    let host = fs::canonicalize(&machine.state)
        .unwrap()
        .join("bays/default/home");
    assert!(
        output.contains(&format!("created {} for a mount", host.display())),
        "{output}"
    );
    let mode = fs::metadata(&host).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    let log = machine.msb_log();
    let create = lines_with(&log, "--name hangar-bay-default ");
    let home_mount = format!(
        "--mount-dir {}:/home/pilot:uid=1000,gid=1000 ",
        host.display()
    );
    assert!(create[0].contains(&home_mount), "{}", create[0]);
    // Copied files may land in the home mount.
    assert_eq!(
        machine.fake_file("vmfs/home/pilot/.claude/CLAUDE.md"),
        "rules"
    );
    // Packages go to hangar's profile on the VM disk, not ~/.nix-profile.
    let installs = lines_with(&log, "hangar-install");
    assert_eq!(installs.len(), 1, "{log}");
    assert!(
        installs[0].contains(
            "hangar-install /nix/var/nix/profiles/hangar \
             --extra-substituters file:///var/cache/hangar/nix?priority=10 \
             llm#codex"
        ),
        "{}",
        installs[0]
    );
    let status = stdout(&run(&machine, &config, &["status", "--json"]));
    assert!(status.contains(r#""home":true"#), "{status}");

    // A new VM: the home folder stays, /nix and the record start fresh, and
    // reconcile reinstalls instead of trusting a stale record.
    ok(&run(&machine, &config, &["destroy"]));
    assert!(host.exists());
    ok(&run(&machine, &config, &["up"]));
    assert_eq!(lines_with(&machine.msb_log(), "nix profile add").len(), 2);
    assert!(machine.fake.join("profile/codex").exists());
    assert_eq!(machine.fake_file("tracked"), r#"{"llm#codex":"codex"}"#);
}

#[test]
fn the_package_cache_is_mounted_tried_first_and_filled_after_installs() {
    let machine = Machine::new("bay-cache");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    let config = machine.config(&vault_config(
        vault.port,
        "",
        r#", "packages": ["llm#codex"]"#,
    ));
    ok(&run(&machine, &config, &["up"]));
    let host = fs::canonicalize(&machine.home)
        .unwrap()
        .join(".cache/hangar/bays/default");
    let log = machine.msb_log();
    let create = lines_with(&log, "--name hangar-bay-default ");
    let cache_mount =
        format!("--mount-dir {}:/var/cache/hangar ", host.display());
    assert!(create[0].contains(&cache_mount), "{}", create[0]);
    // Installs try the cache first; signatures stay required (no flag
    // anywhere trusts it).
    assert_eq!(
        machine.fake_file("substituters"),
        "file:///var/cache/hangar/nix?priority=10\n"
    );
    assert!(!machine.msb_log().contains("no-check-sigs"));
    assert!(!machine.msb_log().contains("trusted"));
    // Filled after the install, from hangar's profile.
    assert_eq!(
        machine.fake_file("cache-fills"),
        "file:///var/cache/hangar/nix?compression=zstd \
         /nix/var/nix/profiles/hangar\n"
    );

    // Nothing new to install: no second fill.
    ok(&run(&machine, &config, &["up"]));
    assert_eq!(machine.fake_file("cache-fills").lines().count(), 1);

    // A new VM installs again, through the cache.
    ok(&run(&machine, &config, &["destroy"]));
    fs::write(machine.fake.join("fail-cache"), "").unwrap();
    let output = stderr(ok(&run(&machine, &config, &["up"])));
    assert!(
        output.contains("warning: couldn't fill the package cache"),
        "{output}"
    );
    assert_eq!(machine.fake_file("substituters").lines().count(), 2);

    fs::write(host.join("blob"), vec![0u8; 2048]).unwrap();
    let status = stdout(&run(&machine, &config, &["status", "--json"]));
    assert!(status.contains(r#""cache":{"bytes":2048,"#), "{status}");
    assert!(status.contains(r#""cache":true"#), "{status}");

    // Off: no mount, no substituter, no fill.
    let off = machine.config(&vault_config(
        vault.port,
        "",
        r#", "packages": ["llm#rtk"], "cache": false"#,
    ));
    ok(&run(&machine, &off, &["destroy"]));
    ok(&run(&machine, &off, &["up"]));
    let log = machine.msb_log();
    let create = lines_with(&log, "--name hangar-bay-default ");
    assert!(
        !create.last().unwrap().contains(":/var/cache/hangar"),
        "{create:?}"
    );
    assert_eq!(machine.fake_file("substituters").lines().count(), 2);
    let status = stdout(&run(&machine, &off, &["status", "--json"]));
    assert!(status.contains(r#""cache":null"#), "{status}");
}

#[test]
fn the_package_cache_follows_xdg_cache_home() {
    let machine = Machine::new("bay-cache-xdg");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    let xdg = machine.home.join("xdg-cache");
    let with_xdg = |config: &Path, args: &[&str]| {
        machine.hangar(
            args,
            &[
                ("HANGAR_CONFIG", config.to_str().unwrap()),
                ("HANGAR_STATE_DIR", machine.state.to_str().unwrap()),
                ("HANGAR_MASTER_PASSWORD", PASSWORD),
                ("XDG_CACHE_HOME", xdg.to_str().unwrap()),
            ],
        )
    };
    let config = machine.config(&vault_config(vault.port, "", ""));
    ok(&with_xdg(&config, &["up"]));
    let host = fs::canonicalize(&xdg).unwrap().join("hangar/bays/default");
    assert!(host.is_dir());
    let log = machine.msb_log();
    let create = lines_with(&log, "--name hangar-bay-default ");
    let cache_mount =
        format!("--mount-dir {}:/var/cache/hangar ", host.display());
    assert!(create[0].contains(&cache_mount), "{}", create[0]);

    // Another bay's cache under the moved root stays refused.
    ok(&run(&machine, &config, &["destroy"]));
    let mount = r#""~/data": {"host": "~/xdg-cache/hangar/bays/other",
                               "writable": true}"#;
    let config = machine.config(&mounts_config(vault.port, mount));
    let error = stderr(&with_xdg(&config, &["up"]));
    assert!(error.contains("holds hangar's package caches"), "{error}");
}

#[test]
fn bay_home_can_be_turned_off_and_changes_warn() {
    let machine = Machine::new("bay-home-off");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    let off =
        machine.config(&vault_config(vault.port, "", r#", "home": false"#));
    ok(&run(&machine, &off, &["up"]));
    let log = machine.msb_log();
    let create = lines_with(&log, "--name hangar-bay-default ");
    assert!(!create[0].contains(":/home/pilot:"), "{}", create[0]);
    assert!(!machine.state.join("bays/default/home").exists());

    let on = machine.config(&vault_config(vault.port, "", ""));
    let output = stderr(ok(&run(&machine, &on, &["up"])));
    assert!(
        output.contains("hangar-bay-default was created with different mounts"),
        "{output}"
    );

    // A mount can't cover the home that home already mounts.
    let covering = machine.config(&mounts_config(
        vault.port,
        r#""/home/pilot": {"host": "~/elsewhere", "writable": true}"#,
    ));
    let error = failed(&run(&machine, &covering, &["status"]));
    assert!(error.contains("where the bay's home is mounted"), "{error}");
}

#[test]
fn a_path_is_either_copied_or_mounted() {
    let machine = Machine::new("mount-overlap");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    fs::create_dir_all(machine.home.join("work/paperclip")).unwrap();
    fs::write(machine.home.join("notes.md"), "notes").unwrap();
    let both = vault_config(
        vault.port,
        "",
        r#", "mounts": {"~/.paperclip": {"host": "~/work/paperclip"}},
            "files": {"~/.paperclip/notes.md": "~/notes.md"}"#,
    );
    let error = failed(&run(&machine, &machine.config(&both), &["status"]));
    assert!(error.contains("either copied or mounted"), "{error}");

    let config = machine.config(&mounts_config(
        vault.port,
        r#""~/.paperclip": {"host": "~/work/paperclip", "writable": true}"#,
    ));
    ok(&run(&machine, &config, &["up"]));
    let error = failed(&run(
        &machine,
        &config,
        &["copy", "~/notes.md", "~/.paperclip/notes.md"],
    ));
    assert!(
        error.contains("overlaps the mount at /home/pilot/.paperclip"),
        "{error}"
    );
}

#[test]
fn a_bay_boots_its_init_and_keeps_docker_on_its_own_disk() {
    let machine = Machine::new("init");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    let config = machine.config(&vault_config(vault.port, "", ""));
    ok(&run(&machine, &config, &["up"]));
    let log = machine.msb_log();
    let bay = lines_with(&log, "--name hangar-bay-default ");
    assert!(
        bay[0].contains(
            "--memory 6G --init /sbin/init --tmpfs /run --root-disk 40G \
             --mount-owned /var/lib/pilot:kind=disk,size=40G --no-net "
        ),
        "{log}"
    );
    assert!(!lines_with(&log, "--name hangar-tower ")[0].contains("--init"));
}

#[test]
fn a_failing_image_loader_is_reported() {
    let machine = Machine::new("image-loader-fails");
    machine.install_fake_msb();
    machine.script("load-image", "exit 3");
    let vault = FakeVault::start(&machine.fake);
    let loader = machine.home.join("bin/load-image");
    let config = machine.config(&vault_config(
        vault.port,
        "",
        &format!(r#", "imageLoader": "{}""#, loader.display()),
    ));
    let error = failed(&run(&machine, &config, &["up"]));
    assert!(error.contains("loading example/agent:1 failed"), "{error}");
}

#[test]
fn shell_runs_commands_with_their_arguments_intact() {
    let machine = Machine::new("shell");
    machine.install_fake_msb();
    let config = machine.config("{}");
    let output = run(&machine, &config, &["shell", "echo", "a b"]);
    assert_eq!(stdout(ok(&output)), "shell: echo a b\n");
    assert!(
        machine.msb_log().contains(
            r#"exec --no-tty --user pilot --workdir /home/pilot hangar-bay-default -- sh -lc exec "$@" sh echo a b"#
        ),
        "{}",
        machine.msb_log()
    );
    let output = run(&machine, &config, &["shell"]);
    assert_eq!(stdout(ok(&output)), "interactive shell\n");
    ok(&run(
        &machine,
        &config,
        &["shell", "claude", "--help", "-h"],
    ));
    assert!(machine.msb_log().contains("sh claude --help -h"));
}

#[test]
fn shell_without_msb_says_so() {
    let machine = Machine::new("shell-no-msb");
    let config = machine.config("{}");
    let error = failed(&run(&machine, &config, &["shell"]));
    assert!(error.contains("msb not found on PATH"), "{error}");
}

#[test]
fn destroy_removes_the_vms_and_with_state_everything() {
    let machine = Machine::new("destroy");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    let config = machine.config(&vault_config(vault.port, "", ""));
    ok(&run(&machine, &config, &["up"]));

    ok(&run(&machine, &config, &["destroy"]));
    let log = machine.msb_log();
    for vm in ["hangar-bay-default", "hangar-tower"] {
        let removal = format!("rm -f -q {vm}");
        assert!(log.lines().any(|line| line.trim_end() == removal), "{log}");
    }
    assert!(machine.state.join("owner-password").exists());
    // The password came from the environment: no keychain call yet.
    assert_eq!(machine.fake_file("keychain.log"), "");

    // Nothing left to remove: no more rm calls, and the state goes, with
    // the (fake) keychain item.
    ok(&run(&machine, &config, &["destroy", "--state", "--yes"]));
    assert_eq!(calls(&machine.msb_log(), "rm "), 2);
    assert!(!machine.state.exists());
    let service = format!("hangar-test-{}", std::process::id());
    #[cfg(target_os = "macos")]
    let deleted =
        format!("delete-generic-password -s {service} -a master-password\n");
    #[cfg(not(target_os = "macos"))]
    let deleted = format!("clear service {service} key master-password\n");
    assert_eq!(machine.fake_file("keychain.log"), deleted);

    let error = usage_error(&run(&machine, &config, &["destroy", "--force"]));
    assert!(error.contains("--force"), "{error}");
}

/// A broken msb is never mistaken for a missing VM.
#[test]
fn destroy_and_down_keep_everything_when_msb_fails() {
    let machine = Machine::new("destroy-fails");
    machine.install_fake_msb();
    fs::write(machine.fake.join("vm-hangar-bay-default"), "Stopped").unwrap();
    fs::create_dir_all(&machine.state).unwrap();
    let config = machine.config("{}");
    let destroy = ["destroy", "--state", "--yes"];
    for (fail, why) in [("rm", "rm refused"), ("inspect", "permission denied")]
    {
        machine.fail(fail);
        let error = failed(&run(&machine, &config, &destroy));
        assert!(error.contains(why), "{error}");
        assert!(machine.state.exists());
    }
    let error = failed(&run(&machine, &config, &["down"]));
    assert!(error.contains("permission denied"), "{error}");
}

#[test]
fn destroy_without_a_terminal_needs_yes_and_removes_nothing() {
    let machine = Machine::new("destroy-confirm");
    machine.install_fake_msb();
    fs::write(machine.fake.join("vm-hangar-bay-default"), "Stopped").unwrap();
    fs::create_dir_all(&machine.state).unwrap();
    let config = machine.config("{}");
    let error = failed(&run(&machine, &config, &["destroy", "--state"]));
    assert!(error.contains("--yes"), "{error}");
    assert!(machine.state.exists());
    assert!(machine.fake.join("vm-hangar-bay-default").exists());
}

#[test]
fn unreadable_run_states_are_unknown_and_unhealthy() {
    let machine = Machine::new("run-unknown");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    let config = machine.config(&vault_config(
        vault.port,
        "",
        r#", "run": {"paperclip": "paperclipai run"}"#,
    ));
    ok(&run(&machine, &config, &["up"]));

    machine.fail("run-status");
    let status = stdout(unhealthy(&run(&machine, &config, &["status"])));
    assert!(
        status.contains("run paperclip: unknown (msb exec"),
        "{status}"
    );

    fs::remove_file(machine.fake.join("fail-run-status")).unwrap();
    fs::write(machine.fake.join("garbled-status"), "").unwrap();
    let status = stdout(unhealthy(&run(&machine, &config, &["status"])));
    assert!(
        status.contains(r#"run paperclip: unknown (unexpected "zombie")"#),
        "{status}"
    );

    machine.fail("shell");
    let error = failed(&run(&machine, &config, &["logs", "paperclip"]));
    assert!(error.contains("no output for paperclip yet"), "{error}");
}

#[test]
fn a_vault_server_that_cannot_start_stops_up() {
    let machine = Machine::new("vault-server");
    machine.install_fake_msb();
    machine.fail("server");
    let config = machine.config(&vault_config(closed_port(), "", ""));
    let error = failed(&run(&machine, &config, &["up"]));
    assert!(error.contains("server did not start"), "{error}");
    assert!(!machine.fake.join("vm-hangar-bay-default").exists());
}

#[test]
fn msb_never_sees_the_master_password() {
    let machine = Machine::new("env");
    machine.script("msb", r#"env >>"$HANGAR_FAKE/msb-env"; exit 1"#);
    let config = machine.config(
        r#"{"bays": [{"name": "default", "image": "example/agent:1"}]}"#,
    );
    let error = failed(&run(&machine, &config, &["up"]));
    assert!(error.contains("msb inspect"), "{error}");
    let seen = machine.fake_file("msb-env");
    assert!(seen.contains("HOME="), "msb was not called");
    assert!(!seen.contains(PASSWORD));
}

#[test]
fn user_credentials_go_to_the_vault_only_and_survive_up() {
    let machine = Machine::new("credential");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    let token = token_file(&machine, "gh", GITHUB_TOKEN);
    let config = machine.config(&tower_config(
        vault.port,
        &format!(r#", "credentialFiles": {{"GITHUB_TOKEN": "{token}"}}"#),
        "",
        "",
    ));
    ok(&run(&machine, &config, &["up"]));

    let value = "oauth-token-value";
    let set = ["credential", "set", "CLAUDE_CODE_OAUTH_TOKEN"];
    let stored = run_with_stdin(&machine, &config, &set, &format!("{value}\n"));
    assert_eq!(stderr(ok(&stored)), "==> stored CLAUDE_CODE_OAUTH_TOKEN\n");
    let posted: Vec<_> = vault
        .admin_requests()
        .into_iter()
        .filter(|r| r.method == "POST" && r.body.contains("CLAUDE_CODE"))
        .collect();
    assert_eq!(posted.len(), 1);
    assert!(
        posted[0]
            .body
            .contains(&format!(r#""CLAUDE_CODE_OAUTH_TOKEN":"{value}""#))
    );
    assert!(!machine.msb_log().contains(value), "value in argv");
    assert!(!read(machine.state.join("credential-keys")).contains("CLAUDE"));

    let list = ["credential", "list"];
    assert_eq!(
        stdout(ok(&run(&machine, &config, &list))),
        "CLAUDE_CODE_OAUTH_TOKEN\tuser\nGITHUB_GIT_USER\tconfig\n\
         GITHUB_TOKEN\tconfig\n"
    );
    // `up` reconciles only the config's credentials.
    ok(&run(&machine, &config, &["up"]));
    let after = stdout(ok(&run(&machine, &config, &list)));
    assert!(after.contains("CLAUDE_CODE_OAUTH_TOKEN\tuser"), "{after}");

    let managed = ["credential", "set", "GITHUB_TOKEN"];
    let error = failed(&run_with_stdin(&machine, &config, &managed, "x\n"));
    assert!(error.contains("managed by"), "{error}");
    let rm_managed = ["credential", "rm", "GITHUB_GIT_USER"];
    let error = failed(&run(&machine, &config, &rm_managed));
    assert!(error.contains("set by app github-token"), "{error}");
    let empty = run_with_stdin(&machine, &config, &set, "\n");
    assert!(failed(&empty).contains("empty value"));

    let rm = ["credential", "rm", "CLAUDE_CODE_OAUTH_TOKEN"];
    let removed = stderr(ok(&run(&machine, &config, &rm)));
    assert_eq!(removed, "==> removed CLAUDE_CODE_OAUTH_TOKEN\n");
    let after = stdout(ok(&run(&machine, &config, &list)));
    assert!(!after.contains("CLAUDE"), "{after}");
}

#[test]
fn credential_commands_check_names_and_need_a_running_vault() {
    let machine = Machine::new("credential-down");
    machine.install_fake_msb();
    let config = machine.config(&vault_config(closed_port(), "", ""));
    let set = ["credential", "set", "TOKEN"];
    let error = failed(&run_with_stdin(&machine, &config, &set, "value\n"));
    assert!(error.contains("run 'hangar up' first"), "{error}");
    let bad = ["credential", "set", "not-a-key"];
    let error = failed(&run_with_stdin(&machine, &config, &bad, "value\n"));
    assert!(error.contains("UPPER_SNAKE_CASE"), "{error}");
    let error = failed(&run(&machine, &config, &["credential", "list"]));
    assert!(error.contains("run 'hangar up' first"), "{error}");
    let error = usage_error(&run(&machine, &config, &["credential"]));
    assert!(error.contains("COMMAND"), "{error}");
}

#[test]
fn bay_env_and_run_entries_reach_the_bay() {
    let machine = Machine::new("bay-run");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    let config = machine.config(&vault_config(
        vault.port,
        "",
        r#", "env": {"CLAUDE_CODE_OAUTH_TOKEN": "placeholder",
                     "QUOTED": "it's $HOME"},
            "run": {"paperclip": "paperclipai run"}"#,
    ));
    let up = run(&machine, &config, &["up"]);
    let progress = stderr(ok(&up));
    assert!(
        progress.contains("started paperclip: hangar logs paperclip"),
        "{progress}"
    );
    // The port overview is the command's output; progress is diagnostics.
    let output = stdout(&up);
    assert!(
        output.contains("all ports bind to 127.0.0.1 only"),
        "{output}"
    );
    assert!(!output.contains("==>"), "{output}");
    assert_eq!(
        machine.fake_file("bay-env"),
        "CLAUDE_CODE_OAUTH_TOKEN='placeholder'\n\
         QUOTED='it'\\''s $HOME'\n"
    );
    assert_eq!(machine.fake_file("run-paperclip"), "paperclipai run");

    let again = stderr(ok(&run(&machine, &config, &["up"])));
    assert!(!again.contains("started paperclip"), "{again}");
    let status = stdout(ok(&run(&machine, &config, &["status"])));
    assert!(status.contains("run paperclip: running"), "{status}");

    let logs = stdout(ok(&run(&machine, &config, &["logs", "paperclip"])));
    assert!(
        logs.contains("shell: tail -n 100 /var/log/hangar/paperclip.log"),
        "{logs}"
    );
    let follow = ["logs", "paperclip", "-f"];
    let logs = stdout(ok(&run(&machine, &config, &follow)));
    assert!(logs.contains("tail -n 100 -f /var/log/hangar"), "{logs}");
    let error = failed(&run(&machine, &config, &["logs", "nope"]));
    assert!(error.contains("not a run entry or app"), "{error}");
    let error = usage_error(&run(&machine, &config, &["logs"]));
    assert!(error.contains("NAME"), "{error}");
}

#[test]
fn vault_ui_copies_the_login_without_printing_it() {
    let machine = Machine::new("vault-ui");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    let config = machine.config(&vault_config(vault.port, "", ""));
    let fake = machine.fake.display().to_string();
    let error = failed(&run(&machine, &config, &["vault-ui"]));
    assert!(error.contains("run 'hangar up' first"), "{error}");
    ok(&run(&machine, &config, &["up"]));

    machine.script("pbcopy", &format!("cat >{fake}/clipboard"));
    machine.script("open", &format!(r#"echo "$1" >{fake}/opened"#));
    machine.script("xdg-open", "exit 1");
    let output = stdout(ok(&run(&machine, &config, &["vault-ui"])));
    let owner = read(machine.state.join("owner-password"));
    let url = format!("http://127.0.0.1:{}", vault.port);
    assert!(!output.contains(&owner), "password printed:\n{output}");
    assert!(output.contains("owner@hangar.local"), "{output}");
    assert!(output.contains(&url), "{output}");
    assert!(output.contains("copied to the clipboard"), "{output}");
    assert_eq!(machine.fake_file("clipboard"), owner);
    assert_eq!(machine.fake_file("opened").trim(), url);

    for tool in ["pbcopy", "wl-copy", "xclip"] {
        machine.script(tool, "exit 1");
    }
    let output = stdout(ok(&run(&machine, &config, &["vault-ui"])));
    assert!(output.contains("(not shown)"), "{output}");
    assert!(!output.contains(&owner), "password printed:\n{output}");
    let error = usage_error(&run(&machine, &config, &["vault-ui", "x"]));
    assert!(error.contains("'x'"), "{error}");
}

#[test]
fn log_levels_come_from_flags_or_hangar_log() {
    let machine = Machine::new("log-levels");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    let config = machine.config(&vault_config(vault.port, "", ""));

    let quiet = stderr(ok(&run(&machine, &config, &["-q", "up"])));
    assert_eq!(quiet, "", "-q prints only errors");
    let after = stderr(ok(&run(&machine, &config, &["up", "-q"])));
    assert_eq!(after, "", "global flags work after the command too");
    let info = stderr(ok(&run(&machine, &config, &["up"])));
    assert!(info.contains("==> ready"), "{info}");
    assert!(!info.contains("debug:"), "{info}");
    let debug = stderr(ok(&run(&machine, &config, &["-v", "up"])));
    assert!(debug.contains("debug: run: msb "), "{debug}");
    assert!(!debug.contains("trace:"), "{debug}");
    let trace = stderr(ok(&run(&machine, &config, &["-vv", "up"])));
    assert!(trace.contains("trace: vault PATCH "), "{trace}");

    let env = |level| {
        let output = machine.hangar(
            &["status"],
            &[
                ("HANGAR_CONFIG", config.to_str().unwrap()),
                ("HANGAR_STATE_DIR", machine.state.to_str().unwrap()),
                ("HANGAR_LOG", level),
            ],
        );
        stderr(ok(&output))
    };
    assert!(env("debug").contains("debug: run: msb "));
    assert!(!env("error").contains("debug:"));

    // Command output on stdout, diagnostics on stderr.
    let status = run(&machine, &config, &["-v", "status"]);
    assert!(stdout(ok(&status)).starts_with("hangar-tower: Running\n"));
    assert!(!stdout(&status).contains("debug:"));
    assert!(!stderr(&status).contains("hangar-tower: Running"));
}

#[test]
fn no_secret_reaches_any_log_level() {
    let machine = Machine::new("no-secret-logs");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    let token = token_file(&machine, "gh", GITHUB_TOKEN);
    let config = machine.config(&tower_config(
        vault.port,
        &format!(r#", "credentialFiles": {{"GITHUB_TOKEN": "{token}"}}"#),
        "",
        "",
    ));
    let mut printed = String::new();
    for args in [
        &["-vv", "up"][..],
        &["-vv", "up", "--json"],
        &["-vv", "status"],
        &["-vv", "status", "--json"],
        &["-vv", "credential", "list", "--json"],
        &["-vv", "vault-ui", "--json"],
    ] {
        let output = run(&machine, &config, args);
        printed.push_str(&stdout(ok(&output)));
        printed.push_str(&stderr(&output));
    }
    let value = "user-credential-value";
    let set = ["-vv", "credential", "set", "USER_TOKEN"];
    let output = run_with_stdin(&machine, &config, &set, value);
    printed.push_str(&stderr(ok(&output)));
    printed.push_str(&stdout(&output));

    assert!(printed.contains("trace: vault "), "trace was off");
    let owner = read(machine.state.join("owner-password"));
    let agent_token = read(machine.state.join("agent-tokens/default"));
    for secret in [PASSWORD, GITHUB_TOKEN, value, &owner, &agent_token] {
        assert!(!printed.contains(secret.trim()), "secret in the logs");
    }
}

/// The single JSON document a `--json` command prints on stdout.
fn json_out(output: &Output) -> miniserde::json::Value {
    let text = stdout(output);
    assert_eq!(text.lines().count(), 1, "one JSON line: {text}");
    miniserde::json::from_str(&text).unwrap()
}

/// Follows `path` through nested objects (array items by index).
fn at<'a>(
    value: &'a miniserde::json::Value,
    path: &[&str],
) -> &'a miniserde::json::Value {
    use miniserde::json::Value;
    path.iter().fold(value, |value, key| match value {
        Value::Object(map) => &map[*key],
        Value::Array(items) => &items[key.parse::<usize>().unwrap()],
        other => panic!("no {key} in {other:?}"),
    })
}

fn text(value: &miniserde::json::Value) -> &str {
    match value {
        miniserde::json::Value::String(text) => text,
        other => panic!("not a string: {other:?}"),
    }
}

fn flag(value: &miniserde::json::Value) -> bool {
    match value {
        miniserde::json::Value::Bool(flag) => *flag,
        other => panic!("not a bool: {other:?}"),
    }
}

/// An app with a host, a run entry and a port nothing listens on.
fn web_app(port: u16) -> String {
    format!(
        r#", "appDefinitions": {{"web": {{
             "routes": [{{"name": "web-api", "host": "api.web.example",
               "auth": {{"type": "bearer", "token": "WEB_TOKEN"}}}}],
             "run": "serve",
             "ports": [{{"name": "web", "vm": 3100, "host": {port},
                         "purpose": "Web UI"}}]}}}}"#
    )
}

#[test]
fn an_app_runs_only_once_its_setup_check_passes() {
    let machine = Machine::new("app-setup");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    let web = r#", "appDefinitions": {"web": {
        "setup": {"command": "touch ~/.web/ready",
                  "check": "test -f ~/.web/ready"},
        "run": "serve"}}"#;
    let config =
        machine.config(&vault_config(vault.port, web, r#", "apps": ["web"]"#));

    let up = stderr(ok(&run(&machine, &config, &["up"])));
    assert!(
        up.contains("web needs setup: run 'hangar setup web'"),
        "{up}"
    );
    assert!(!up.contains("started web"), "{up}");
    let status = stdout(unhealthy(&run(&machine, &config, &["status"])));
    assert!(
        status.contains("run web: stopped (needs setup? hangar setup web)"),
        "{status}"
    );

    machine.fail("setup");
    let error = failed(&run(&machine, &config, &["setup", "web"]));
    assert!(
        error.contains("web's setup failed: touch ~/.web/ready"),
        "{error}"
    );
    fs::remove_file(machine.fake.join("fail-setup")).unwrap();

    let setup = run(&machine, &config, &["setup", "web"]);
    assert_eq!(stdout(ok(&setup)), "setup: touch ~/.web/ready\n");
    assert!(
        stderr(&setup).contains("run 'hangar up' to start web"),
        "{}",
        stderr(&setup)
    );
    let log = machine.msb_log();
    assert_eq!(
        lines_with(&log, "exec -t --user pilot --workdir /home/pilot hangar-bay-default -- sh -lc")
            .len(),
        2,
        "{log}"
    );

    let again = stderr(ok(&run(&machine, &config, &["setup", "web"])));
    assert!(again.contains("web is already set up"), "{again}");
    ok(&run(&machine, &config, &["setup", "--force", "web"]));
    let log = machine.msb_log();
    assert_eq!(
        lines_with(&log, "exec -t --user pilot --workdir /home/pilot hangar-bay-default -- sh -lc")
            .len(),
        3,
        "{log}"
    );

    let up = stderr(ok(&run(&machine, &config, &["up"])));
    assert!(!up.contains("needs setup"), "{up}");
    assert!(up.contains("started web"), "{up}");

    let error = failed(&run(&machine, &config, &["setup", "nope"]));
    assert!(error.contains("nope is not an enabled app"), "{error}");
    let error = usage_error(&run(&machine, &config, &["setup"]));
    assert!(error.contains("NAME"), "{error}");
}

#[test]
fn json_status_and_up_follow_the_documented_schema() {
    let machine = Machine::new("json-status");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    let web = closed_port();
    let config = machine.config(&vault_config(
        vault.port,
        &web_app(web),
        r#", "apps": ["web"]"#,
    ));

    let up = run(&machine, &config, &["--json", "up"]);
    let report = json_out(ok(&up));
    assert!(stderr(&up).contains("==> creating hangar"), "progress");
    assert!(matches!(
        at(&report, &["version"]),
        miniserde::json::Value::Number(miniserde::json::Number::U64(1))
    ));
    assert!(flag(at(&report, &["healthy"])));
    assert_eq!(text(at(&report, &["tower", "vm"])), "running");
    assert_eq!(text(at(&report, &["bays", "0", "state"])), "running");
    assert_eq!(text(at(&report, &["tower", "backend"])), "agent-vault");
    assert!(flag(at(&report, &["tower", "reachable"])));
    assert!(flag(at(&report, &["tower", "healthy"])));
    assert_eq!(text(at(&report, &["tower", "unlistedHosts"])), "deny");
    let host = at(&report, &["bays", "0", "apps", "web", "routes", "0"]);
    assert_eq!(text(at(host, &["name"])), "web-api");
    assert_eq!(text(at(host, &["host"])), "api.web.example");
    assert_eq!(text(at(&report, &["bays", "0", "name"])), "default");
    assert!(
        matches!(at(&report, &["failed"]), miniserde::json::Value::Array(failed) if failed.is_empty())
    );
    assert!(
        matches!(at(&report, &["leftovers"]), miniserde::json::Value::Array(left) if left.is_empty())
    );
    assert_eq!(text(at(host, &["credential"])), "WEB_TOKEN");
    assert_eq!(text(at(&report, &["bays", "0", "run", "web"])), "running");
    // Probed for real: the vault answers, nothing listens on the app port.
    assert!(flag(at(&report, &["tower", "ports", "0", "reachable"])));
    assert!(!flag(at(
        &report,
        &["bays", "0", "ports", "0", "reachable"]
    )));
    let human = stdout(ok(&run(&machine, &config, &["up"])));
    assert!(
        human.starts_with("app web: api.web.example ← WEB_TOKEN\nvault-ui "),
        "{human}"
    );

    // `-v --json` and `status --json -v`: flags work on either side.
    let status = run(&machine, &config, &["status", "--json"]);
    assert!(flag(at(&json_out(ok(&status)), &["healthy"])));

    // A stopped run entry makes status unhealthy: exit 3.
    fs::remove_file(machine.fake.join("run-web")).unwrap();
    let status =
        json_out(unhealthy(&run(&machine, &config, &["status", "--json"])));
    assert!(!flag(at(&status, &["healthy"])));
    assert_eq!(text(at(&status, &["bays", "0", "run", "web"])), "stopped");
    let human = stdout(unhealthy(&run(&machine, &config, &["status"])));
    assert!(human.contains("app web: api.web.example ← WEB_TOKEN\n"));
    assert!(human.contains("run web: stopped"), "{human}");

    // Stopped VMs too; their run entries count as stopped.
    ok(&run(&machine, &config, &["--json", "down"]));
    let status =
        json_out(unhealthy(&run(&machine, &config, &["status", "--json"])));
    assert_eq!(text(at(&status, &["tower", "vm"])), "stopped");
    assert_eq!(text(at(&status, &["bays", "0", "state"])), "stopped");
    assert_eq!(text(at(&status, &["bays", "0", "run", "web"])), "stopped");
}

#[test]
fn json_errors_are_one_object_with_a_hint() {
    let machine = Machine::new("json-errors");
    machine.install_fake_msb();
    let config = machine.config(&format!(
        r#"{{"tower": {{"agentVault": {{"adminPort": {}}}}}}}"#,
        closed_port()
    ));

    let human = failed(&run(&machine, &config, &["credential", "list"]));
    assert_eq!(
        human,
        "error: the vault isn't running: run 'hangar up' first\n"
    );

    let output = run(&machine, &config, &["--json", "credential", "list"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    let error: miniserde::json::Value =
        miniserde::json::from_str(stderr(&output).trim()).unwrap();
    assert_eq!(text(at(&error, &["error"])), "the vault isn't running");
    assert_eq!(text(at(&error, &["hint"])), "run 'hangar up' first");

    // Without an obvious next step, the hint is null.
    let bad = machine.config(r#"{"agnet": {}}"#);
    let output = run(&machine, &bad, &["status", "--json"]);
    let error: miniserde::json::Value =
        miniserde::json::from_str(stderr(&output).trim()).unwrap();
    assert!(
        text(at(&error, &["error"])).contains("agnet"),
        "names the key"
    );
    assert!(matches!(
        at(&error, &["hint"]),
        miniserde::json::Value::Null
    ));
}

/// What a `--json` command without output of its own prints.
const ACK: &str = "{\"ok\":true,\"version\":1}\n";

#[test]
fn json_commands_without_output_acknowledge_success() {
    let machine = Machine::new("json-ok");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    let token = token_file(&machine, "gh", GITHUB_TOKEN);
    let config = machine.config(&tower_config(
        vault.port,
        &format!(r#", "credentialFiles": {{"GITHUB_TOKEN": "{token}"}}"#),
        "",
        "",
    ));
    ok(&run(&machine, &config, &["up"]));

    let set = ["--json", "credential", "set", "USER_TOKEN"];
    let output = run_with_stdin(&machine, &config, &set, "user-value");
    assert_eq!(stdout(ok(&output)), ACK);

    let rm = ["--json", "credential", "rm", "USER_TOKEN"];
    assert_eq!(stdout(ok(&run(&machine, &config, &rm))), ACK);

    let ui = json_out(ok(&run(&machine, &config, &["--json", "vault-ui"])));
    assert_eq!(text(at(&ui, &["login"])), "owner@hangar.local");
    assert_eq!(
        text(at(&ui, &["url"])),
        format!("http://127.0.0.1:{}", vault.port)
    );
    assert_eq!(text(at(&ui, &["password"])), "file");
    let owner = read(machine.state.join("owner-password"));
    assert!(
        !stdout(&run(&machine, &config, &["--json", "vault-ui"]))
            .contains(owner.trim())
    );

    let down = run(&machine, &config, &["--json", "down"]);
    assert_eq!(stdout(ok(&down)), ACK);
    let destroy = ["--json", "destroy", "--state", "--yes"];
    assert_eq!(stdout(ok(&run(&machine, &config, &destroy))), ACK);
}

#[test]
fn every_command_has_help_with_examples() {
    let machine = Machine::new("help-examples");
    let help = stdout(ok(&machine.hangar(&["--help"], &[])));
    assert!(
        help.contains("Exit codes: 0 ok, 1 error, 2 usage, 3"),
        "{help}"
    );
    assert!(help.contains("docs/cli.md"), "{help}");
    assert!(help.contains("--json"), "{help}");
    for (command, example) in [
        (&["init"][..], "hangar setup"),
        (&["up"], "hangar up --json"),
        (&["down"], "the tower keeps running"),
        (&["status"], "hangar status --json"),
        (&["shell"], "hangar shell gh --help"),
        (&["logs"], "hangar logs -f web"),
        (&["vault-ui"], "hangar vault-ui --json"),
        (&["credential"], "List credential names"),
        (&["credential", "set"], "< token.txt"),
        (&["credential", "list"], "credential list --json"),
        (&["credential", "rm"], "hangar credential rm"),
        (&["destroy"], "hangar destroy --state --yes"),
    ] {
        let mut args = command.to_vec();
        args.push("--help");
        let help = stdout(ok(&machine.hangar(&args, &[])));
        let usage = format!("Usage: hangar {}", command.join(" "));
        assert!(help.contains(&usage), "{help}");
        assert!(help.contains(example), "{command:?}: {help}");
    }
    let version = stdout(ok(&machine.hangar(&["--version"], &[])));
    assert!(version.contains(env!("CARGO_PKG_VERSION")), "{version}");
}

#[test]
fn bay_files_are_copied_reconciled_and_never_sent_as_arguments() {
    let machine = Machine::new("bay-files");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    let claude = machine.home.join("dotfiles/claude");
    fs::create_dir_all(claude.join("agents")).unwrap();
    fs::write(claude.join("CLAUDE.md"), "MARKER-claude-md v1").unwrap();
    fs::write(claude.join("agents/review.md"), "MARKER-agent").unwrap();
    fs::write(claude.join("agents/tool.sh"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(
        claude.join("agents/tool.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let both = r#", "files": {
        "~/.claude/CLAUDE.md": "~/dotfiles/claude/CLAUDE.md",
        "~/.claude/agents": "~/dotfiles/claude/agents"}"#;
    let config = machine.config(&vault_config(vault.port, "", both));
    let vm = |path: &str| {
        machine.fake.join(format!("vmfs/home/pilot/.claude/{path}"))
    };

    let progress = stderr(ok(&run(&machine, &config, &["up"])));
    assert!(
        progress.contains("copying 3 file(s) into hangar"),
        "{progress}"
    );
    assert_eq!(read(vm("CLAUDE.md")), "MARKER-claude-md v1");
    assert_eq!(read(vm("agents/review.md")), "MARKER-agent");
    assert_eq!(mode(&vm("agents/tool.sh")), 0o755);
    assert_eq!(mode(&vm("CLAUDE.md")), 0o644);
    // Contents travel on stdin only.
    assert!(
        !machine.msb_log().contains("MARKER"),
        "{}",
        machine.msb_log()
    );
    let record = machine.state.join("bays/default/files");
    assert_eq!(mode(&record), 0o600);

    // Unchanged files aren't rewritten; a changed one is.
    let writes = |m: &Machine| lines_with(&m.msb_log(), "hangar-file").len();
    let before = writes(&machine);
    let again = stderr(ok(&run(&machine, &config, &["up"])));
    assert!(!again.contains("copying"), "{again}");
    assert_eq!(writes(&machine), before);
    fs::write(claude.join("CLAUDE.md"), "MARKER-claude-md edited").unwrap();
    ok(&run(&machine, &config, &["up"]));
    assert_eq!(read(vm("CLAUDE.md")), "MARKER-claude-md edited");
    assert_eq!(writes(&machine), before + 1);

    // A dropped entry is removed from the VM; a file hangar didn't copy
    // stays.
    fs::write(vm("by-hand.md"), "mine").unwrap();
    let one =
        r#", "files": {"~/.claude/CLAUDE.md": "~/dotfiles/claude/CLAUDE.md"}"#;
    let config = machine.config(&vault_config(vault.port, "", one));
    let progress = stderr(ok(&run(&machine, &config, &["up"])));
    assert!(
        progress.contains("removing /home/pilot/.claude/agents/review.md"),
        "{progress}"
    );
    assert!(!vm("agents/review.md").exists());
    assert!(!vm("agents/tool.sh").exists());
    assert!(vm("CLAUDE.md").exists());
    assert_eq!(read(vm("by-hand.md")), "mine");

    // A new VM gets every file again, despite the record.
    ok(&run(&machine, &config, &["destroy"]));
    fs::remove_dir_all(machine.fake.join("vmfs")).unwrap();
    let progress = stderr(ok(&run(&machine, &config, &["up"])));
    assert!(progress.contains("copying 1 file(s)"), "{progress}");
    assert!(vm("CLAUDE.md").exists());
}

#[test]
fn bay_files_refuse_secrets_before_copying_anything() {
    let machine = Machine::new("bay-files-secrets");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    fs::create_dir_all(machine.home.join(".claude")).unwrap();
    fs::write(machine.home.join(".claude/CLAUDE.md"), "fine").unwrap();
    fs::write(machine.home.join(".claude/.credentials.json"), "{}").unwrap();
    let files = r#", "files": {"~/.claude": "~/.claude"}"#;
    let config = machine.config(&vault_config(vault.port, "", files));

    let error = failed(&run(&machine, &config, &["up"]));
    assert!(
        error.contains(".credentials.json: looks like a credentials file"),
        "{error}"
    );
    assert!(error.contains("hangar credential set"), "{error}");
    let copies = lines_with(&machine.msb_log(), "hangar-file").len();
    assert_eq!(copies, 0);
    assert!(!machine.fake.join("vmfs").exists());

    let bad = r#", "files": {"/run/hangar/x": "~/.claude/CLAUDE.md"}"#;
    let config = machine.config(&vault_config(vault.port, "", bad));
    let error = failed(&run(&machine, &config, &["status"]));
    assert!(
        error.contains("config.bays.default.files./run/hangar/x"),
        "{error}"
    );
}

fn json_line(output: &Output) -> String {
    stdout(ok(output)).trim().to_string()
}

#[test]
fn copy_applies_declared_files_or_one_source_with_the_same_checks() {
    let machine = Machine::new("copy");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    fs::create_dir_all(machine.home.join("dotfiles")).unwrap();
    fs::write(machine.home.join("dotfiles/CLAUDE.md"), "v1").unwrap();
    let files = r#", "files": {"~/.claude/CLAUDE.md": "~/dotfiles/CLAUDE.md"}"#;
    let config = machine.config(&vault_config(vault.port, "", files));
    let vm = |path: &str| machine.fake.join(format!("vmfs{path}"));

    let error = failed(&run(&machine, &config, &["copy"]));
    assert!(
        error.contains(
            "hangar-bay-default isn't running: run 'hangar up' first"
        ),
        "{error}"
    );
    ok(&run(&machine, &config, &["up"]));

    fs::write(machine.home.join("dotfiles/CLAUDE.md"), "edited").unwrap();
    let copied = json_line(&run(&machine, &config, &["--json", "copy"]));
    assert_eq!(
        copied,
        r#"{"bay":"default","copied":["/home/pilot/.claude/CLAUDE.md"],"ok":true,"removed":[],"version":1}"#
    );
    assert_eq!(read(vm("/home/pilot/.claude/CLAUDE.md")), "edited");
    let none = stdout(ok(&run(&machine, &config, &["copy"])));
    assert_eq!(none.trim(), "nothing changed");

    // Ad hoc: SRC's place under the VM home by default, never managed,
    // and a declared entry for the same path wins on the next copy.
    fs::write(machine.home.join("notes.md"), "mine").unwrap();
    let human = stdout(ok(&run(&machine, &config, &["copy", "~/notes.md"])));
    assert_eq!(human.trim(), "copied /home/pilot/notes.md");
    assert_eq!(read(vm("/home/pilot/notes.md")), "mine");
    fs::write(machine.home.join("other.md"), "other").unwrap();
    let args = ["copy", "~/other.md", "~/.claude/CLAUDE.md"];
    ok(&run(&machine, &config, &args));
    assert_eq!(read(vm("/home/pilot/.claude/CLAUDE.md")), "other");
    ok(&run(&machine, &config, &["copy"]));
    assert_eq!(read(vm("/home/pilot/.claude/CLAUDE.md")), "edited");
    assert_eq!(read(vm("/home/pilot/notes.md")), "mine");
    assert!(
        !read(machine.state.join("bays/default/files")).contains("notes.md")
    );

    let error = failed(&run(&machine, &config, &["copy", "/etc/hosts"]));
    assert!(error.contains("outside your home: give a DEST"), "{error}");
    fs::write(machine.home.join(".netrc"), "machine x").unwrap();
    let error = failed(&run(&machine, &config, &["copy", "~/.netrc"]));
    assert!(error.contains("looks like a credentials file"), "{error}");
    let error =
        failed(&run(&machine, &config, &["copy", "~/notes.md", "/nix/x"]));
    assert!(error.contains("must be in ~"), "{error}");
}

#[test]
fn up_never_restarts_and_restart_applies_changed_inputs() {
    let machine = Machine::new("restart");
    machine.install_fake_msb();
    let vault = FakeVault::start(&machine.fake);
    fs::create_dir_all(machine.home.join("dotfiles")).unwrap();
    fs::write(machine.home.join("dotfiles/app.toml"), "v1").unwrap();
    let agent = |mode: &str| {
        format!(
            r#", "env": {{"MODE": "{mode}"}},
                "run": {{"web": "serve", "worker": "work"}},
                "files": {{"~/.app.toml": "~/dotfiles/app.toml"}}"#
        )
    };
    let config = machine.config(&vault_config(vault.port, "", &agent("a")));
    let restarts = |m: &Machine| m.fake_file("restarts");

    ok(&run(&machine, &config, &["up"]));
    let again = stderr(ok(&run(&machine, &config, &["up"])));
    assert!(!again.contains("inputs changed"), "{again}");

    // A changed input: up warns and leaves the process alone.
    let config = machine.config(&vault_config(vault.port, "", &agent("b")));
    let warned = stderr(ok(&run(&machine, &config, &["up"])));
    assert!(
        warned.contains(
            "web's inputs changed; run 'hangar restart web' to apply"
        ),
        "{warned}"
    );
    assert_eq!(restarts(&machine), "");

    let restarted =
        json_line(&run(&machine, &config, &["--json", "restart", "web"]));
    assert_eq!(
        restarted,
        r#"{"bay":"default","ok":true,"restarted":["web"],"version":1}"#
    );
    assert_eq!(restarts(&machine), "web\n");
    let quiet = stderr(ok(&run(&machine, &config, &["up"])));
    assert!(!quiet.contains("web's inputs changed"), "{quiet}");
    assert!(quiet.contains("worker's inputs changed"), "{quiet}");

    // --no-copy restarts without copying the changed file; the next up
    // copies it and warns, because the process started with the old one.
    fs::write(machine.home.join("dotfiles/app.toml"), "edited").unwrap();
    let human = stdout(ok(&run(&machine, &config, &["restart", "--no-copy"])));
    assert_eq!(human.trim(), "restarted web\nrestarted worker");
    let app = machine.fake.join("vmfs/home/pilot/.app.toml");
    assert_eq!(read(&app), "v1");
    assert!(machine.fake_file("bay-env").contains("MODE='b'"));
    let warned = stderr(ok(&run(&machine, &config, &["up"])));
    assert_eq!(read(&app), "edited");
    assert!(warned.contains("web's inputs changed"), "{warned}");
    assert_eq!(restarts(&machine), "web\nweb\nworker\n");

    let error = failed(&run(&machine, &config, &["restart", "nope"]));
    assert!(
        error.contains("nope is not a run entry or app: run has: web, worker"),
        "{error}"
    );
}
