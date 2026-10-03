# hangar

Locked-down VMs for AI coding agents.

hangar runs [microsandbox](https://microsandbox.dev) VMs on your machine:

```
 your machine (macOS / Linux)
 ├─ hangar-tower      the tower: agent-vault holds the real credentials,
 │                    MITM proxy · 127.0.0.1:14321 admin, :14322 proxy
 ├─ hangar-bay-work   a bay: your agents (see apps, packages) + docker,
 │                    nix, git, gh, uv. Its only allowed connection is the
 │                    tower's proxy port.
 └─ hangar-bay-oss    any number of bays, each with its own home, cache
                      and vault token
```

Bays never reach each other or each other's folders, unless you mount one
host folder into both. They share the tower: every bay can use every route
and credential.

A bay holds placeholders, never secrets.
[agent-vault](https://github.com/Infisical/agent-vault) swaps in the real
credential on the way out, and only for hosts on the allowlist. Everything
else is refused, including direct connections, DNS, your LAN and the vault's
admin port.

## Security model

- **Tokens are the blast radius.** Agents can't steal credentials, but they
  can use them. Give hangar a fine-grained GitHub token that covers only the
  repos agents work on (no `workflow`, `gist` or admin scopes), and protect
  your default branch.
- **Deny by default.** The vault rejects every host that isn't on the
  allowlist with a `403` that names the host.
- **No production access to start with.** Add production APIs later, one at
  a time, read-only and path-scoped, ideally in a separate vault.
- **Keep app UIs behind a login.** App ports are published on `127.0.0.1`,
  so any local process could reach them (the `paperclip` app runs
  Paperclip in authenticated mode).
- **Apps never run as root.** Everything an app, a shell or `files` does
  in a bay runs as the user `pilot` (uid 1000, home `/home/pilot`), with no
  sudo. Docker is pilot's own rootless daemon, and nix goes through a
  daemon that doesn't trust pilot. The VM itself stays the real boundary.
- **A bay is disposable.** Recreate it regularly with
  `hangar destroy NAME && hangar up NAME`. Code leaves only through reviewed
  PRs.
- **Bays share credentials.** Every bay can use every route, so a
  compromised bay can do what any bay can, and one bay can load the shared
  tower for the rest. Use separate machines or vaults where that matters.
  A host folder you mount into two bays is a channel between them; their
  homes and caches never are.

## Install

You need [microsandbox](https://microsandbox.dev) (`msb`). On Linux you also
need access to `/dev/kvm`. hangar itself is a single binary with no runtime
dependencies.

```sh
brew install superradcompany/tap/microsandbox # or see microsandbox.dev

# a release binary (aarch64-darwin, x86_64-linux, aarch64-linux) …
curl -fsSLo ~/.local/bin/hangar \
  https://github.com/zahidkizmaz/hangar/releases/latest/download/hangar-aarch64-darwin
chmod +x ~/.local/bin/hangar
# … or from source
cargo install --git https://github.com/zahidkizmaz/hangar

hangar init # writes a minimal ~/.config/hangar/hangar.json
```

Edit `~/.config/hangar/hangar.json` (`$XDG_CONFIG_HOME/hangar/hangar.json`
when that's set). It only holds what you set; everything
else comes from the built-in defaults, so updates to them reach you:

```json
{
  "bays": [
    { "name": "default",
      "apps": ["nix", "github", "claude-code"],
      "packages": ["nixpkgs#rtk"] }
  ],
  "tower": { "credentialFiles": {} }
}
```

- `bays`: the bays, in order; each lists its [apps](#apps). Without any
  there is one bay, `default`.
- `tower.credentialFiles`: vault credential key → file holding its value,
  e.g. `{"GITHUB_TOKEN": "/path/to/github-token"}`, for credentials you
  keep in files. Others you store with `hangar credential set NAME`.

Then run `hangar up`.

### The master password

You don't create one. On the first `hangar up`, each install generates its
own random password and stores it in the OS keychain: the macOS login
Keychain, or the Secret Service (`secret-tool`, from libsecret) on Linux. It
only ever travels on stdin: never in a process's arguments or a child
process's environment, and never written to disk. hangar calls the keychain
tool by its absolute path, never through `PATH`: `/usr/bin/security` on
macOS, and on Linux `/usr/bin/secret-tool` (the Nix package builds in
libsecret's).

Everything else is re-applied from your config on every `up` (deny mode,
routes, credentials; a credential removed from `credentialFiles` is deleted
from the vault), so a new machine only needs the same config and credential
files. What doesn't carry over is state that lives only inside
the vault, such as OAuth logins pasted into agent-vault.

To supply your own instead, hangar uses the first of these that is set:

1. `HANGAR_MASTER_PASSWORD`: the password itself
2. `HANGAR_MASTER_PASSWORD_FILE`: a file holding it
3. `tower.masterPasswordFile` in the config: an explicit opt-in, e.g. a
   file a secrets manager provides in CI
4. the OS keychain (service `hangar`, account `master-password`;
   `HANGAR_KEYCHAIN_SERVICE` picks another service name)

A password is only generated for a new vault. If vault data exists but the
keychain item is gone, `hangar up` stops instead of generating one, which
would lock that vault: restore the item, or `hangar destroy --state` to
start fresh. `destroy --state` also deletes the keychain item.

### Configuration

`hangar.json` is read from `$HANGAR_CONFIG`, else the file the Nix module
generates, else `$XDG_CONFIG_HOME/hangar/hangar.json`. Missing keys use the defaults
in `config/defaults.json` (built into the binary). Unknown settings are an
error, and so are unknown keys in a route's `auth`, so typos don't go
unnoticed.

Every setting, with its default. `bays` is a list; each bay is one VM,
`hangar-bay-<name>`, and `up` and `status` take them in this order.
`tower` is everything the bays share.

| Key | Default |
| --- | --- |
| `bays[].name` | required: lowercase letters and digits joined by single `-`, at most 32 |
| `bays[].apps` | `[]`: see [Apps](#apps) |
| `bays[].ports` | `{}`: app port name → host port (two bays with one app) |
| `bays[].image` | the image released with this hangar (`…:v<version>`) |
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
| `tower.routes` | `[]`: your own hosts (see [Allowing another host](#allowing-another-host)) |
| `tower.credentialFiles` | `{}`: vault credential key → file |
| `tower.masterPasswordFile` | none: generated into the OS keychain |
| `tower.backend` | `"agent-vault"`, the only broker so far |
| `tower.agentVault.image` | `infisical/agent-vault:<version>@sha256:…` (pinned) |
| `tower.agentVault.adminPort`, `.proxyPort` | `14321`, `14322` |
| `appDefinitions` | `{}`: your own apps (see [Your own apps](#your-own-apps)) |
| `stateDir` | `$XDG_DATA_HOME/hangar` |
| `sandbox.backend` | `"msb"`: microsandbox, the only backend so far |

**hangar ships no routes.** The tower allows nothing until an app or your
`tower.routes` names a host, and the hosts every agent needs come as
route-only apps you list in a bay's `apps`:

| App            | Hosts |
| -------------- | ----- |
| `nix`          | `cache.nixos.org`, `cache.numtide.com` (llm-agents.nix's cache, which the image trusts), `channels.nixos.org`, `releases.nixos.org` |
| `github`       | `api.github.com`, `github.com`, `codeload.github.com`, `objects.githubusercontent.com`, `release-assets.githubusercontent.com`, passthrough |
| `github-token` | the same, with `GITHUB_TOKEN` injected on `api.github.com` (bearer) and `github.com` (git over HTTPS) |
| `docker`       | Docker Hub (`*.docker.io`, `*.docker.com`, `production.cloudfront.docker.com`) |
| `python`       | `pypi.org`, `files.pythonhosted.org`, `releases.astral.sh` (uv's Python downloads) |
| `node`         | `registry.npmjs.org` |

A bay with `packages` needs `nix` and `github` (or `github-token`) in some
bay: `hangar up` refuses it otherwise. Routes are shared, so one bay's
apps serve every bay. Use `github` or `github-token`, not both; for
`github-token`, store the token with `hangar credential set GITHUB_TOKEN`
or name its file in `tower.credentialFiles`.

A bay's ports, CPUs, memory, disk and mounts are fixed when its VM is
created: after changing them, run `hangar destroy NAME && hangar up NAME`
(the home is kept). Its one way out, `tower.agentVault.proxyPort`, is
checked: until the VMs are recreated, `up` refuses to start with another
port. It also refuses an existing VM without a record of how it was
created (`bays/<name>/vm`, `tower-vm` in `stateDir`), with the same fix.

### Where hangar keeps things

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

## First run

```sh
hangar up       # creates the VMs; prints the ports below
hangar status   # the same overview, any time
```

### Accounts and logins

You don't create any account. On the first `up`, hangar creates agent-vault's
admin account itself:

| | |
|---|---|
| email | `owner@hangar.local` (fixed, not a real address) |
| password | random, in `<stateDir>/owner-password` (default `~/.local/share/hangar`, mode 0600) |

Don't change or delete that account or file: hangar logs in with it on every
`up`. To open the vault UI, run `hangar vault-ui`: it copies the password to
the clipboard, prints the email and URL, and opens the page. Without it:
`pbcopy < ~/.local/share/hangar/owner-password` (Linux: `wl-copy <` or
`xclip -sel c <`), then open `http://127.0.0.1:14321`.

You normally don't need the UI: add tokens with `hangar credential set`.
The UI is for looking at services (one per route), the request log, where
each bay shows as its own agent (`hangar-<bay>`), and `403`s.

### Ports

| Port | What | Reachable from |
|---|---|---|
| `127.0.0.1:14321` | vault UI and admin API (`tower.agentVault.adminPort`) | your machine |
| `127.0.0.1:14322` | vault proxy (`tower.agentVault.proxyPort`) | the bays only |
| app ports, e.g. `127.0.0.1:3100` | Paperclip's web UI (the `paperclip` app) | your machine |

Everything binds to `127.0.0.1` only, and two ports can't share a host
port: two bays running Paperclip need `ports.paperclip` in one of them
(`port 3100: work/paperclip and oss/paperclip: set ports.paperclip in bay
oss`). Changing a port takes `hangar destroy NAME && hangar up NAME`.

### Where secrets live

| Secret | Where |
|---|---|
| master password | the OS keychain, generated per install |
| your tokens | inside agent-vault, encrypted (`hangar credential set`) |
| a bay | placeholders only, never a real value |
| Nix config, `hangar.json` | credential *names* only |

Name a credential after the environment variable your tool reads (e.g.
`CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_API_KEY`). hangar then sets
`NAME=hangar-placeholder` in every bay for every credential a route
injects, and the vault swaps in the real value on the way out.

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

| App | Brings |
|---|---|
| `claude-code` | `github:numtide/llm-agents.nix#claude-code`; `api.anthropic.com` ← `CLAUDE_CODE_OAUTH_TOKEN` |
| `paperclip` | `github:numtide/llm-agents.nix#paperclip`; `PAPERCLIP_DEPLOYMENT_MODE=authenticated`, `PAPERCLIP_DEPLOYMENT_EXPOSURE=private`, `PAPERCLIP_BIND=lan`; runs `paperclipai run`; port 3100 (web UI) |
| `nix`, `github`, `github-token`, `docker`, `python`, `node` | routes only: see [Configuration](#configuration) |

Both bring packages, so they need `nix` and `github` (or `github-token`).

Give Claude Code its token through the vault:

```sh
claude setup-token                               # on your machine
hangar credential set CLAUDE_CODE_OAUTH_TOKEN    # paste it (hidden)
```

Paperclip needs Claude Code, so list both. Listing `claude-code` means you
accept its unfree license. Claude Code may need hosts beyond
`api.anthropic.com` for some features (not verified); the vault's `403`
names any it hits, see [Allowing another host](#allowing-another-host).

**Paperclip needs onboarding once.** `paperclipai run` refuses to start
until it's set up, so the first `hangar up` skips it and warns
`paperclip needs setup: run 'hangar setup paperclip'`. Run that: it starts
`paperclipai onboard` in the VM with your terminal (pick authenticated
mode), then checks it worked. The next `hangar up` starts Paperclip;
`hangar logs paperclip -f` shows its output (with several bays, add
`--bay NAME` to `setup` and `logs`). Then open `http://127.0.0.1:3100`. Its
data lives in `~/.local/share/hangar/bays/default/home/.paperclip` on your
machine and survives `hangar destroy` (see [A bay's home](#a-bays-home) for the
database caveat). Authenticated mode and bind `lan` (not `loopback`) are
on purpose: the published port reaches the VM's network interface, and any
local process could reach the port.

`up` and `status` show one line per app with its hosts:

```
app claude-code: api.anthropic.com ← CLAUDE_CODE_OAUTH_TOKEN
app paperclip: no hosts
```

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
stays in a bay's own `files` and `mounts`. The built-ins are in
`config/apps/`.

## Nix (first-class)

hangar ships system modules for NixOS (`systemd` user service) and nix-darwin
(`launchd` user agent). No home-manager needed.

```nix
# flake.nix
inputs.hangar.url = "github:zahidkizmaz/hangar";

# NixOS: inputs.hangar.nixosModules.default
# nix-darwin: inputs.hangar.darwinModules.default
services.hangar = {
  enable = true;
  bays = [
    { name = "work"; apps = [ "nix" "github-token" "claude-code" "paperclip" ];
      packages = [ "github:numtide/llm-agents.nix#rtk" ]; cpus = 6; }
    { name = "oss"; apps = [ "nix" "github-token" "claude-code" "paperclip" ];
      ports.paperclip = 3200; }
  ];
  tower.credentialFiles.GITHUB_TOKEN = "/run/agenix/github-token"; # optional
};
```

`services.hangar` takes the [configuration](#configuration) keys as
options (`bays`, `tower.routes`, `tower.credentialFiles`,
`tower.masterPasswordFile`, `appDefinitions`, `stateDir`; two modules'
`bays` lists concatenate), plus:

- `enable`: install `hangar`;
- `autoStart`: run `hangar up` at login (default `false`);
- `msbPackage`: microsandbox (default: the packaged release, installed for
  you; `null` uses `msb` from `PATH`);
- `bays.*.imagePackage`: build the bay image locally (see
  [Local images](#local-images));
- `settings`: any other `hangar.json` key, e.g.
  `{ tower.agentVault.adminPort = 14421; }`; its lists replace the
  generated ones whole.

Each option's description is in `nix/module.nix`.

Secret options take **strings**, so the files are never copied into the Nix
store. On Linux the package builds in libsecret's `secret-tool` for the
keychain.

On Nix, microsandbox comes with the module: no Homebrew needed.

Don't set `inputs.hangar.inputs.nixpkgs.follows`: the modules build the
CLI with hangar's own nixpkgs, because it needs a newer Rust than stable
NixOS releases ship.

For a local checkout, use `git+file:///path/to/hangar` (a `path:` input
copies `.git`, which fails when git's fsmonitor socket exists) and run
`nix flake update hangar` after editing it.

## Commands

`hangar --help` lists the commands, flags and exit codes (0 ok, 1 error,
2 usage, 3 `status`: not healthy); every command has its own `--help`
with examples.

`-q`/`-v` work anywhere (`hangar -v up` or `hangar up -v`);
`HANGAR_LOG=debug` does the same and also works for the login autostart.
Output (status, ports, `credential list`) goes to stdout; progress and
warnings go to stderr, so `hangar status | grep` stays clean. Everything
after `hangar shell CMD` goes to the command unchanged, `--help` included:
`hangar shell gh --help`.

**Bays:** `up`, `down` and `destroy` take bay names (`hangar up work`);
without them they act on every bay, and `down`/`destroy` on the tower
too. `shell`, `logs`, `setup`, `copy` and `restart` act on one bay: the
only one, or `--bay NAME` (`-b`) when there are several (`hangar shell
--bay oss`). `up` takes the bays in config order, and a failing bay
doesn't stop the others: it's logged as `bay NAME: …`, `--json` lists it
under `failed`, and `up` exits 1. A bay removed from the config is
reported as left over, with the `hangar destroy NAME` that removes it;
its vault token goes on the next `up`.

**`up`, `copy` and `restart`:** `up` converges everything to your config
and is safe to repeat, but **never restarts** a VM or a `run` entry
(only a bay's Docker and nix daemons, when its token or the tower's CA
changed; see [Docker in a bay](#docker-in-a-bay)): when a running `run`
entry's command, environment or copied files changed, it says
`<name>'s inputs changed; run 'hangar restart <name>' to apply`. `hangar
copy` copies only the declared `files`; `hangar copy SRC [DEST]`
copies one file or directory ad hoc (same checks, not managed, so a declared
entry for the same path overwrites it on the next `copy`, `up` or
`restart`). `hangar restart [NAME…]` re-applies `files` (skip with
`--no-copy`) and the env file, then stops each entry (TERM, then KILL after
5 seconds) and starts it again.

**Scripts and agents:** add `--json` for one JSON document per command on
stdout (`hangar status --json | jq .tower`), and use the exit codes
(`hangar status --json || hangar up`). Schemas, flows and rules are in
[docs/cli.md](docs/cli.md).

With `autoStart = true` in the Nix module, `hangar up` runs at login, so the VMs come
back after a reboot. On NixOS it starts with the graphical session, where the
Secret Service is unlocked; a headless host sets `tower.masterPasswordFile` and
starts at boot instead, which for a systemd user unit also needs lingering
(`loginctl enable-linger <user>`). Disks persist: repos, docker images, the nix store and
Paperclip's data survive `down`/`up`.

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

Claude Code and Paperclip come as [apps](#apps), which bring their hosts
and settings too.

Unfree packages install too (`NIXPKGS_ALLOW_UNFREE` is set for that step
only): listing one, such as Claude Code, means you accept its license.

Their provider auth goes through agent-vault too: see [Apps](#apps) for
the pattern.

## A bay's home

A bay's home (`~` in the VM, `/home/pilot`) is kept on your machine in
`<stateDir>/bays/<name>/home`, i.e. `~/.local/share/hangar/bays/default/home`
(`home`), created with mode 0700 and mounted writable. Whatever the apps there keep in their home (`~/.claude`,
`~/.paperclip`, `~/.codex`, …) survives `hangar destroy`, and you can look
at it from your machine. It's **hangar's own home, separate from yours**:
hangar never mounts or copies your `~/.claude` or other config by default.
To give the VM your own config, copy it with [`files`](#your-config-files-in-the-vm)
or mount a folder with [`mounts`](#mounting-folders).

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
  folder. Use the vault (see [Apps](#apps)).
- Untested: databases on a host share (Paperclip's embedded Postgres under
  `~/.paperclip`) may not like its file locking. If one misbehaves, point
  the app's data at the VM disk (e.g.
  `env.PAPERCLIP_HOME = "/var/lib/pilot/paperclip"`, pilot's folder on
  the VM disk) or set
  `home = false`.

## Docker in a bay

Each bay runs Docker rootless as `pilot` (`DOCKER_HOST` points at it), so
full test suites with containers work without giving apps root. The usual
rootless limits apply: no cgroup resource limits on containers, slower
container networking, and `--privileged` only gets pilot's
user-namespace capabilities. Pulls use the bay's proxy by themselves.

Docker's data lives on a disk of its own in the VM, `disk` sized like the
root (both grow only as they're used), and goes with the VM. When a
bay's token is renewed or the tower's CA changes, `up` restarts the
bay's Docker daemon so it uses the new ones, which stops its running
containers.

Containers don't inherit the proxy or its CA: pass them in. Containers
can't resolve the proxy's host name, so add it:

```sh
ip=$(awk '$2 == "host.microsandbox.internal" {print $1; exit}' /etc/hosts)
docker run --rm --add-host "host.microsandbox.internal:$ip" \
  -e HTTPS_PROXY -v "$SSL_CERT_FILE:/etc/ssl/certs/ca-certificates.crt:ro" \
  curlimages/curl https://example.com
```

Never write the proxy URL (it carries the bay's token) into
`~/.docker/config.json`: the home is kept on your machine.

## Package cache

Nix packages from `packages` are cached on your machine in
`$XDG_CACHE_HOME/hangar/bays/<name>`, i.e. `~/.cache/hangar/bays/default`
(`cache`, created 0700, mounted writable). Each bay has its own
cache, never shared: a bay can only reach its own. Packages
in a bay live on its own disk, so a new VM (`hangar destroy NAME &&
hangar up NAME`) would download all of them again; with the cache it gets them from
your machine first and only goes to the internet for what's missing. `up`
fills the cache after it installs something; `hangar status` shows its
size.

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
the vault instead (see [Apps](#apps)). Paperclip is configured by its app's
variables and its UI, so it needs `files` only for a config file of
its own.

Changes reach the VM on the next `up` or `hangar copy`: hangar rewrites
files whose source changed and removes files it copied whose entry is gone.
A running app keeps the old config until `hangar restart <name>`; `up` tells
you when that's needed. `hangar copy SRC [DEST]` pushes one file ad hoc
through the same checks. Files you created
in the VM yourself are left alone. Everything the agents can read here is
visible to them, so hangar refuses the whole step, before copying anything,
when a file:

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
  directory (`~/.claude`, `~/.config`, `~/.local`, …) or `~/Library` — where
  host programs keep their config. Use an ordinary directory made for it,
  such as `~/work/shared`. The only exceptions are the bay's own folders:
  `<stateDir>/bays/<name>/home` (nothing else in `stateDir`) and
  `$XDG_CACHE_HOME/hangar/bays/<name>` (not `~/.cache` itself, another
  bay's cache or another app's).

Sources are checked where they really point (symlinks resolved). A missing
source is created by `up` (mode 0700), but only after the path it would have
passes the same rules; a refused path is never created. A read-only mount
exposes the whole directory to the
agents, so keep it narrow. A VM path is either copied or mounted, never both,
and `mounts` don't nest in each other; they may sit inside the
bay's home (microsandbox applies the enclosing mount first), but not
cover it. Mounts are fixed when the VM is created: after
changing them, `hangar destroy NAME && hangar up NAME` (`up` and `status`
say so).

## Publishing the bay image

The image is a NixOS system (`nix/bay/configuration.nix`, packed by
`nix/image.nix`) that each bay boots into, and holds generic tools only:
docker, nix, direnv + nix-direnv, git, gh, uv and common CLI tools (ripgrep,
fd, jq, yq, ast-grep, just, make, …). It redistributes no agent CLIs; those
come from `packages`. `.github/workflows/image.yml`
builds it on native amd64 and arm64 runners on every push to `main` or a `v*`
tag, and pushes a multi-arch image to `ghcr.io/zahidkizmaz/hangar-bay`.

By default hangar uses the image tagged with its own version
(`ghcr.io/zahidkizmaz/hangar-bay:v<version>`), so the CLI and the image's
`hangar-start` always match. A fork sets a bay's `imageRepository` to its
own registry, or `image` to any ref (a digest pins it). The VM's disk is
made from the image once: after switching images, `hangar up` warns until you
run `hangar destroy NAME && hangar up NAME`. Any image must be built from
`nix/bay/configuration.nix`: hangar boots its systemd and runs its
`hangar-start`, and apps run as its user `pilot` (uid 1000, home
`/home/pilot`).

### Local images

- Nix: a bay's `imagePackage = inputs.hangar.packages.${pkgs.system}.bay-image;`
  builds the image and loads it into microsandbox once per build. On macOS
  this needs a Linux builder, e.g. nix-darwin's `nix.linux-builder.enable = true;`.
- Without the module, on Linux: load the image under a local tag and
  point hangar at it. A cached tag is used without contacting a registry.

  ```sh
  nix build .#bay-image && ./result | msb load --tag hangar-bay:dev
  ```

  Then set the bay's `image = "hangar-bay:dev"` and run
  `hangar destroy NAME && hangar up NAME`.

## Troubleshooting

- An image pull hangs on macOS: `msb` is waiting on
  `docker-credential-osxkeychain` from `~/.docker/config.json`. Run hangar
  with `DOCKER_CONFIG` set to a folder whose `config.json` is `{}`.

## Development

See [docs/development.md](docs/development.md). Coding agents start with
[AGENTS.md](AGENTS.md).

Releases: pushing a `v*` tag attaches binaries for aarch64-darwin,
x86_64-linux and aarch64-linux, plus `SHA256SUMS`, to the GitHub release.
