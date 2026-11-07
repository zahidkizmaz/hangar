# Configuration

hangar reads one JSON file: `$HANGAR_CONFIG`, else the file the Nix module
renders (`HANGAR_NIX_CONFIG`), else `$XDG_CONFIG_HOME/hangar/hangar.json`
(`~/.config/hangar/hangar.json`; `hangar init` writes a starter). It holds
only what you set: missing keys come from `config/defaults.json`, built
into the binary. Unknown settings are an error, so typos fail loudly.

The Nix module takes the same keys as options (see [nix.md](nix.md)).

## Settings

| Key | Default |
| --- | --- |
| `proxies` | `{}`: name → proxy, see [Proxies](#proxies) |
| `defaults.proxy` | none: the proxy of every bay that doesn't set `proxy` |
| `bays` | one bay, `default`: a list of bays, see below |
| `apps` | `{}`: your own apps, see [Your own apps](#your-own-apps) |
| `sandbox.backend` | `"msb"`: microsandbox, the only backend so far |

Each entry of `bays` is one VM, `hangar-bay-<name>`; `up` and `ps` take
them in this order.

| Bay key | Default |
| --- | --- |
| `name` | required |
| `proxy` | `defaults.proxy`; a proxy name, or `null` for the open internet |
| `apps` | `[]`: see [Apps](#apps) |
| `packages` | `[]`: see [Packages](#packages) |
| `env` | `{}`: plain variables for the bay, never secrets |
| `mounts` | `{}`: VM path → `{"host", "mode"}`, see [Mounting folders](#mounting-folders) |
| `ports` | `{}`: app port name → host port, for two bays running the same app |
| `image` | the image released with this hangar, see [The bay image](#the-bay-image) |
| `imageRepository` | `ghcr.io/zahidkizmaz/hangar-bay`: where that release image is |
| `imageLoader` | none: a program streaming an image archive to load first (the Nix module's `imagePackage` sets it) |
| `cpus`, `memory`, `disk` | `4`, `"6G"`, `"40G"` (`disk` sizes the root disk and the data volume) |

Bays and proxies share one namespace. A name is lowercase letters and
digits joined by single `-`, at most 32 characters.

`env` keys are `UPPER_SNAKE_CASE`. hangar refuses the ones it sets itself
(`PATH`, `HOME`, the proxy and CA variables, the `NIX_*` ones it uses,
`HANGAR_*`) and values that look like real secrets.

## Proxies

A proxy is what a bay's traffic goes through. Every entry has a `kind`
and says where it runs: `"run": "managed"` (hangar runs it in its own VM,
`hangar-proxy-<name>`) or `"url"` (it already runs on your machine). The
other keys belong to the kind:

```json
{
  "proxies": {
    "vault": { "kind": "infisical", "run": "managed" },
    "corp": { "kind": "http", "url": "http://127.0.0.1:3128" }
  },
  "defaults": { "proxy": "vault" },
  "bays": [
    { "name": "work" },
    { "name": "docs", "proxy": "corp" },
    { "name": "scratch", "proxy": null }
  ]
}
```

A bay's egress is a choice. `work` uses the default, `docs` names its
proxy, and `scratch` (`null`) reaches the open internet directly. A bay
with no `proxy` and no `defaults.proxy` is an error, so nobody ends up
direct by omission. The kinds and their keys are in
[proxy-kinds.md](proxy-kinds.md).

A proxy's own tokens never go in the config. They live in the OS
keychain (see [Proxy tokens](proxy-kinds.md#proxy-tokens)): `hangar up`
asks for a missing one, `hangar secret set NAME` replaces them.

## What `up` changes

`hangar up` applies the config. Some changes need a new VM; `up` then
recreates it and prints why (`recreated (image, mounts)`):

- a bay's `image`, `proxy`, `ro`/`rw` mounts, `ports`, `cpus`, `memory`
  or `disk`;
- a managed proxy's image, ports, or its fingerprint (for `infisical`,
  its proxy token).

Everything else is applied to the running VM and printed as `updated
(…)`: `packages`, `env`, `copy` mounts, apps, and the proxy's CA or login.

A recreate keeps the bay's home, its package cache and its data volume.
Changing `disk` replaces the data volume (`data disk replaced`). A
managed proxy keeps its data folder, except when its fingerprint
changed (`recreated (fingerprint)`): what an `infisical` proxy saved
belongs to the old token, so it starts empty. A managed `agent-vault`'s
data is never emptied.

## Where hangar keeps things

Every path follows the
[XDG Base Directory spec](https://specifications.freedesktop.org/basedir-spec/latest/);
an unset or relative variable falls back to the spec's default.

| What | Where |
| --- | --- |
| config | `$XDG_CONFIG_HOME/hangar/hangar.json` |
| a bay's records and home | `$XDG_DATA_HOME/hangar/bays/<name>/` (`vm`, `copies`, `home/`) |
| a proxy's records and data | `$XDG_DATA_HOME/hangar/proxies/<name>/` (`vm`, `data/`) |
| a bay's package cache | `$XDG_CACHE_HOME/hangar/bays/<name>` |
| hangar's log (0600) and lock | `$XDG_STATE_HOME/hangar/hangar.log`, `…/lock` |

The data dir and the cache dir must not overlap.

## Apps

An app is what one tool needs in a bay, as data: packages, plain env, a
setup step, a long-running command and ports. List the ones a bay wants
in its `apps`.

| Built-in app | Brings |
| --- | --- |
| `claude-code` | `github:numtide/llm-agents.nix#claude-code`; `CLAUDE_CODE_OAUTH_TOKEN=hangar-placeholder` |
| `codex` | `github:numtide/llm-agents.nix#codex`; `OPENAI_API_KEY=hangar-placeholder`; a setup that logs Codex in with that placeholder |
| `paperclip` | `github:numtide/llm-agents.nix#paperclip`; `PAPERCLIP_DEPLOYMENT_MODE=authenticated`, `PAPERCLIP_DEPLOYMENT_EXPOSURE=private`, `PAPERCLIP_BIND=lan`; setup `paperclipai onboard`; runs `paperclipai run`; port 3100 (web UI) |

The placeholders are what the tool sends; the bay's proxy swaps in the
real credential. Which hosts a bay may reach, and with which credential,
is the proxy's configuration, not hangar's.

How a bay's apps merge with its own settings:

- `packages`: the apps' packages, then the bay's, without duplicates.
- `env`: the apps' variables, then the bay's, which win per name. Two of a
  bay's apps setting one variable to different values is an error.
- `run`: one entry per app that has a command, named after the app.
- `ports`: unique by name in a bay; the bay's `ports` moves one to another
  host port. No two bays or managed proxies may publish the same host
  port.

### Your own apps

`apps` adds an app, or replaces a built-in of the same name whole:

```json
{
  "apps": {
    "web": {
      "packages": ["nixpkgs#nodejs"],
      "run": "npx serve -l 3000",
      "ports": [{ "name": "web", "vm": 3000, "purpose": "Web UI" }]
    }
  },
  "bays": [{ "name": "work", "proxy": null, "apps": ["web"] }]
}
```

| App key | Meaning |
| --- | --- |
| `packages` | flake installables, merged into the bay's |
| `env` | plain variables, checked like a bay's `env` |
| `setup` | `{command, check}`: a one-time interactive step. While `check` fails, `up` doesn't start the app and `ps` reports it; `hangar setup BAY APP` runs `command` with your terminal |
| `run` | the long-running command, a systemd user unit in the bay |
| `ports` | `[{name, vm, host?, purpose, http?}]`: `host` defaults to `vm` |

App and port names are lowercase letters, digits and `-`.

## Packages

The bay image ships no agent CLIs. A bay's `packages` lists flake
installables, and `up` installs them into hangar's own profile on the VM
disk (`/nix/var/nix/profiles/hangar`, first on `PATH`). Removing an entry
uninstalls it; packages you installed by hand are left alone. Listing an
unfree package, such as Claude Code, means you accept its license.

Packages download through the bay's proxy like everything else. The
package cache, `$XDG_CACHE_HOME/hangar/bays/<name>` mounted at
`/var/cache/hangar`, keeps them on your machine, so a new VM gets them
from there first. It is never trusted on its own: a cached package is used
only when its upstream signature checks out.

## A bay's home

A bay's home (`~` in the bay, `/home/pilot`) is
`$XDG_DATA_HOME/hangar/bays/<name>/home` on your machine, created 0700
and mounted writable. App data there (`~/.claude`, `~/.paperclip`, …)
survives a new VM. It is hangar's own home, never yours: to give a bay
your config, use a `copy` mount.

Pilot's data, Docker's included, lives on the bay's data volume,
`hangar-bay-<name>-data`, mounted at `/var/lib/pilot`. It survives a
recreate unless `disk` changes. `hangar rm NAME --data` deletes the home
and the data volume; the package cache is always kept.

## Mounting folders

`mounts` maps a VM path in `~` to a host path and a mode:

- `ro`: mounted read-only. The bay sees your changes live.
- `copy`: copied into the bay on every `up`, one way. See
  [Your config files in the VM](#your-config-files-in-the-vm).
- `rw`: mounted writable, only with `"unsafe": true`, see below.

```json
"mounts": {
  "~/notes": { "host": "~/notes", "mode": "ro" },
  "~/.claude/CLAUDE.md": { "host": "~/dotfiles/claude/CLAUDE.md", "mode": "copy" }
}
```

**A writable mount is the one way an agent can leave files your machine
runs.** Anywhere in that folder, at any time, it can write a git hook, a
repo's `core.fsmonitor` (run by your next `git status`), an `.envrc` or a
script, and they run as you, outside the bay. So `rw` is refused unless
the mount says `"unsafe": true`, and `up` warns about it every time. To
work on code, keep the repo in the bay (`~` survives a new VM) and push
through the proxy; read your files with `ro`.

Every mount is also refused, before anything is created, when it is:

- any mount of your home or a folder above it, of hangar's data or cache
  dir (or a folder holding them), of another bay's folders, or of a
  credentials place (`.ssh/`, `.gnupg/`, `.aws/`, `.kube/`, `.config/gh/`,
  `.docker/config.json` and the names in the next section);
- writable and outside your home, or under a dot directory
  (`~/.config`, `~/.local`, …) or `~/Library`;
- a source that is neither a regular file nor a directory (sockets,
  devices).

`ro` and `rw` follow symlinks to the real path and mount that. The name
rules apply to both the path you wrote and the resolved target, so a
stow link `~/.bashrc` is refused as writable wherever it points. A
missing source is created (0700), but only after the path it would have
passes the rules.

Symlinks inside a mounted folder that point outside it stay broken in the
bay: the VM sees the link, not its target (a virtiofs limit). Mount the
target itself, or use `copy`.

VM paths can't use `..`, mounts can't nest or cover `~`, and a VM path is
either mounted or copied, never both. `ro` and `rw` mounts are fixed when
the VM is created, so changing them recreates it.

### Your config files in the VM

A `copy` mount copies a file or a directory from your machine into the
bay on every `up`. hangar rewrites what changed and removes what it copied
when the entry is gone; files you created in the bay are left alone.
`hangar cp` copies one ad hoc through the same checks.

Everything copied is visible to the agents, so `up` refuses the whole
step, before copying anything, when a file:

- is a credentials file by name or place: `.credentials.json`,
  `credentials.json`, `.env`, `.env.*`, `*.pem`, `*.key`, `*.p12`, `*.pfx`,
  `*.kdbx`, `id_rsa*`, `id_ed25519*`, `id_ecdsa*`, `.netrc`,
  `.git-credentials`, `hosts.yml`, `.npmrc`, `.pypirc`, or anything under
  the credentials places above;
- contains what looks like a credential (a private key, or a token such
  as `ghp_…`, `sk-ant-…`, `AKIA…`);
- is in hangar's data or cache dir (but the bay's own folders), isn't a
  regular file or directory, or is a symlink loop;
- is over 1 MiB, or the copy passes 10 MiB in total or 1000 entries.

`copy` follows symlinks and checks every resolved target, its name and
the link's. Never copy a login such as `~/.claude/.credentials.json`.

## The bay image

The image is a NixOS system (`nix/bay/configuration.nix`, packed by
`nix/image.nix`) that each bay boots into, with generic tools only:
docker, nix, direnv with nix-direnv, git, gh, uv and common CLI tools
(ripgrep, fd, jq, yq, ast-grep, just, make, …). CI publishes it to
`ghcr.io/zahidkizmaz/hangar-bay`.

By default a bay uses the image tagged with hangar's own version
(`ghcr.io/zahidkizmaz/hangar-bay:v<version>`), so the CLI and the image
always match. A fork sets `imageRepository`, or `image` to any ref (a
digest pins it). Any image must be built from
`nix/bay/configuration.nix`: hangar boots its systemd and runs apps as
units of its user `pilot` (uid 1000). To build and load one locally, see
[Local images](nix.md#local-images).
