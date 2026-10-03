# Architecture

## VMs and request flow

microsandbox VMs: one tower and any number of bays (`bays` in the config;
none configured means one, `default`).

- `hangar-tower`, the tower, runs the credential broker (`tower.backend`,
  today only `agent-vault`, digest-pinned image) with the real
  credentials. It publishes its admin API (`tower.agentVault.adminPort`,
  default 14321) and MITM proxy (`tower.agentVault.proxyPort`, 14322) on
  the host's `127.0.0.1` only; its own egress is the public internet.
- `hangar-bay-<name>` is a bay, where agents run. Its only outbound path
  is the host's proxy port, so traffic flows bay → `host:proxyPort` →
  agent-vault → upstream. The vault swaps placeholders for real
  credentials and, in deny mode, answers `403` naming any host without a
  route. Each bay has its own vault agent, `hangar-<name>`, and token, so
  the vault's request log says which bay made a request.

## Names

- **Bay**: a VM where AI coding agents run (`hangar-bay-<name>`).
- **Tower**: the one VM that runs the credential broker (`hangar-tower`).
- **Broker** is the tower's role. The `Broker` trait, `src/broker/` and
  the agent-vault backend keep that name: the trait says what the tower
  does, not where it runs, much as `Sandbox` isn't named after a VM.
- **Agent** stays only as an upstream term: agent-vault's own agents
  (`hangar-<bay>`, its token, `agent-tokens/`) and "AI coding agents" in
  prose.

## What the core verifies

Backends aren't trusted with the security-critical parts; the core
checks them through the traits:

| Invariant | Enforcement |
|---|---|
| deny before routes and credentials | `Broker::deny_unlisted`, then `health().unlisted` must be `Deny`, else `up` refuses to apply routes |
| step order | `TOWER_STEPS`: deny → routes → credentials → tokens; then each bay's `access()` |
| only managed credentials deleted | `stale_keys` and `credential-keys`; the broker only gets explicit keys |
| one egress port | bays are created with `Egress::OnlyHostPort(proxy)`, and `access()` must name the same port |
| egress fixed at create | `bays/<name>/vm` and `tower-vm` record it; an existing VM without a matching record is refused with the recreate hint |
| secrets never in argv, env or logs | `exec` stdin, `Access` and `UiLogin` hold `Secret`s, `VmSpec.env` is non-secret |

## State

Every default path comes from `src/dirs.rs`, per the XDG Base Directory
spec, at runtime: `hangar_dir` gives `$XDG_{CONFIG,DATA,CACHE}_HOME/hangar`,
or the spec's fallback under `$HOME` (`~/.config`, `~/.local/share`,
`~/.cache`) when a variable is unset or relative. No default is a fixed
path in `config/defaults.json` or the Nix module; the macOS login agent
logs to `${XDG_STATE_HOME:-~/.local/state}/hangar/hangar.log`, resolved by
its shell wrapper.

- `stateDir` (default `$XDG_DATA_HOME/hangar`, mode 0700):
  - shared: the broker's own files (agent-vault: `vault/`, its data,
    mounted at `/data`, and `owner-password`; they never move, so an
    existing vault keeps unlocking), `credential-keys` (credentials hangar
    set), `tower-vm` (the tower's published ports,
    `port\t<name>\t<host>\t<vm>` lines, the proxy named `proxy`),
    agent-vault's `agent-tokens/<bay>` (0600, one per bay; a bay gets only
    its own, inside the proxy URL) and `guest/` (only `ca.pem`, mounted
    read-only at `/run/hangar` in every bay);
  - per bay, `bays/<name>/`: `vm` (what the bay's VM was created with,
    `key\tvalue` lines: `image\t<ref>`, `egress\t<port>`,
    `port\t<name>\t<host>\t<vm>`, and `mount\t<vm>\t<host>\t<ro|rw>` per
    mount with its resolved host, the home mount included; no `mount`
    lines means none), `files` (hash and VM path of each file
    `files` copied), `run-fingerprints` (what each `run` entry
    was started with) and `home/`.
- `home` (`true`): the bay's `bays/<name>/home` (0700) is its home
  (`/home/pilot`), mounted writable, so app data outlives the VM; `destroy`
  keeps it, `destroy --state` deletes it with the rest of `stateDir`.
- `cache` (`true`): `$XDG_CACHE_HOME/hangar/bays/<name>` (0700),
  mounted writable at `/var/cache/hangar`; `nix/` inside is a `file://`
  binary cache of the installed packages. Never shared between bays.
- Neither takes a path, so every bay's folders sit in one of two roots,
  `stateDir` and the cache root `$XDG_CACHE_HOME/hangar`; `Hangar::load`
  refuses a `stateDir` that overlaps the cache root. A `Bay` (built by
  `Hangar::bay`, no I/O) knows both folders even when off: the mount
  rules need them.
- `$XDG_STATE_HOME/hangar/lock`: held by every command that changes
  state, first thing (before the master password is read or generated),
  so two `up`s can't race. It lives outside `stateDir`, which
  `destroy --state` deletes. `src/lock.rs`: one `try_lock`, then
  "waiting for another hangar" and a blocking `lock()`.
- OS keychain: service `hangar`, account `master-password`, through
  `/usr/bin/security` or `secret-tool` by absolute path (`/usr/bin/`, or
  libsecret's store path baked in by `nix/cli.nix` via
  `HANGAR_SECRET_TOOL` at compile time), never found on `PATH`.
- VM disks: a bay's root disk holds `/nix`, hangar's package profile
  (`/nix/var/nix/profiles/hangar`, first on login shells' `PATH`), the
  package record (`/var/lib/hangar/packages.json`), docker data and repos.
  Profile, record and store are lost together with the VM, so a new VM with
  a kept home reinstalls cleanly. The root keeps the image's layers, so
  `/var/lib/pilot`, Docker's storage, gets a disk of its own
  (`VmSpec.native_fs`), since overlayfs can't stack on them; both are
  `disk` in size.

## Who runs what in a bay

Apps never run as root. Every bay `exec` names its user (`bay::ROOT` or
`bay::PILOT`); the tower keeps its image's default.

- root: the CA bundle and proxy env, the package reconcile and cache
  fill, the env file (`write_env`): hangar's own steps.
- `pilot` (uid 1000, home `/home/pilot`, from the image): run entries
  (start, stop, status), setup and its check, `files` copies and
  removals, and `shell`, `logs` and `setup` (which start in the home).
  Docker is pilot's own rootless daemon; nix goes through `nix-daemon`,
  which doesn't trust pilot. No sudo.

Root never follows, sources or executes a path pilot can write:
- `files` and `mounts` targets must be in `~` (`files::vm_path`), so
  pilot only ever owns its own home;
- root's execs name absolute programs (`/bin/sh`,
  `/run/current-system/sw/bin/nix`) and never start a login shell; the
  package step sources only the root-owned proxy env file;
- pilot's folders are declared in the image (systemd-tmpfiles), under
  root-owned parents, and dockerd makes its own data root;
- `start` first waits until the VM's systemd has booted.

## The bay image

`nix/bay/configuration.nix` is the bay as a NixOS system, and
`nix/image.nix` packs it (`dockerTools.streamLayeredImage`, with the Nix
DB): a root of `/sbin/init` (the system's `init`), `/etc`, `/root` and
`/home/pilot` (for `home = false`). Its `Env` is only
`PATH=/run/wrappers/bin:/run/current-system/sw/bin`, the `PATH` of root's
`sh -c` steps, so it never holds hangar's profile, where a bay package
could ship its own `mv`. Pilot's login shell
puts the profile first (`environment.profiles`).

- Boot: `msb create --init /sbin/init --tmpfs /run`. agentd sets up the
  VM (mounts, network, `/etc/hosts`, `resolv.conf`), appends to
  `/etc/profile` and the CA bundle, then execs `/sbin/init`: NixOS
  activation, then systemd as PID 1. Both appended files are copies
  (`mode = "0644"`), so the append never writes into `/nix/store`.
  Without `--tmpfs /run`, systemd's own `/run` hides the `/run/hangar`
  mount; with it, `/run` is per boot and run PID files can't outlive a
  restart.
- Readiness: `msb create` and `start` return before systemd is up. Root's
  exec fails ("failed to resolve guest uid 0") until activation writes
  passwd, then `systemctl is-system-running --wait` says `offline`, then
  `running` or `degraded`, both booted. `wait_booted` polls every 0.5 s
  for up to 2 minutes; `maintenance` and `stopping` fail at once.
- Snapshots: restoring an msb snapshot loses `--init` (msb #1676), so
  hangar never restores one.
- Disks: the root keeps the image's layers (msb can't flatten a loaded
  image), and overlayfs can't stack on them, so `/var/lib/pilot`
  (Docker's storage) gets a disk of its own (`VmSpec.native_fs`), removed
  with the VM. Both are `disk` in size and sparse: at worst twice `disk`
  on the host.
- Daemons are systemd units: socket-activated `nix-daemon` and pilot's
  rootless `docker` user unit (`linger`, so it starts without a login;
  `TimeoutStartSec = 60s` and `TimeoutStopSec = 30s` over the module's
  `TimeoutSec = 0`, so a hanging start or stop can't hang `up`). Both read
  `EnvironmentFile=-/etc/hangar/proxy.env`, plain `KEY='value'` lines
  (systemd rejects `export`); the `-` lets them start on the first boot,
  before the file exists.
- `up` writes what is unknown until then, as root over stdin (`bay.rs`,
  `replace`): the CA bundle (`/var/lib/hangar/ca-bundle.crt`, 0644, the
  image's `/etc/hangar/system-ca.crt` plus `/run/hangar/ca.pem`), then
  `proxy.env` (root:pilot 0640). Each goes to a `.tmp` file first and
  replaces the old one with `mv` only when `cmp` finds it changed.
  `proxy.env` starts with a `# tower CA <hash>` line, so a new CA changes
  it too.
- Restarts are declared: the `hangar-proxy-env.path` unit
  (`PathChanged=/etc/hangar/proxy.env`) starts the oneshot
  `hangar-proxy-env.service`, which runs `systemctl try-restart
  nix-daemon`, then `systemctl --user -M pilot@.host reset-failed docker`
  and `restart docker`. The reset is there because the upstream unit
  allows 3 starts per 60 s (`StartLimitBurst`): a new bay uses two (boot,
  the first `proxy.env`), so one crash or a renewed token in that minute
  would hit `start-limit-hit`, and with nothing changed no later `up`
  would restart it. The limit itself stays, so a broken dockerd doesn't
  restart forever on its own (`Restart=always`). The
  path unit fires when the file is created or replaced (inotify on the
  file's inode, and on its folder until it exists), never on its own
  start (systemd.path(5)). It watches only `proxy.env`, written last:
  while the oneshot runs, the path unit doesn't watch, so a second
  watched file could change unseen. A renewed token or a new tower CA
  therefore stops pilot's containers; an unchanged `up` restarts
  nothing.
- Readiness: `up` then polls as pilot (`wait_for_docker`, every 0.5 s for
  up to 2 minutes) until `systemctl list-jobs hangar-proxy-env.service`
  is empty (a restart was queued or is still running) and `docker info`
  answers; the unit is active before its socket is.
- Shells: `msb exec` is no PAM login, so `environment.extraInit` sets
  `USER`, `LOGNAME` and `XDG_RUNTIME_DIR`, then sources `proxy.env` and
  `bay.env` with `set -a`. Root's package step sources only `proxy.env`.
- pilot: uid and gid 1000, `createHome = false` (the home is usually a
  host mount), subuid and subgid 100000:65536 (NixOS's automatic range
  for the only normal user). msb doesn't map uids by
  identity: a mounted file shows the mount's `uid=,gid=` (`Mount.owner`),
  so the host uid doesn't matter.
- Privileges left: `newuidmap`/`newgidmap` (capabilities, for rootless
  Docker) and setuid `unix_chkpwd` (PAM needs it for `user@1000`); `su`,
  `sg`, `newgrp`, `mount`, `umount` and sudo are off. `/dev/net/tun` is
  0666, which rootlesskit needs (root:pilot 0660 is unverified).
  nix-daemon trusts only root, and builds run as nixbld.
- Known gap: with `sandbox = false`, pilot's builds run unsandboxed as
  nixbld and write to the store root installs packages from. Turn the
  sandbox on once the bay kernel is shown to support it.
- A custom `bays[].image` must be built from this module: hangar needs
  systemd at `/sbin/init`, pilot, `nix`, `systemctl` and `docker` in the
  system's `sw/bin`, `/etc/hangar/system-ca.crt` and the
  `hangar-proxy-env` units.

## `hangar up`

Every step is idempotent, and `up` never restarts the VMs or run
entries (the bay restarts its daemons only when their proxy env or CA
changed):

1. Resolve the master password: `HANGAR_MASTER_PASSWORD`,
   `HANGAR_MASTER_PASSWORD_FILE`, `tower.masterPasswordFile`, then the keychain.
   Generate one only when the broker has no data (`Broker::has_data`).
   Fails before anything is created; an empty password is refused.
2. Tower pre-flight, before any VM is touched: an existing tower must have
   a `tower-vm` record with today's proxy port. Without a record, or with
   another port, `up` refuses with "run 'hangar destroy && hangar up'
   (home is kept)": it's fixed when the VM is created. Other tower port
   changes only warn.
3. Tower (`Broker::ensure_running`): create or start, then unlock it
   with the password on stdin and log in as admin (agent-vault: start the
   server, since starting a sandbox doesn't rerun the image's entrypoint,
   then the owner login). A new VM gets its `tower-vm` record, even when
   a later part of the start fails, so the next `up` can finish it.
4. Deny mode; the core then reads the policy back (`Broker::health`) and
   refuses to go on unless unlisted hosts are denied.
5. Routes (the whole set; agent-vault: `service set -f -` on stdin).
6. Credentials from `tower.credentialFiles` (values on stdin or in a request
   body); delete those in `credential-keys` that left the config. The
   broker only ever gets explicit keys.
7. Bays' tokens (`Broker::retain_bays`): the vault agent and token file
   of every bay that has a token but isn't configured are deleted (a 404
   counts as gone); vault agents without a token file aren't hangar's.
8. Each bay, in order. Its first step is its own pre-flight: an existing
   VM needs a `bays/<name>/vm` record whose egress is the proxy port, and
   its recorded mounts must pass today's mount rules (a root may have
   moved), or the bay is refused with the same recreate hint. Then:
   create or start, wait until its systemd has booted (see "The bay
   image"), then `Broker::access(bay)` (agent-vault keeps the bay's token
   in `agent-tokens/<bay>`; without the file it deletes any vault agent
   `hangar-<bay>` and creates it again, never adopting one, and warns to
   restart the bay's run entries) with the CA written to `guest/ca.pem`,
   checked to name the proxy port, then the CA bundle and the proxy env
   (the proxy URL on stdin) and the wait for docker. The image is
   `bays[].image`, else `<imageRepository>:v<hangar version>`;
   `imageLoader` (from `imagePackage`) loads it first if missing. The
   VM's only egress is the tower's proxy port (`Egress::OnlyHostPort`); it
   publishes the enabled apps' ports. Its image, egress, published ports
   and mounts are recorded in `bays/<name>/vm`, also when a failing create
   left the VM behind; a later `up` with a different image, ports or
   mounts warns instead of recreating the VM (`drift_warnings`).
   `mounts` plus hangar's own mounts (`mounts::with_hangar`:
   `home` at `/home/pilot` and `cache` at `/var/cache/hangar`,
   writable, same rules; pilot (uid 1000) owns every mount but the
   cache and `/run/hangar`, which stay root's) are checked on their resolved sources
   (`mounts::Roots`; a missing one on the path it would have, then created
   0700) before the VM is created, passed in order (read-only unless
   writable; the sandbox applies enclosing mounts before nested ones) and
   recorded. A source may contain neither root, and inside them only the
   bay's own home and cache are allowed (`mounts::check_hangar_dirs`, also
   used for `files` sources); those are the only folders in a dot
   directory a writable mount may use.
9. Reconcile `packages` (below) in hangar's profile
   (`--profile /nix/var/nix/profiles/hangar`).
10. Copy `files` (`src/files.rs`). The whole set is planned first:
    every file is read and checked (credentials deny-list by name and place,
    the shared secret scan, no symlinks or special files, 1 MiB per file,
    10 MiB and 1000 files in total, VM paths inside pilot's home,
    without `..`). Any refusal stops the step before anything is copied. Then
    files hangar copied whose entry is gone are removed (`rm -f`), and each
    changed file is written atomically with its contents on stdin; the
    `bays/<name>/files` record (FNV-1a of mode plus contents) is saved after
    each file. Creating a new bay VM clears the record, so every file is copied
    again. One core serves every copy: the checks in `files::plan`, the pure
    `plan_copy` (declared: write changed, delete gone; ad hoc: write all,
    delete nothing, forget the hashes it shadows) and `apply_copy`.
11. Write `/etc/hangar/bay.env` (via `bay.env.tmp` and `mv`, so a login
    never sources half a file; login shells source it with `set -a`):
    `NAME='hangar-placeholder'` for
    every credential name a route's auth references (`token`,
    `username`, `password`, `key`, `{{ NAME }}` in custom headers) plus
    `credentialFiles`, then `env` on top. Values are single-quoted;
    `env` refuses real-looking secrets. Rewritten every `up`.
12. Run each enabled app's setup `check` in a login shell (`sh -lc`, so
    `PATH` and the app env apply); only its exit status counts. An app
    whose check fails gets the warning "NAME needs setup: run 'hangar
    setup NAME'", and step 13 skips the run entry of that name, even when
    `run.NAME` overrides the command. There's no record: the check
    is the truth, so a wiped home asks for setup again.
13. Start each `run` entry not already running: `setsid sh -lc` (so
    the proxy env and placeholders apply), output to
    `/var/log/hangar/<name>.log`, PID in `/run/hangar-run/<name>.pid` (on
    a per-boot tmpfs). It counts as running only while that PID carries
    `HANGAR_RUN=<name>`, so a reused PID isn't mistaken for it. Each start
    records a fingerprint (FNV-1a of the command, the env file and the
    `files` record) in `run-fingerprints`; a running entry whose
    inputs differ only gets a warning (`run_decision`).
14. Print one line per app (its hosts and credentials) and the port
    overview (also in `status`). Every published port is probed the same
    way: an HTTP GET for HTTP ports, a TCP connect otherwise; the proxy,
    which only the bays use, isn't probed.

`hangar copy` runs step 10 alone (or one ad-hoc source through the same
core). `hangar restart [NAME…]` runs step 10 (unless `--no-copy`) and step
11, then stops each entry's process group (TERM, KILL after 5 seconds) and
starts it again through the same `launch` as step 13, recording the new
fingerprint. It doesn't run setup checks: a restart is an explicit action.

`hangar setup NAME [--force]` needs the bay running and `NAME` an
enabled app with a `setup`. Unless `--force`, an app whose check passes is
left alone; otherwise it runs `sh -lc COMMAND` through `Sandbox::shell`
with a terminal, then the check again, and a failing command or check is
an error. `status` shows a stopped entry of an app with a setup as
`stopped (needs setup? hangar setup NAME)`.

## Credentials

- `tower.credentialFiles`: hangar-managed, recorded in `credential-keys`,
  reconciled on every `up`. So is an app's fixed, non-secret
  `credentials` value (e.g. `github-token`'s `GITHUB_GIT_USER`) whenever
  a route references it; two apps may share one only with the same value,
  and `credentialFiles` can't name it.
- `hangar credential set/list/rm`: the user's own. Values come from a
  hidden prompt (`stty -echo` in a trapped `sh`, value back over a pipe) or
  stdin and go to the admin API in-process. Never recorded, so `up` never
  deletes them; names managed by the config are refused.
- `hangar vault-ui`: pipes `owner-password` into `pbcopy`, `wl-copy` or
  `xclip` and opens the UI; it never prints the password.

## Package reconcile

hangar records which element of its profile (`/nix/var/nix/profiles/hangar`)
each installable became, in the package record on the VM disk. Each `up`
reads the record (none yet: empty) and the profile (missing on a new VM:
empty), removes recorded elements no longer listed, installs missing ones
(`nix profile add --profile … --impure` in a login shell, with
`NIXPKGS_ALLOW_UNFREE=1` for that step only) and saves the record after
every step. Recorded elements missing from the profile are reinstalled;
elements not in the record were installed by hand and are never touched.

With `cache`, installs add `--extra-substituters
file:///var/cache/hangar/nix?priority=10` (lower priority wins over
cache.nixos.org's 40; `trusted` stays off, so cached paths still need a
signature from a trusted key). After an `up` that installed something, `nix
copy --to file:///var/cache/hangar/nix?compression=zstd <profile>` copies
the profile's closure into the cache, keeping each path's upstream
signature; a failed copy only warns. hangar never creates a signing key, so
locally built (unsigned) paths are copied but never used from the cache.

## Config

`hangar.json` is the only config. Lookup: `$HANGAR_CONFIG`, then the
Nix-rendered file (`HANGAR_NIX_CONFIG`), then
`$XDG_CONFIG_HOME/hangar/hangar.json`. Its top level is checked first
(`bays`, `tower`, `appDefinitions`, `sandbox`, `stateDir`; anything else is
an unknown setting), then it's merged over `config/defaults.json`, whose
`bay` object is merged under each entry of `bays` (none: one bay,
`default`). Bay names are `[a-z0-9]+(-[a-z0-9]+)*`, at most 32 characters,
unique (agent-vault's own name rule then always holds for `hangar-<bay>`).
`home` and `cache` are strict booleans. Unknown settings and unknown `auth`
keys are errors.

Routes are what the tower forwards: a list of `{name, host, auth, extra?}`;
`extra` holds broker-specific fields, which agent-vault flattens into the
service (keys clashing with `name`, `host` or `auth` are errors, checked
by `Broker::validate` when the config loads). hangar ships none: they come
from the bays' apps and `tower.routes`.

Apps (`src/apps.rs`) are data: `config/apps/<name>.json` (built in via
`include_str!`) or `appDefinitions.<name>` (replaces a built-in whole),
enabled per bay by its `apps` (the built-ins are listed in
[configuration.md](configuration.md#built-in-apps)).
`config::resolve`:

1. Per bay: its `apps` (`apps::Catalog::enabled`).
2. Once, for the tower (`apps::merge_routes`): every bay's apps' routes,
   each app once in bay order; two apps may share a route only when it's
   identical ("enable only one of the two apps" otherwise), and a
   `tower.routes` entry may not take an app route's name, so apps and the
   user never shadow each other. Then `check_routes`: two sources can't
   route one host (`apps::Origin`), and an app's route can't use a
   credential another source uses. No app may set a credential's
   placeholder variable (`check_placeholders`), over every bay.
3. Per bay (`apps::apply`): `packages` are a union (order kept), `env` and
   `run` per key with the bay winning (two of the bay's apps disagreeing on
   a variable is an error), ports unique by name, with the bay's `ports`
   moving one to another host port. Then `check_packages`: a bay with
   packages needs a route whose host is exactly `cache.nixos.org`, and for
   `github:` or registry refs (`nixpkgs#x`, `flake:nixpkgs#x`) exactly
   `api.github.com`.

An app's route can't set `extra`, which reaches the broker unchecked and
so is trusted user config only. `Hangar::load` refuses two published
ports (the tower's and every bay's) on one host port, naming both owners
(`port 3100: work/paperclip and oss/paperclip: set ports.paperclip in bay
oss`), and warns about a `credentialFiles` entry no route uses. No code
outside `config/apps/` knows an app by name: checks key on hosts and
credential names.

`commands::pick` resolves `--bay` for the one-bay commands (the only bay,
else an error listing them). `up` runs the tower steps once, then each
selected bay's steps; a failing bay is logged, collected in
`UpReport::failed` and makes `up` exit 1, and the others go on.
`Hangar::leftovers` lists folders in `bays/` with no config entry (the
sandbox can't list VMs): `status` shows them, `up` warns, `down`/`destroy`
reach them by name.

## Files

- `src/main.rs`: argument parsing (clap) and dispatch.
- `src/commands.rs`: what each command does, including the ordered
  `TOWER_STEPS` and per-bay `BAY_STEPS` of `up`.
- `src/hangar.rs`: `Hangar`, the shared context (settings, state dir, the
  package cache root, sandbox and broker), which builds each `Bay`; the
  only place that picks the backends (`sandbox.backend`,
  `tower.backend`).
- `src/credential.rs`: `hangar credential`.
- `src/overview.rs`: what `status`, the port summary and `vault-ui`
  report (data only).
- `src/output.rs`: renders command results as text or `--json`, errors
  included, and picks the exit code (docs/cli.md).
- `src/config.rs`: lookup, merge, validation (routes into
  `broker::Route`).
- `src/apps.rs`: app definitions (built-in catalog, `appDefinitions`),
  their merge into the settings and the cross-source checks.
- `src/broker/mod.rs`: the `Broker` trait, `Route`/`Auth`, and the core
  side: the pre-flight, `tower-vm`, the deny check, the credential
  reconcile, the bays' tokens and the access check. `src/broker/agent_vault.rs` is the
  agent-vault backend (its VM, CLI, admin API and state files) and
  `src/broker/fake.rs` the in-process test double.
- `src/bay.rs`: `Bay` and its steps (its pre-flight, create or start,
  image loading, placeholders, `run`).
- `src/packages.rs`: package reconcile.
- `src/files.rs`: `files` (plan with the safety checks, reconcile,
  copy).
- `src/mounts.rs`: `mounts` (source and target rules, overlap with
  `files`, the creation record and status). Reuses `files.rs`'s path
  and credentials helpers.
- `src/sandbox/mod.rs`: the `Sandbox` trait, `VmSpec`, `Mount`, `Egress`,
  `BoxState` and the VM names; `src/sandbox/msb.rs` is the msb backend
  and `src/sandbox/fake.rs` the in-process test double (see
  `sandbox-backends.md`).
- `src/vm_record.rs`: the `bays/<name>/vm` and `tower-vm` records
  (`VmRecord`).
- `src/password.rs`, `src/keychain.rs`: password resolution, keychain.
- `src/lock.rs`: the lock that keeps a second hangar waiting.
- `src/state.rs`: the `stateDir` layout (`StateDir`, one method per file)
  and `write_private` for owner-only files.
- `src/process.rs`: child processes (strips `HANGAR_MASTER_PASSWORD`).
- `src/http.rs`: minimal HTTP/1.1 client for the admin API.
- `src/secret.rs`: `Secret` (redacting `Debug`, no `Display`),
  `random_hex` and `find_secret`, the scan `env` and `files`
  share.
- `src/json.rs`, `src/error.rs`: helpers.
- `nix/bay/configuration.nix`: the bay as a NixOS system (pilot, the
  daemons, the tool set).
- `config/defaults.json`, `config/apps/*.json`: the only copy of the
  defaults and the built-in apps, built into the binary (the Nix module
  only sets what you configure).
- `nix/image.nix`: the bay image, that system behind `/sbin/init`
  (generic tools only).
- `nix/module.nix`, `nix/darwin.nix`, `nix/nixos.nix`: Nix modules
  (render `hangar.json`, autostart).
- `nix/cli.nix` builds the binary, which `nix/module.nix` wraps with the
  rendered config; `nix/microsandbox.nix` packages msb.
- `tests/cli.rs`: integration tests; `nix/tests/module.nix`: the module
  check, which renders `hangar.json` and diffs it against
  `tests/module.json`.
