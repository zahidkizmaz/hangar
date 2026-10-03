# AGENTS.md

hangar is a Rust CLI that runs AI coding agents in locked-down sandbox
VMs (microsandbox), the bays. A bay can only reach the proxy of the
credential broker (agent-vault) in the tower, another VM, which injects
real credentials for allowlisted hosts. User docs: `README.md` (short
overview) and `docs/usage.md`, `docs/configuration.md`, `docs/nix.md`.

## Invariants (never break)

- The master password and credential values travel only on stdin or in
  in-process HTTP bodies: never argv, env, logs or disk. hangar's own
  tokens live in `stateDir` at mode 0600. Start every child via
  `process::command`.
- The proxy URL carries the bay's own token (one per bay, in
  `agent-tokens/<bay>`): it reaches that bay on stdin only, into its
  proxy env, never in argv, `guest/` (the bays' read-only mount:
  `ca.pem` only) or an `info!`/`debug!` line.
- No default or suggested master-password file. The keychain password is
  generated per install; file/env are explicit opt-ins.
- Deny mode is set, and read back from the broker, before routes and
  credentials. hangar ships no routes: every allowed host comes from an
  app a bay lists or from `tower.routes`, and apps and the user never
  shadow each other's routes.
- Only hangar-managed credentials, packages, copied files and the vault
  agents hangar created (`hangar-<bay>`, the ones with a token file) are
  ever deleted. `up` never restarts a VM or run entry; only `hangar
  restart` restarts run entries. A bay restarts its daemons only when
  their proxy env (which names the CA) changed: hangar never replaces an
  unchanged one.
- Every bay gets placeholders only: a bay's `env` refuses real-looking
  values. Never print or change the broker admin login (hangar logs in
  with it on every `up`).
- A bay's `files` never copies secrets: the deny-list and the content scan
  (`secret::find_secret`, shared with `env`) must stay.
- A bay's writable `mounts` never point at host config dirs (dot dirs,
  `~/Library`, outside home); the mount safety rules in `mounts.rs` stay.
  No mount or copied file may contain or enter `stateDir` or the cache
  root `$XDG_CACHE_HOME/hangar`, except each bay's own
  `<stateDir>/bays/<name>/home` and `$XDG_CACHE_HOME/hangar/bays/<name>`,
  for that bay only: nothing else under either root, and never another
  bay's folders.
- All hangar paths follow XDG (config, data, cache, state), resolved at
  runtime by `src/dirs.rs`; no ad-hoc dirs in `$HOME`.
- Never mount or copy the user's real config dirs (`~/.claude`, …) by
  default: a bay's home is hangar's own (`home`); packages and
  hangar's VM records stay on the VM disk, off the kept home.
- The package cache (`cache`) is never trusted without upstream
  signatures (no `trusted`, `require-sigs` or `--no-check-sigs` changes);
  hangar never creates a signing key.
- JSON is the only config; Nix only renders `hangar.json`. Defaults live
  only in `config/defaults.json` and `config/apps/*.json`.
- The published image ships no agent tools (each bay's `packages`
  instead).
- The bay image is declarative NixOS (`nix/bay/configuration.nix`): no
  hand-written users, setuid copies or daemon launches. Bays boot its
  systemd (`VmSpec.init`, the sandbox's own `/run`) and `up` waits for
  it; hangar never restores snapshots (they lose the init handoff).
  Daemon restarts are systemd units (`hangar-proxy-env.path`), not
  hangar's execs; `up` writes only what is unknown until then.
- In a bay, root's execs name absolute programs and never start a login
  shell, and the image's `PATH` (root's `sh -c` steps) never holds
  hangar's package profile. Pilot can't write anything root runs or
  reads: `/etc/hangar/proxy.env` stays root:pilot 0640, the CA bundle
  and `/etc/hangar/bay.env` root's.
- Only `src/{sandbox,broker}/<backend>.rs` know their backend; the rest
  goes through the `Sandbox` and `Broker` traits. No code branches on an
  app name: apps are data in `config/apps/` or `appDefinitions`.
- Every bay's only egress is the tower's proxy port, bays never reach each
  other, and `up` refuses VMs
  whose records (`bays/<name>/vm`, `tower-vm`) it can't match. hangar never
  sets `AGENT_VAULT_ALLOW_PRIVATE_RANGES` or `AGENT_VAULT_NETWORK_ALLOWLIST`:
  agent-vault's netguard keeps refusing loopback and private addresses.
- Logs never contain secrets at any level: log argv and request method +
  path, never headers, bodies or values.
- Neutral content: no employer, person or machine specifics.

## Coding standards

- Simple over complex; delete before adding; no speculative abstractions.
- Fewest dependencies (`miniserde`, `clap`, `log`); justify any new crate.
- stdout = command output, stderr = diagnostics via `log` (`info!` for
  progress, `debug!`/`trace!` for detail). Commands return a result;
  `output.rs` renders it (`Render`: `human()` + `json()`). `output::VERSION`
  stays 1 until the first release; after that, changing a `--json`
  field's meaning means bumping it (docs/cli.md).
- Edition 2024, `[lints]` in `Cargo.toml`, no `unwrap`/`expect` outside
  tests, `#[expect(.., reason)]` over `#[allow]`.
- Pure logic unit-tested, I/O at the edges; errors name the file or item.
- Tests cover all code: every new or changed path gets a test (unit tests
  for logic, `tests/cli.rs` for CLI behavior). Untested code isn't done.
- Docs move with the code: update the user docs (`docs/usage.md`,
  `docs/configuration.md`, `docs/nix.md`; `README.md` stays a short
  overview) and the internals (other `docs/`) in the same change. A
  stale doc is a bug.
- Comments only for genuine traps; 80 columns.

## Gates

Run the checks in `docs/development.md` ("Checks") before calling work done.

## Read when relevant

- `docs/architecture.md`: before changing how VMs, the broker, config,
  packages or the bay image work (how a bay boots and starts its
  daemons); has the file map, the naming rules (bay, tower, broker,
  agent) and what the core verifies of a backend.
- `docs/development.md`: before running end-to-end tests or touching CI,
  Renovate or releases.
- `docs/sandbox-backends.md`: before touching `src/sandbox/` or adding a
  sandbox backend; `src/broker/mod.rs` for the `Broker` trait.
- `docs/configuration.md`, `docs/usage.md`, `docs/nix.md`: before
  changing a setting, an app, a command's behavior or a module option.
- `docs/cli.md`: before driving hangar from a script or agent, or changing
  output, `--json` schemas or exit codes.
