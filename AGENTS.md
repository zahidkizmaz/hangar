# AGENTS.md

hangar is a Rust CLI that runs AI coding agents in locked-down sandbox
VMs, the bays, on a pluggable backend (microsandbox by default). A bay's only way out is a named proxy
(`src/proxy/<kind>.rs`), which holds the real credentials, or, with
`"proxy": null`, the open internet. User docs: `README.md` (short
overview), `docs/usage.md`, `docs/configuration.md`,
`docs/proxy-kinds.md` and `docs/nix.md`.

## Invariants (never break)

- No secret reaches argv, env (`VmSpec.env`), logs, `--json` or a bay's
  files. Secrets travel on stdin only, or in-process as `Secret`s. Start
  every child via `process::command`.
- A proxy's login reaches a bay only inside `/etc/hangar/proxy.env`,
  written as root over stdin (root:pilot 0640). Enrolment tokens and CA
  keys never enter a bay.
- A proxy's own tokens live in the OS keychain (`src/keychain.rs`) or
  come from a command the config names; never in the config, a file of
  hangar's or a bay. Only debug builds accept a fake keychain tool.
- Proxy kinds live only in `src/proxy/<kind>.rs` plus one line in
  `proxy::kinds()`. Nothing else in `src/` names a kind or branches on
  one (`no_core_module_names_a_kind`). Optional capabilities return
  `Option`, never an "unsupported" error.
- Managed proxies run only in their own VM (`hangar-proxy-<name>`),
  with mounts only under their own data folder.
- One egress: a bay with a proxy is created with
  `Egress::HostPort(proxy.egress())` and nothing else; direct bays and
  managed proxies get `Egress::Open`, plus one host port
  (`OpenAndHostPort`) for a managed proxy whose upstream is local. Remote (non-loopback) proxies are
  refused until the egress spec supports them.
- An existing VM whose record (`bays/<name>/vm`, `proxies/<name>/vm`)
  doesn't match is recreated, never used. `up` prints why. A VM-level
  change is never applied in place, and an in-place change never
  recreates.
- Kept across a recreate: the bay's home, its package cache and its data
  volume (unless `disk` changed). Nothing else.
- Every bay gets placeholders only: `env` refuses real-looking values.
- Mount and copy safety rules (`mounts.rs`, `files.rs`) apply to the path
  as written and to every resolved symlink target. Writable mounts never
  point at host config (dot dirs, `~/Library`, outside home). No mount or
  copy may contain or enter hangar's data or cache dir, except the bay's
  own home and cache.
- All hangar paths follow XDG, resolved by `src/dirs.rs`; no ad-hoc dirs
  in `$HOME`.
- The package cache is never trusted without upstream signatures; hangar
  never creates a signing key.
- JSON is the only config; Nix only renders `hangar.json`. Defaults live
  only in `config/defaults.json` and `config/apps/*.json`. No code
  branches on an app name.
- The published image ships no agent tools (each bay's `packages`
  instead).
- The bay image is declarative NixOS (`nix/bay/configuration.nix`). Bays
  boot its systemd (`VmSpec.init`) and `up` waits for it; hangar never
  restores snapshots. Daemon restarts are systemd units
  (`hangar-proxy-env.path`), not hangar's execs.
- In a bay, root's execs name absolute programs and never start a login
  shell, and the image's `PATH` never holds hangar's package profile.
  Pilot can't write anything root runs or reads.
- Only `src/sandbox/<backend>.rs` knows its backend; the rest goes
  through the `Sandbox` trait.
- Logs never contain secrets at any level: log argv and paths, never
  headers, bodies or values.
- Neutral content: no employer, person or machine specifics.

## Coding standards

- Simple over complex; delete before adding; no speculative abstractions.
- Fewest dependencies: std, then crates already in `Cargo.lock`; justify
  any new crate.
- stdout = command output, stderr = diagnostics via `log` (`info!` for
  progress, `debug!`/`trace!` for detail). Commands return a result;
  `output.rs` renders it (`Render`: `human()` + `json()`). `output::VERSION`
  stays 1 until the first release; after that, changing a `--json`
  field's meaning means bumping it (docs/cli.md).
- Edition 2024, `[lints]` in `Cargo.toml`, no `unwrap`/`expect` outside
  tests, `#[expect(.., reason)]` over `#[allow]`.
- Pure logic unit-tested, I/O at the edges; errors name the item and
  carry a hint.
- Tests cover all code and use fakes only (`FakeSandbox`, `FakeProxy`,
  `FakeKeychain`, `FakePrompt`, `tests/fakes/msb.sh`,
  `tests/fakes/keychain.sh`, loopback servers, XDG in a scratch dir):
  never a real VM, the network, `$HOME`, the user's config or keychain.
- Docs move with the code: a stale doc is a bug. `README.md` stays a
  short overview; no design docs in the repo.
- Comments only for genuine traps; 80 columns.

## Gates

Run the checks in `docs/development.md` ("Checks") before calling work
done.

## Read when relevant

- `docs/architecture.md`: before changing how VMs, proxies, config,
  packages or the bay image work; has the file map and what the core
  verifies.
- `docs/proxy-kinds.md`: before touching `src/proxy/` or adding a kind.
- `docs/sandbox-backends.md`: before touching `src/sandbox/` or adding a
  sandbox backend.
- `docs/development.md`: before running end-to-end tests or touching CI,
  Renovate or releases.
- `docs/configuration.md`, `docs/usage.md`, `docs/nix.md`: before
  changing a setting, an app, a command's behavior or a module option.
- `docs/cli.md`: before driving hangar from a script or agent, or changing
  output, `--json` schemas or exit codes.
