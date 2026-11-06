# Using hangar

hangar runs VMs on your machine, with
[microsandbox](https://microsandbox.dev) by default:

```
 your machine (macOS / Linux)
 ├─ hangar-proxy-vault   a managed proxy (here: Infisical Agent Vault),
 │                       published on 127.0.0.1:17323
 ├─ hangar-bay-work      a bay: your agents, docker, nix, git, gh, uv.
 │                       Its only allowed connection is its proxy's port.
 └─ hangar-bay-scratch   "proxy": null: the open internet, no proxy
```

A bay with a proxy holds placeholders, never secrets: the proxy swaps in
the real credential on the way out, for the hosts you allow there.
Everything else, direct connections, DNS and your LAN included, is
blocked by the sandbox.

## Security model

- **Tokens are the blast radius.** Agents can't read the credentials, but
  they can use them. Give the proxy narrow tokens (a fine-grained GitHub
  token for the repos agents work on, no admin scopes) and protect your
  default branch.
- **One egress.** A bay with a proxy reaches only that proxy's host port.
  The rule is the sandbox's, outside the VM, so root or Docker in the bay
  can't change it.
- **A direct bay is a choice.** `"proxy": null` gives a bay the open
  internet. A bay without a proxy and without `defaults.proxy` is an
  error.
- **Apps never run as root.** Apps, shells and copied files run as the
  user `pilot` (uid 1000, home `/home/pilot`), with no sudo. Docker is
  pilot's own rootless daemon. The VM stays the real boundary.
- **Keep app UIs behind a login.** App ports are published on
  `127.0.0.1`, so any local process can reach them.
- **Bays sharing a proxy share its credentials.** Use separate proxies or
  sessions where that matters. A host folder mounted into two bays is a
  channel between them; their homes and caches never are.
- **A bay is disposable.** Remove and recreate it now and then with
  `hangar rm NAME && hangar up NAME`. Code leaves through reviewed PRs.

## Install

On NixOS or nix-darwin, use the module: it installs hangar and
microsandbox (see [nix.md](nix.md)).

Otherwise install [microsandbox](https://microsandbox.dev) (`msb`). On
Linux you also need access to `/dev/kvm` and, for proxy tokens,
`/usr/bin/secret-tool` with a running Secret Service (a desktop session
has one). Arch's `libsecret` ships it; on Debian and Ubuntu install
`libsecret-tools`. hangar is a single binary.

```sh
brew install superradcompany/tap/microsandbox # or see microsandbox.dev

# from source …
cargo install --git https://github.com/zahidkizmaz/hangar
# … or, once a version is tagged, a release binary
# (aarch64-darwin, x86_64-linux, aarch64-linux)
curl -fsSLo ~/.local/bin/hangar \
  https://github.com/zahidkizmaz/hangar/releases/latest/download/hangar-aarch64-darwin
chmod +x ~/.local/bin/hangar

hangar init # writes ~/.config/hangar/hangar.json
```

`hangar init` writes one direct bay, `default`. Add a proxy and point the
bay at it; the README has a full example with a managed Infisical proxy,
and [proxy-kinds.md](proxy-kinds.md) has every kind. Then:

```sh
hangar up    # proxies first, then the bays
hangar ps    # state, proxy and image of each bay and proxy
```

`up` prints one line per bay and proxy:

```
vault: created
work: recreated (image, mounts)
docs: updated (packages +rtk; started paperclip)
scratch: unchanged
```

## Commands

`hangar --help` lists the commands and exit codes; each command has its
own `--help`. Names are positional, and bays and proxies share one
namespace.

| Command | Does |
| --- | --- |
| `init [--force]` | writes a starter config |
| `up [NAME…]` | creates, starts, updates or recreates bays and proxies (default: all) |
| `down [NAME…]` | stops them (bays first) |
| `restart [NAME…]` | `down`, then `up` |
| `rm NAME [--data] [-y]` | removes a VM; `--data` also deletes its data |
| `ps` | lists bays, proxies and leftovers, and warns about anything wrong |
| `inspect NAME` | everything about one bay or proxy |
| `exec BAY [CMD…]` | a login shell in the bay, or `CMD` |
| `logs [NAME [APP]] [-f]` | hangar's log, a bay's app journal, or a managed proxy's log |
| `cp SRC DEST` | copies into or out of a running bay (`BAY:PATH` on one side) |
| `setup BAY APP [--force]` | runs an app's one-time interactive setup |
| `secret set PROXY [TOKEN]` | asks for a proxy's tokens and keeps them in the OS keychain |

- **`up`** brings each selected bay's proxy up first. A bay whose proxy
  failed is left alone, and any failure makes `up` exit 1 while the rest
  goes on. What `up` applies in place and what recreates a VM is in
  [configuration.md](configuration.md#what-up-changes).
- **`rm`** takes a configured name or a leftover (a folder in the data dir
  with no config entry). `--data` deletes a bay's home and data volume, or
  a proxy's data folder, after asking; `-y` skips the question, and
  without a terminal it's required. The package cache is always kept.
- **`ps`** warns about apps that aren't running or need setup,
  unreachable proxies and leftovers, and then exits 3.
- **`exec`**, **`cp`** and **`setup`** need the bay running. Everything
  after `exec BAY` goes to the command unchanged, `--help` included.
- **`cp`** into a bay runs the same checks as a `copy` mount, but the copy
  isn't managed. Out of a bay, the destination must not exist yet.
- **`logs`** without a name shows hangar's own log; with a bay, the
  journal of its apps (the last 200 lines); with a managed proxy, its log.
  `-f` keeps following.

`up`, `down`, `restart`, `rm`, `cp`, `secret set` and `setup` hold a lock
(`$XDG_STATE_HOME/hangar/lock`); a second one logs `waiting for another
hangar` and goes on once the first is done.

`-q` and `-v`/`-vv` work anywhere; `HANGAR_LOG=debug` does the same when
flags can't. Results go to stdout, progress and warnings to stderr.
Everything at info level and above, and every change, also goes to
`$XDG_STATE_HOME/hangar/hangar.log` (mode 0600). For scripts, add
`--json` and see [cli.md](cli.md).

## Claude Code, Codex and Paperclip

All three are built-in [apps](configuration.md#apps). Each sets its
credential variable to a placeholder; the real credential belongs in the
proxy, for that provider's host.

- **Claude Code**: the bay sends `CLAUDE_CODE_OAUTH_TOKEN`'s placeholder.
  Store the token from `claude setup-token` in your proxy.
- **Codex**: store your OpenAI API key in the proxy, then run `hangar
  setup BAY codex` once per home. It logs Codex in with the placeholder;
  the proxy swaps in the real key. Sign in with ChatGPT doesn't work
  through a proxy: Codex would keep that login's refresh token in the bay.
- **Paperclip** needs onboarding once. Until then `up` doesn't start it
  and `ps` says `app paperclip needs setup`. Run `hangar setup BAY
  paperclip` (pick authenticated mode), then `hangar up` starts it. Open
  `http://127.0.0.1:3100`; `hangar logs BAY paperclip -f` follows its
  output. Its data lives in the bay's home.

Listing `claude-code` means you accept its unfree license.

## Docker in a bay

Each bay runs Docker rootless as `pilot` (`DOCKER_HOST` points at it), so
test suites with containers work without root. The usual rootless limits
apply: no cgroup limits on containers, slower container networking, and
`--privileged` gets only pilot's user-namespace capabilities. Pulls go
through the bay's proxy.

Docker's data is on the bay's data volume (`/var/lib/pilot`), so images
and containers survive a recreate unless `disk` changes. When the proxy
login changes, the bay restarts its Docker daemon, which stops running
containers.

Containers don't inherit the proxy or its CA: pass them in, and add the
proxy's host name, which containers can't resolve:

```sh
ip=$(awk '$2 == "host.microsandbox.internal" {print $1; exit}' /etc/hosts)
docker run --rm --add-host "host.microsandbox.internal:$ip" \
  -e HTTPS_PROXY -v "$SSL_CERT_FILE:/etc/ssl/certs/ca-certificates.crt:ro" \
  curlimages/curl https://example.com
```

Never write the proxy URL (it carries the bay's login) into
`~/.docker/config.json`: the home is kept on your machine.

## Troubleshooting

- `ps` exits 3: read its warnings; each names the command that fixes it.
- `proxy NAME unreachable` or `didn't come up`: `hangar logs NAME` shows a
  managed proxy's log. For `infisical`, check both tokens
  (`hangar secret set NAME`).
- `no TOKEN for proxy NAME`: without a terminal, `up` doesn't ask; run
  `hangar secret set NAME`, or pipe the token into it.
- An image pull hangs on macOS: `msb` is waiting on
  `docker-credential-osxkeychain` from `~/.docker/config.json`. Run hangar
  with `DOCKER_CONFIG` set to a folder whose `config.json` is `{}`.
- `dockerd in hangar-bay-NAME did not start`: in `hangar exec NAME`, run
  `systemctl --user reset-failed docker && systemctl --user restart
  docker`; `systemctl --user status docker` says why it stopped.
- A mount or copy is refused: the error names the path and the rule; see
  [Mounting folders](configuration.md#mounting-folders).
