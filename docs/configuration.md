# Configuration

`hangar.json` is read from `$HANGAR_CONFIG`, else the file the Nix module
generates, else `$XDG_CONFIG_HOME/hangar/hangar.json`
(`~/.config/hangar/hangar.json`; `hangar init` writes a minimal one). It
only holds what you set: missing keys use the defaults in
`config/defaults.json` (built into the binary), so updates to them reach
you. Unknown settings are an error, and so are unknown keys in a route's
`auth`, so typos don't go unnoticed.

The Nix module takes the same keys as options (see [nix.md](nix.md)). The
examples below use its syntax; the same keys go in `hangar.json`.

## Settings

Every setting, with its default. `bays` is a list; each bay is one VM,
`hangar-bay-<name>`, and `up` and `status` take them in this order.
Without any there is one bay, `default`. `tower` is everything the bays
share.

| Key | Default |
| --- | --- |
| `bays[].name` | required: lowercase letters and digits joined by single `-`, at most 32 |
| `bays[].apps` | `[]`: see [Apps](#apps) |
| `bays[].ports` | `{}`: app port name → host port (two bays with one app) |
| `bays[].image` | the image released with this hangar (`…:v<version>`), see [The bay image](#the-bay-image) |
| `bays[].imageRepository` | `ghcr.io/zahidkizmaz/hangar-bay`: where that release image is |
| `bays[].imageLoader` | none: a program streaming an image archive to load first (the Nix module's `imagePackage` sets it) |
| `bays[].packages` | `[]`: see [Choosing your agents](#choosing-your-agents) |
| `bays[].env` | `{}`: plain variables for the VM (never secrets) |
| `bays[].run` | `{}`: name → long-running command `up` keeps started |
| `bays[].files` | `{}`: VM path → host config file or directory, see [Your config files in the VM](#your-config-files-in-the-vm) |
| `bays[].mounts` | `{}`: VM path → `{"host", "writable"}`, see [Mounting folders](#mounting-folders) |
| `bays[].home` | `true`: keep the bay's home in `<stateDir>/bays/<name>/home` (`false`: VM disk), see [A bay's home](#a-bays-home) |
| `bays[].cache` | `true`: the bay's package cache in `$XDG_CACHE_HOME/hangar/bays/<name>` (`false`: off), see [Package cache](#package-cache) |
| `bays[].cpus`, `.memory`, `.disk` | `4`, `"6G"`, `"40G"` (`disk` twice: the root and Docker's own disk) |
| `tower.routes` | `[]`: your own hosts, see [Allowing another host](#allowing-another-host) |
| `tower.credentialFiles` | `{}`: vault credential key → file holding its value, e.g. `{"GITHUB_TOKEN": "/path/to/github-token"}` |
| `tower.masterPasswordFile` | none: generated into the OS keychain, see [The master password](usage.md#the-master-password) |
| `tower.backend` | `"agent-vault"`, the only broker so far |
| `tower.agentVault.image` | `infisical/agent-vault:<version>@sha256:…` (pinned) |
| `tower.agentVault.adminPort`, `.proxyPort` | `14321`, `14322` |
| `appDefinitions` | `{}`: your own apps, see [Your own apps](#your-own-apps) |
| `stateDir` | `$XDG_DATA_HOME/hangar` |
| `sandbox.backend` | `"msb"`: microsandbox, the only backend so far |

Credentials you don't keep in files you store with `hangar credential set
NAME` (see [Where secrets live](usage.md#where-secrets-live)).

A bay's ports, CPUs, memory, disk and mounts are fixed when its VM is
created: after changing them, run `hangar destroy NAME && hangar up NAME`
(the home is kept). Its one way out, `tower.agentVault.proxyPort`, is
checked: until the VMs are recreated, `up` refuses to start with another
port. It also refuses an existing VM without a record of how it was
created (`bays/<name>/vm`, `tower-vm` in `stateDir`), with the same fix.

## Where hangar keeps things

Every path follows the [XDG Base Directory spec](https://specifications.freedesktop.org/basedir-spec/latest/),
resolved when hangar runs; a variable that isn't set (or isn't absolute)
falls back to the spec's default:

| What | Where |
|---|---|
| config (`hangar.json`) | `$XDG_CONFIG_HOME/hangar` (`~/.config/hangar`) |
| `stateDir`: vault data, tokens, records | `$XDG_DATA_HOME/hangar` (`~/.local/share/hangar`) |
| a bay's home (`home`) | `<stateDir>/bays/<name>/home` |
| a bay's package cache (`cache`) | `$XDG_CACHE_HOME/hangar/bays/<name>` (`~/.cache/hangar/bays/default`) |
| lock: one hangar changes state at a time | `$XDG_STATE_HOME/hangar/lock` (`~/.local/state/hangar`) |
| login autostart log (macOS) | `$XDG_STATE_HOME/hangar/hangar.log` (`~/.local/state/hangar`); Linux: the journal |

## Apps

An app is what one tool needs in a bay, as data: its packages, the
hosts it may reach (its routes) and the credential each one gets, plain
env, a long-running command and its ports. List the ones a bay wants in its
`apps`:

```nix
services.hangar.bays = [
  { name = "default"; apps = [ "nix" "github" "claude-code" "paperclip" ]; }
];
```

### Built-in apps

**hangar ships no routes.** The tower allows nothing until an app or your
`tower.routes` names a host. The built-ins are in `config/apps/`:

| App | Brings |
|---|---|
| `nix` | routes to `cache.nixos.org`, `cache.numtide.com` (llm-agents.nix's cache, which the image trusts), `channels.nixos.org`, `releases.nixos.org` |
| `github` | routes to `api.github.com`, `github.com`, `codeload.github.com`, `objects.githubusercontent.com`, `release-assets.githubusercontent.com`, passthrough |
| `github-token` | the same, with `GITHUB_TOKEN` injected on `api.github.com` (bearer) and `github.com` (git over HTTPS) |
| `docker` | routes to Docker Hub (`*.docker.io`, `*.docker.com`, `production.cloudfront.docker.com`) |
| `python` | routes to `pypi.org`, `files.pythonhosted.org`, `releases.astral.sh` (uv's Python downloads) |
| `node` | a route to `registry.npmjs.org` |
| `rust` | routes to `index.crates.io` (cargo's sparse index), `static.crates.io` (crate downloads), `static.rust-lang.org` (rustup toolchains) |
| `claude-code` | `github:numtide/llm-agents.nix#claude-code`; `api.anthropic.com` ← `CLAUDE_CODE_OAUTH_TOKEN` |
| `codex` | `github:numtide/llm-agents.nix#codex`; `api.openai.com` ← `OPENAI_API_KEY`; a setup that logs Codex in with the key's placeholder |
| `paperclip` | `github:numtide/llm-agents.nix#paperclip`; `PAPERCLIP_DEPLOYMENT_MODE=authenticated`, `PAPERCLIP_DEPLOYMENT_EXPOSURE=private`, `PAPERCLIP_BIND=lan`; runs `paperclipai run`; port 3100 (web UI) |

A bay with `packages` (`claude-code`, `codex` and `paperclip` bring some)
needs `nix` and `github` (or `github-token`) in some bay: `hangar up` refuses it
otherwise. Routes are shared, so one bay's apps serve every bay. Use
`github` or `github-token`, not both; for `github-token`, store the token
with `hangar credential set GITHUB_TOKEN` or name its file in
`tower.credentialFiles`. How to set up Claude Code, Codex and Paperclip
is in [usage.md](usage.md#claude-code-codex-and-paperclip). `rust` covers
fetching crates and toolchains, not `cargo search` or `cargo publish`
(those use `crates.io`; add a route if you need them).

`hangar status --all` shows each bay's apps with their hosts and the
credential each one gets (names only, never values):

```
  APP          HOST               CREDENTIAL
  claude-code  api.anthropic.com  CLAUDE_CODE_OAUTH_TOKEN
  paperclip    -                  -
```

### How apps merge

How a bay's apps merge with its settings, in `apps` order, then the bay's
own:

- `packages`: the apps' packages, then yours, without duplicates.
- `env`: the apps' variables; yours win per name. Two of a bay's apps
  setting one variable to different values is an error (apps in different
  bays may), and an app can't set a credential's placeholder variable or
  one hangar sets itself (`PATH`, `HOME`, the proxy and CA variables,
  `NIX_*`, `HANGAR_*`).
- `run`: an entry per app with a command, named after the app; yours win
  per name.
- `ports`: each app's ports, unique by name in a bay; the bay's `ports`
  moves one to another host port.
- Routes: every bay's apps' routes go to the one tower. Two apps may bring
  the same route only if it's identical (else enable only one of them), a
  route in `tower.routes` can't take an app route's name, two sources may
  not route one host, and an app's route may not use a credential another
  source's route uses.

### Your own apps

`appDefinitions` adds an app, or replaces a built-in of the same name
whole. List it in a bay's `apps` to enable it:

```nix
services.hangar = {
  bays = [ { name = "default"; apps = [ "nix" "github" "claude-code" "paperclip" ]; } ];
  # Paperclip on host port 3200: a full copy with one change.
  appDefinitions.paperclip = {
    packages = [ "github:numtide/llm-agents.nix#paperclip" ];
    env = {
      PAPERCLIP_DEPLOYMENT_MODE = "authenticated";
      PAPERCLIP_DEPLOYMENT_EXPOSURE = "private";
      PAPERCLIP_BIND = "lan";
    };
    setup = {
      command = "paperclipai onboard";
      check = "test -f ~/.paperclip/instances/default/config.json";
    };
    run = "paperclipai run";
    ports = [ { name = "paperclip"; vm = 3100; host = 3200; purpose = "Paperclip web UI"; } ];
  };
};
```

| Key | Meaning |
|---|---|
| `packages` | flake installables, merged into the bay's `packages` |
| `routes` | `[{name, host, auth}]`, as in [`tower.routes`](#allowing-another-host); no `extra` |
| `env` | plain variables (never secrets), checked like a bay's `env` |
| `credentials` | fixed, non-secret credential values its routes send (e.g. `github-token`'s `GITHUB_GIT_USER = x-access-token`); hangar stores them in the vault while a route uses them, and `credential set`/`rm` refuse them |
| `setup` | `{command, check}`: a one-time interactive step. `up` runs `check` (in a login shell, after the env file) and skips the app's `run` while it fails; `hangar setup NAME` runs `command` with your terminal, then `check` (`--force` runs it even if `check` passes) |
| `run` | the long-running command, named after the app |
| `ports` | `[{name, vm, host?, purpose, http?}]`: `host` defaults to `vm`; `http` (default `true`) makes `status` probe it with a GET, else a TCP connect |

App names, like `run` names, route names and port names, are lowercase
letters, digits and `-`. An app can't copy files or mount folders: that
stays in a bay's own `files` and `mounts`.

## Allowing another host

When an agent hits a host that isn't allowed, the vault answers `403` and
names it. Add it to `tower.routes`:

```nix
services.hangar.tower.routes = [
  # just allow it
  { name = "crates"; host = "index.crates.io"; auth.type = "passthrough"; }

  # or inject a credential (hangar credential set, or tower.credentialFiles)
  {
    name = "jira";
    host = "your-site.atlassian.net";
    auth = { type = "basic"; username = "JIRA_EMAIL"; password = "JIRA_TOKEN"; };
  }
  {
    name = "heroku";
    host = "api.heroku.com";
    auth = { type = "bearer"; token = "HEROKU_API_KEY"; };
  }
];
```

The same entries go under `"tower": {"routes": [...]}` in `hangar.json`.
Wildcards match one label only (`*.docker.io` doesn't match
`a.b.docker.io`). See agent-vault's docs for every auth type and for
path-scoped hosts (`api.example.com/v1/*`).

A path-scoped host lets the bays reach only those paths, and the vault
answers `403` for the rest of that host (an MCP server's route keeps the
bays off its provider's `/register` and `/token`, see
[MCP servers](usage.md#mcp-servers)). That holds only while no other
route covers the whole host: hangar warns when a bare host or a `*.`
wildcard does (`route NAME is scoped to a path, but route OTHER lets bays
reach its whole host`).

A route can't take the name of a route an app brings, and two routes can't
share a host. To use your own auth on a host an app routes (say a private
npm token), leave that app out and add your own route under another name,
or replace the app with an `appDefinitions` entry of the same name. The
tower never reaches loopback or private addresses (agent-vault's
netguard), so a route to `localhost` or a LAN host doesn't open a way to
your machine.

## Choosing your agents

The bay image ships no agent CLIs: you pick them. A bay's `packages` lists
flake installables, and `hangar up` installs them into hangar's own nix
profile on the VM disk (`/nix/var/nix/profiles/hangar`, first on `PATH`), so
only once per VM; a new VM reinstalls them. Removing an entry uninstalls it
on the next `up`; packages you install by hand inside the VM are left alone.

```nix
services.hangar.bays = [
  {
    name = "default";
    apps = [ "nix" "github" ];
    packages = [
      "github:numtide/llm-agents.nix#codex"
      "github:numtide/llm-agents.nix#opencode"
      "nixpkgs#rtk"
    ];
  }
];
```

Packages come through the tower like everything else, so some bay needs
the `nix` and `github` (or `github-token`) apps: `up` says so otherwise.
When the numtide cache misses, nix builds locally and fetches sources from
other hosts (npm, for example), which then need routes too. Private
`github:` inputs aren't supported: the image sets no nix `access-tokens`,
so the token stays in the tower.

Claude Code, Codex and Paperclip come as [apps](#apps), which bring their
hosts and settings too; their provider auth goes through agent-vault as
well.

Unfree packages install too (`NIXPKGS_ALLOW_UNFREE` is set for that step
only): listing one, such as Claude Code, means you accept its license.

## A bay's home

A bay's home (`~` in the VM, `/home/pilot`) is kept on your machine in
`<stateDir>/bays/<name>/home`, i.e. `~/.local/share/hangar/bays/default/home`
(`home`), created with mode 0700 and mounted writable. Whatever the apps
there keep in their home (`~/.claude`, `~/.paperclip`, `~/.codex`, …)
survives `hangar destroy`, and you can look at it from your machine. It's
**hangar's own home, separate from yours**: hangar never mounts or copies
your `~/.claude` or other config by default. To give the VM your own
config, copy it with [`files`](#your-config-files-in-the-vm) or mount a
folder with [`mounts`](#mounting-folders).

- `hangar destroy` keeps it; `hangar destroy NAME --state` deletes that
  bay's folder, and `hangar destroy --state` all of `stateDir`. To start
  only the apps fresh, `rm -rf ~/.local/share/hangar/bays/default/home`
  (with the VMs destroyed).
- `home = false` keeps the home on the VM's own disk instead (lost
  with the VM). Changing `home` needs `hangar destroy NAME && hangar up NAME`.
- Its place is fixed: to keep app data elsewhere, mount a folder of yours
  onto a path in the home with [`mounts`](#mounting-folders), e.g.
  `~/work/claude` at `~/.claude`.
- Don't log in to tools inside the VM: the login would be stored in this
  folder. Use the vault (see
  [Claude Code, Codex and Paperclip](usage.md#claude-code-codex-and-paperclip)).
- Untested: databases on a host share (Paperclip's embedded Postgres under
  `~/.paperclip`) may not like its file locking. If one misbehaves, point
  the app's data at the VM disk (e.g.
  `env.PAPERCLIP_HOME = "/var/lib/pilot/paperclip"`, pilot's folder on
  the VM disk) or set `home = false`.

## Package cache

Nix packages from `packages` are cached on your machine in
`$XDG_CACHE_HOME/hangar/bays/<name>`, i.e. `~/.cache/hangar/bays/default`
(`cache`, created 0700, mounted writable). Each bay has its own
cache, never shared: a bay can only reach its own. Packages
in a bay live on its own disk, so a new VM (`hangar destroy NAME &&
hangar up NAME`) would download all of them again; with the cache it gets
them from your machine first and only goes to the internet for what's
missing. `up` fills the cache after it installs something; `hangar status --all`
shows its size.

The cache is writable from the VM, so it's never trusted on its own: each
package keeps the signature of the cache it came from (cache.nixos.org,
cache.numtide.com), and the VM only uses a cached package whose signature
checks out against those keys. A tampered or unsigned package is ignored
and fetched from the internet instead. hangar never creates a signing key,
so packages built locally in the VM (unsigned) aren't reused.

- Wipe it: `rm -rf ~/.cache/hangar/bays/default`. Turn it off:
  `cache = false`.
- Changing `cache` needs `hangar destroy NAME && hangar up NAME`. `destroy`
  never deletes caches, not even with `--state`.

## Your config files in the VM

`files` copies config files or whole directories from your machine
into the bay on every `hangar up` (VM path → host path; `~` is the VM
user's home on the left and yours on the right):

```nix
services.hangar.bays = [{
  name = "default";
  files = {
    "~/.claude/CLAUDE.md" = "~/dotfiles/claude/CLAUDE.md";
    "~/.claude/settings.json" = "~/dotfiles/claude/settings.json";
    "~/.claude/agents" = "~/dotfiles/claude/agents";
  };
}];
```

**Never copy `~/.claude/.credentials.json`** (or any login): log in through
the vault instead (see
[Claude Code, Codex and Paperclip](usage.md#claude-code-codex-and-paperclip)).
Paperclip is configured by its app's variables and its UI, so it needs
`files` only for a config file of its own.

Changes reach the VM on the next `up` or `hangar copy`: hangar rewrites
files whose source changed and removes files it copied whose entry is gone.
A running app keeps the old config until `hangar restart <name>`; `up` tells
you when that's needed. `hangar copy SRC [DEST]` pushes one file ad hoc
through the same checks. Files you created in the VM yourself are left
alone. Everything the agents can read here is visible to them, so hangar
refuses the whole step, before copying anything, when a file:

- is a credentials file by name or place: `.credentials.json`,
  `credentials.json`, `.env`, `.env.*`, `*.pem`, `*.key`, `*.p12`, `*.pfx`,
  `id_rsa*`, `id_ed25519*`, `id_ecdsa*`, `*.kdbx`, `.netrc`,
  `.git-credentials`, `hosts.yml`, `.npmrc`, `.pypirc`,
  `.docker/config.json`, or anything under `.ssh/`, `.gnupg/`, `.aws/`,
  `.config/gh/`, `.kube/`;
- contains what looks like a credential (a private key, or a token such as
  `ghp_…`, `sk-ant-…`, `AKIA…`);
- is a symlink, a special file, or hangar's own state directory;
- is over 1 MiB, or the total passes 10 MiB or 1000 files.

VM paths must be in `~` (`/home/pilot`, the bay user's home) and can't
use `..`: everything an app may write is the app's own, never a file
hangar or the system runs.

## Mounting folders

`files` copies one way: changes an app makes in the VM stay there and
are overwritten by the next copy. When the app itself maintains a folder,
mount it instead: `mounts` mounts a host directory into the VM, live
both ways (VM path → host directory; `~` as in `files`).

- **You maintain it** (your `CLAUDE.md`, skills): copy it with `files`.
- **The app maintains it** (Paperclip's data): mount a dedicated directory,
  writable.

```nix
services.hangar.bays = [{
  name = "default";
  mounts = {
    # Read-only: the VM sees changes you make, but can't write.
    "~/.claude/skills" = { host = "~/dotfiles/claude/skills"; };
    # Shared with another tool on your machine, writable.
    "~/shared" = { host = "~/work/shared"; writable = true; };
  };
}];
```

App data (Paperclip's `~/.paperclip`) already lives in
[the bay's home](#a-bays-home), so it needs no mount of its own.

**A writable mount is the one way an agent can write files on your
machine.** If a host program reads its config from that directory, an agent
could plant something there (a hook, a startup script) that runs outside the
sandbox. So hangar refuses, before creating the VM:

- as any mount: your home itself or anything above it, hangar's state
  directory and package caches (or a folder holding either), another bay's
  home or cache, and credentials places (`.ssh/`, `.gnupg/`, `.aws/`,
  `.config/gh/`, `.kube/`, and the `files` deny-list names);
- as a writable mount: anything outside your home, and anything under a dot
  directory (`~/.claude`, `~/.config`, `~/.local`, …) or `~/Library`, where
  host programs keep their config. Use an ordinary directory made for it,
  such as `~/work/shared`. The only exceptions are the bay's own folders:
  `<stateDir>/bays/<name>/home` (nothing else in `stateDir`) and
  `$XDG_CACHE_HOME/hangar/bays/<name>` (not `~/.cache` itself, another
  bay's cache or another app's).

Sources are checked where they really point (symlinks resolved). A missing
source is created by `up` (mode 0700), but only after the path it would have
passes the same rules; a refused path is never created. A read-only mount
exposes the whole directory to the agents, so keep it narrow. A VM path is
either copied or mounted, never both, and `mounts` don't nest in each
other; they may sit inside the bay's home (microsandbox applies the
enclosing mount first), but not cover it. Mounts are fixed when the VM is
created: after changing them, `hangar destroy NAME && hangar up NAME` (`up`
and `status --all` say so).

## The bay image

The image is a NixOS system (`nix/bay/configuration.nix`, packed by
`nix/image.nix`) that each bay boots into, and holds generic tools only:
docker, nix, direnv + nix-direnv, git, gh, uv and common CLI tools (ripgrep,
fd, jq, yq, ast-grep, just, make, …). It redistributes no agent CLIs; those
come from `packages`. CI publishes it to `ghcr.io/zahidkizmaz/hangar-bay`
(see [development.md](development.md#ci-renovate-releases)).

By default hangar uses the image tagged with its own version
(`ghcr.io/zahidkizmaz/hangar-bay:v<version>`), so the CLI and the image
always match. A fork sets a bay's `imageRepository` to its
own registry, or `image` to any ref (a digest pins it). The VM's disk is
made from the image once: after switching images, `hangar up` warns until you
run `hangar destroy NAME && hangar up NAME`. Any image must be built from
`nix/bay/configuration.nix`: hangar boots its systemd, which restarts the
daemons when hangar writes their proxy env, and apps run as units in
the user manager of its user `pilot` (uid 1000, home `/home/pilot`). To build and load one locally, see
[Local images](nix.md#local-images).
