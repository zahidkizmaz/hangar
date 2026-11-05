# Architecture

## VMs and request flow

Every VM is a microsandbox VM:

- `hangar-bay-<name>`, a bay, where agents run. With a proxy, its only
  outbound path is that proxy's port on the host's loopback: bay →
  `host:<port>` → proxy → upstream. With `"proxy": null` it has the open
  internet.
- `hangar-proxy-<name>`, a managed proxy (kinds with `"run": "managed"`).
  Its own egress is the open internet; it publishes its port on the
  host's `127.0.0.1` only.

External proxies (`"url"`) run outside hangar; hangar only checks that
they answer.

## Proxies: the trait and the registry

`src/proxy/mod.rs` holds the `Proxy` and `Managed` traits and
`proxy::kinds()`, the registry: one `(name, build)` line per kind. A kind
is one file, `src/proxy/<kind>.rs`; nothing else in `src/` names one
(`no_core_module_names_a_kind`). `config.rs` checks each entry's `kind`
and location; `Hangar::load` calls the kind's `build` with a `Spec` (the
entry's fields, its location, the user's home and the proxy's data
folder), so a kind checks its own keys when the config loads.

The core never trusts a kind with the security-critical parts:

| Invariant | Enforcement |
| --- | --- |
| one egress per bay | a bay with a proxy is created with `Egress::HostPort(proxy.egress())`; direct bays and managed proxies get `Egress::Open` |
| egress fixed at create | the VM record holds the egress; a different one recreates the VM |
| managed proxies in their own VM | `proxy::managed` creates `hangar-proxy-<name>` from `Managed::vm()`; its mounts must resolve under the proxy's data folder |
| the CA a bay trusts | `proxy::access` checks it is exactly one PEM certificate (`ca::verify`), pinned when the kind sets `sha256` |
| secrets off argv, env and logs | logins and URLs are `Secret`s; tokens go on `exec` stdin; `VmSpec.env` is non-secret |
| remote proxies | `proxy::loopback_port` refuses a non-loopback `url` |

Kinds and how to add one: [proxy-kinds.md](proxy-kinds.md).

## The egress spec

`VmSpec.egress` is backend-neutral: `Egress::Open`, or
`Egress::HostPort(port)`, one port on the host and nothing else. msb
turns `HostPort` into `--no-net --net-rule allow@host:tcp:<port>`, plus an
ingress rule per published port. The record stores it as `open` or
`host:<port>`.

## `hangar up`

`commands::up` selects the named bays and proxies (none: all), adds each
selected bay's proxy, and brings the proxies up first, each once. A bay
whose proxy failed is left alone. Every resource ends in one `Change`:
`created`, `recreated` (with reasons), `updated` (with what changed),
`unchanged` or `failed`.

### A VM's plan

`lifecycle::plan` compares the VM's state and its record
(`bays/<name>/vm`, `proxies/<name>/vm`) with the wanted record: image,
egress, mounts, ports, resources and, for a managed proxy, a fingerprint.

- missing → create;
- exists without a record → recreate (`no record`);
- any field differs → recreate, with the fields as reasons;
- stopped → start; running → keep.

A recreate stops the VM first (`msb rm` is a hard kill), removes it and
creates it again. The home, the package cache and the bay's data volume
outlive it; a changed `disk` also replaces the data volume. A VM whose
create failed has no record, so the next `up` recreates it.

### A managed proxy

`proxy::managed::up`: plan and create the VM (`Managed::vm()`; the record
holds the fingerprint, e.g. a hash of the proxy token), and if the proxy
doesn't answer yet (`health`), wait until exec works, run
`Managed::start` and wait up to 60 seconds for it to come up. An external
proxy is only health-checked.

### A bay

`bay::reconcile`, each step named in its errors (`bay work: packages: …`):

1. **vm**: resolve and check the mounts (see [Mounts](#mounts)), then
   plan. A new VM gets its image (loaded first through `imageLoader`
   when set), the data volume `hangar-bay-<name>-data` at
   `/var/lib/pilot`, `--init /sbin/init`, the egress and the published
   ports.
2. **boot**: wait up to 2 minutes until systemd says `running` or
   `degraded`.
3. **proxy env**: `proxy::access` gets the CA, the egress and the login.
   As root over stdin, `up` writes the CA bundle
   (`/var/lib/hangar/ca-bundle.crt`: the image's CAs plus the proxy's),
   then `/etc/hangar/proxy.env` (root:pilot 0640: the proxy URL with the
   login, `NO_PROXY`, the CA variables, and the CA's SHA-256 in a header,
   so a new CA alone changes it too). Each replaces the old file only
   when it changed. Then it waits for Docker.
4. **packages**: reconcile hangar's nix profile (below).
5. **copies**: the `copy` mounts (`src/files.rs`).
6. **env**: `/etc/hangar/bay.env`, the bay's `env`.
7. **setup checks**: each app's setup `check`; an app that fails it is
   skipped in the next step.
8. **apps**: start each run entry as the transient user unit
   `hangar-run-<name>.service` in pilot's user manager. Its description
   holds a hash of the command, `bay.env`, the copies record, the proxy
   env and the CA bundle, so an entry whose inputs changed restarts.
   Units no app lists are stopped.

`down` stops bays, then proxies. `restart` is `down`, then `up`. `rm`
stops and removes the VM; with `--data` it also deletes the bay's folder
and data volume, or the proxy's folder.

### Package reconcile

hangar records which element of its profile
(`/nix/var/nix/profiles/hangar`) each installable became, in a record on
the VM disk. Each `up` removes recorded elements no longer listed and
installs missing ones (`nix profile add`, with `NIXPKGS_ALLOW_UNFREE=1`
for that step only); elements it didn't install are never touched. The
package cache at `/var/cache/hangar` is an extra substituter of lower
priority; cached paths still need a trusted signature. After an install,
the profile's closure is copied into the cache; a failed copy only warns.

## Mounts

`mounts::resolve` checks each `ro`/`rw` mount before anything is created:
the source resolved (symlinks followed) and as written, against your
home, the data and cache dirs (only the bay's own folders allowed), the
credentials names, and for writable mounts your home's dot directories
and `~/Library`. A missing source is created 0700 after its would-be path
passes. hangar adds its own: the home at `/home/pilot` and the package
cache at `/var/cache/hangar`, both writable. Pilot (uid 1000) owns every
mount but the cache, which is root's.

`copy` mounts go through `files::copy`: the whole set is planned first
(every symlink followed and each resolved target checked, the secret scan,
1 MiB per file, 10 MiB and 1000 entries in total), then files whose entry
is gone are removed and changed ones written atomically, contents on
stdin. `bays/<name>/copies` records a hash per file; a new VM clears it.

## State

Every path comes from `src/dirs.rs`, per the XDG Base Directory spec.

- `$XDG_DATA_HOME/hangar` (0700):
  - `bays/<name>/`: `vm` (the VM record: `image`, `egress`, `resources`,
    `port` and `mount` lines), `copies` and `home/`;
  - `proxies/<name>/`: `vm` (with the `fingerprint`) and `data/`, the
    kind's own files.
- `$XDG_CACHE_HOME/hangar/bays/<name>`: the bay's package cache. The data
  and cache dirs must not overlap: the mount rules exempt a bay's own
  folder in one and refuse everything else in both.
- `$XDG_STATE_HOME/hangar`: `hangar.log` (0600, opened without following
  symlinks) and `lock`, held by every command that changes state.
- `Hangar::leftovers`: folders in `bays/` or `proxies/` with no config
  entry. The sandbox can't list VMs, so this is how they're found.
- VM disks: a bay's root disk holds `/nix`, hangar's profile and its
  record. They are lost with the VM, so a new VM reinstalls cleanly. The
  data volume (`/var/lib/pilot`, Docker's storage) is ext4 of its own,
  because overlayfs can't stack on the image's layers.

## Who runs what in a bay

Apps never run as root. Every bay `exec` names its user.

- root: the CA bundle, the proxy env, the package reconcile and cache
  fill, the env file.
- `pilot` (uid 1000, home `/home/pilot`): run entries (units in its own
  user manager), setup and its check, copies, `exec`, `logs` and `setup`.
  Docker is pilot's rootless daemon; nix goes through `nix-daemon`, which
  doesn't trust pilot. No sudo.

Root never follows, sources or executes a path pilot can write: mount and
copy targets are in `~`, root's execs name absolute programs and never
start a login shell, and pilot's folders are declared in the image under
root-owned parents.

## The bay image

`nix/bay/configuration.nix` is the bay as a NixOS system, and
`nix/image.nix` packs it (`dockerTools.streamLayeredImage`, with the Nix
DB) behind `/sbin/init`. Its `Env` is only
`PATH=/run/wrappers/bin:/run/current-system/sw/bin`, the `PATH` of root's
`sh -c` steps, so it never holds hangar's profile.

- Boot: `msb create --init /sbin/init --tmpfs /run`. agentd sets up the
  VM (mounts, network, `/etc/hosts`, `resolv.conf`), appends to
  `/etc/profile` and the CA bundle, then execs `/sbin/init`. Both
  appended files are copies, so the append never writes into the store.
  hangar never restores snapshots: they lose `--init`.
- Daemons: socket-activated `nix-daemon` and pilot's rootless `docker`
  (lingering, so it starts without a login). Both read
  `EnvironmentFile=-/etc/hangar/proxy.env`. The `hangar-proxy-env.path`
  unit watches that file and restarts both when hangar replaces it; it
  resets docker's start limit first, since boot and the first proxy env
  already use two of its three starts a minute.
- Shells: `msb exec` is no PAM login, so `environment.extraInit` sets
  `USER`, `LOGNAME` and `XDG_RUNTIME_DIR` and sources `proxy.env` and
  `bay.env` with `set -a`.
- Privileges left: `newuidmap`/`newgidmap` for rootless Docker and
  setuid `unix_chkpwd`; `su`, `sg`, `newgrp`, `mount`, `umount` and sudo
  are off.
- Known gap: with `sandbox = false`, pilot's nix builds run unsandboxed
  as nixbld.

## Config

`hangar.json` is the only config (lookup in
[configuration.md](configuration.md)). `config::resolve` checks the top
level (`proxies`, `defaults`, `bays`, `apps`, `sandbox`), merges it over
`config/defaults.json` (whose `bay` goes under each bay), checks each
proxy's kind and location and each bay's keys, picks each bay's proxy
(its own, `null`, or `defaults.proxy`; none is an error), and applies the
bay's apps (`apps::apply`). `Hangar::load` then builds the proxies and
refuses two things publishing one host port. No code outside
`config/apps/` knows an app by name.

## Files

- `src/main.rs`: argument parsing (clap) and dispatch.
- `src/commands.rs`: what each command does.
- `src/hangar.rs`: `Hangar`, the shared context (settings, data dir,
  cache root, sandbox, proxies); picks the sandbox backend.
- `src/config.rs`: lookup, merge, validation.
- `src/apps.rs`: app definitions and their merge into a bay.
- `src/proxy/mod.rs`: the traits, the registry, `access`;
  `src/proxy/managed.rs` runs a managed proxy's VM; `src/proxy/ca.rs`
  loads and checks CAs; one file per kind; `src/proxy/fake.rs` for tests.
- `src/bay.rs`: a bay's steps.
- `src/lifecycle.rs`: plans, outcomes and changes.
- `src/status.rs`: what `ps` and `inspect` report, and `ps`'s events.
- `src/output.rs`: renders results as text or `--json` and picks the exit
  code ([cli.md](cli.md)).
- `src/packages.rs`, `src/files.rs`, `src/mounts.rs`: packages, copies
  and mounts.
- `src/sandbox/mod.rs`: the `Sandbox` trait and `VmSpec`;
  `src/sandbox/msb.rs` the msb backend, `src/sandbox/fake.rs` the test
  double ([sandbox-backends.md](sandbox-backends.md)).
- `src/vm_record.rs`, `src/state.rs`, `src/dirs.rs`, `src/lock.rs`,
  `src/logger.rs`: records, the data dir layout, XDG paths, the lock and
  the log.
- `src/secret.rs`: `Secret` (redacting `Debug`, no `Display`) and the
  secret scan `env` and copies share.
- `src/http.rs`, `src/process.rs`, `src/json.rs`, `src/error.rs`: helpers.
- `config/defaults.json`, `config/apps/*.json`: the only copy of the
  defaults and the built-in apps.
- `nix/bay/configuration.nix`, `nix/image.nix`: the bay image.
- `nix/module.nix`, `nix/nixos.nix`, `nix/darwin.nix`: the modules;
  `nix/cli.nix` builds the binary, `nix/microsandbox.nix` packages msb.
- `tests/cli.rs`: integration tests; `nix/tests/module.nix` renders the
  module and diffs it against `tests/module.json`, which a unit test in
  `src/config.rs` also loads.
