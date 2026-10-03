# Using hangar

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

On NixOS or nix-darwin, use the module: it installs hangar and
microsandbox (see [nix.md](nix.md)).

Otherwise you need [microsandbox](https://microsandbox.dev) (`msb`). On
Linux you also need access to `/dev/kvm`. hangar itself is a single binary
with no runtime dependencies.

```sh
brew install superradcompany/tap/microsandbox # or see microsandbox.dev

# from source …
cargo install --git https://github.com/zahidkizmaz/hangar
# … or, once a version is tagged, a release binary
# (aarch64-darwin, x86_64-linux, aarch64-linux)
curl -fsSLo ~/.local/bin/hangar \
  https://github.com/zahidkizmaz/hangar/releases/latest/download/hangar-aarch64-darwin
chmod +x ~/.local/bin/hangar

hangar init # writes a minimal ~/.config/hangar/hangar.json
```

Edit `~/.config/hangar/hangar.json` (`$XDG_CONFIG_HOME/hangar/hangar.json`
when that's set). It only holds what you set; everything else comes from
the built-in defaults, so updates to them reach you:

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

- `bays`: the bays, in order; each lists its
  [apps](configuration.md#apps). Without any there is one bay, `default`.
- `tower.credentialFiles`: vault credential key → file holding its value,
  e.g. `{"GITHUB_TOKEN": "/path/to/github-token"}`, for credentials you
  keep in files. Others you store with `hangar credential set NAME`.

Every other setting is in [configuration.md](configuration.md). Then run
`hangar up`.

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

## First run

```sh
hangar up       # creates the VMs; prints the ports below
hangar status   # the same overview, any time
```

### Accounts and the vault UI

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

### Ports and URLs

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
`hangar credential list` shows the names and who manages them, never the
values; `hangar credential rm NAME` deletes one of yours.

## Claude Code and Paperclip

Both are built-in [apps](configuration.md#built-in-apps) and bring
packages, so they need `nix` and `github` (or `github-token`):

```nix
services.hangar.bays = [
  { name = "default"; apps = [ "nix" "github" "claude-code" "paperclip" ]; }
];
```

Give Claude Code its token through the vault:

```sh
claude setup-token                               # on your machine
hangar credential set CLAUDE_CODE_OAUTH_TOKEN    # paste it (hidden)
```

Paperclip needs Claude Code, so list both. Listing `claude-code` means you
accept its unfree license. Claude Code may need hosts beyond
`api.anthropic.com` for some features (not verified); the vault's `403`
names any it hits, see
[Allowing another host](configuration.md#allowing-another-host).

**Paperclip needs onboarding once.** `paperclipai run` refuses to start
until it's set up, so the first `hangar up` skips it and warns
`paperclip needs setup: run 'hangar setup paperclip'`. Run that: it starts
`paperclipai onboard` in the VM with your terminal (pick authenticated
mode), then checks it worked. The next `hangar up` starts Paperclip;
`hangar logs paperclip -f` shows its output (with several bays, add
`--bay NAME` to `setup` and `logs`). Then open `http://127.0.0.1:3100`. Its
data lives in `~/.local/share/hangar/bays/default/home/.paperclip` on your
machine and survives `hangar destroy` (see
[A bay's home](configuration.md#a-bays-home) for the database caveat).
Authenticated mode and bind `lan` (not `loopback`) are on purpose: the
published port reaches the VM's network interface, and any local process
could reach the port.

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
[cli.md](cli.md).

Disks persist: repos, docker images, the nix store and Paperclip's data
survive `down`/`up`. To bring the VMs back after a reboot, see
[Autostart](nix.md#autostart).

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

## Troubleshooting

- An image pull hangs on macOS: `msb` is waiting on
  `docker-credential-osxkeychain` from `~/.docker/config.json`. Run hangar
  with `DOCKER_CONFIG` set to a folder whose `config.json` is `{}`.
- A host is refused with `403`: add a route for it, see
  [Allowing another host](configuration.md#allowing-another-host).
- `up` refuses an existing VM, or says its ports, mounts or image changed:
  these are fixed when the VM is created, so run
  `hangar destroy NAME && hangar up NAME` (the home is kept).
- `NAME needs setup: run 'hangar setup NAME'`: the app's one-time setup
  hasn't run yet; run that command (see
  [Claude Code and Paperclip](#claude-code-and-paperclip)).
- `dockerd in hangar-bay-NAME did not start`: in `hangar shell -b NAME`,
  run `systemctl --user reset-failed docker && systemctl --user restart
  docker` (systemd stops restarting docker after three starts in a
  minute); `systemctl --user status docker` says why it stopped.
