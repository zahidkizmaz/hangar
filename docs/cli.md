# hangar CLI for agents and scripts

Everything here works without a terminal. Add `--json` to any command (before
or after it) for machine-readable output.

## Streams and exit codes

- **stdout**: the command's result, nothing else. With `--json`, exactly one
  JSON document per command.
- **stderr**: progress and diagnostics (`==> …`, `warning: …`; more with
  `-v`/`-vv` or `HANGAR_LOG=debug`). With `--json`, an error is the last
  stderr line, as JSON.

| Code | Meaning |
|---|---|
| 0 | success (for `status`: healthy) |
| 1 | error; for `up`, also a bay that failed (see `failed`) |
| 2 | usage error (unknown command or flag; clap's text, not JSON) |
| 3 | `status` ran, but something isn't healthy |

Healthy means: the tower and every configured bay running, the vault
healthy and denying unlisted hosts, and every bay's `run` entries running.
Bays left out of the config don't count. `up` never exits 3: health is
`status`'s.

`down` and `destroy` exit 1 when msb can't report a VM's state: a broken
msb is never mistaken for a stopped or missing VM.

One hangar changes state at a time. `up`, `down`, `restart`, `copy`,
`destroy` and `credential set|rm` hold a lock
(`$XDG_STATE_HOME/hangar/lock`); a second one logs
`==> waiting for another hangar` and continues once the first is done.
`status`, `shell`, `logs`, `setup`, `vault-ui`, `credential list` and
`init` never wait.

## Commands

Bays: `up`, `down` and `destroy` take `[BAY…]` (none: every bay, and for
`down`/`destroy` the tower and left-over bays too). `shell`, `logs`,
`setup`, `copy` and `restart` act on one bay: the only one, or `--bay
NAME` (`-b`); with several bays and no `--bay` they fail with `several
bays: pass --bay (…)`. A name that's neither configured nor left over is
an error (exit 1).

| Command | Result (`--json`) |
|---|---|
| `up [BAY…]` | the status document below plus `failed` (as text: the app lines and the port summary) |
| `status` | the status document; exit 3 when unhealthy |
| `down`, `init`, `destroy`, `credential set`, `credential rm` | `{"ok":true,"version":1}` |
| `credential list` | names and who manages them, never values |
| `vault-ui` | where the vault login is, never the password |
| `copy [SRC [DEST]]` | `{"ok":true,"bay":…,"copied":[…],"removed":[…],"version":1}` (VM paths only) |
| `restart [NAME…] [--no-copy]` | `{"ok":true,"bay":…,"restarted":[…],"version":1}` |
| `shell [CMD…]`, `logs NAME [-f]`, `setup NAME [--force]` | the command's own output, streamed; `--json` only changes errors |

Every document is an object with a `version`, 1. It stays 1 until the
first release; after that it goes up when a field changes meaning or
disappears. Key order isn't significant.

### `status --json` (and `up --json`)

```json
{
  "version": 1,
  "healthy": true,
  "tower": { "vm": "running", "backend": "agent-vault", "reachable": true,
             "healthy": true, "unlistedHosts": "deny",
             "ports": [
    { "name": "vault-ui", "app": null, "url": "http://127.0.0.1:14321",
      "purpose": "agent-vault admin UI and API", "reachable": true },
    { "name": "proxy", "app": null, "url": "127.0.0.1:14322",
      "purpose": "the bays' only way out", "reachable": null } ] },
  "bays": [
    { "name": "work", "vm": "hangar-bay-work", "state": "running",
      "healthy": true,
      "apps": {
        "claude-code": { "routes": [
          { "name": "anthropic", "host": "api.anthropic.com",
            "credential": "CLAUDE_CODE_OAUTH_TOKEN" } ] },
        "paperclip": { "routes": [] } },
      "run": { "paperclip": "running" },
      "mounts": [
        { "vm": "/home/pilot", "host": "/home/you/.local/share/hangar/bays/work/home",
          "writable": true, "home": true, "cache": false, "applied": true },
        { "vm": "/var/cache/hangar", "host": "/home/you/.cache/hangar/bays/work",
          "writable": true, "home": false, "cache": true, "applied": true } ],
      "cache": { "host": "/home/you/.cache/hangar/bays/work", "bytes": 734003200 },
      "ports": [
        { "name": "paperclip", "app": "paperclip", "url": "http://127.0.0.1:3100",
          "purpose": "Paperclip web UI", "reachable": true } ] }
  ],
  "leftovers": [ { "name": "old", "vm": "hangar-bay-old", "state": "running" } ],
  "failed": [ { "name": "oss", "error": "…" } ]
}
```

- `tower`: the VM running the credential broker; `vm` is its state
  (`running`, `stopped`, `missing` or `unknown`: the sandbox failed),
  `backend` is `tower.backend` (`agent-vault` is the only one so far),
  `unlistedHosts` is `deny`, `allow`, or `null` while unreachable, and
  `ports` its ports (`app: null`).
- `bays`: each configured bay, in config order. `state` as for the
  tower; `healthy` is the bay running with every `run` entry running.
- `bays[].apps.<name>.routes`: each enabled app's routes and the credential
  the vault injects there (`null`: passthrough; several are comma-joined).
- `bays[].run.<name>`: `running`, `stopped` or `unknown`, for each `run`
  entry, including the apps' own.
- `bays[].mounts`: the `home` mount (`home: true`), the `cache` mount
  (`cache: true`) and each `mounts` entry; `applied` is false when the bay
  is missing or was created without it, and `null` when an existing VM has
  no `bays/<name>/vm` record (the text output says `unknown`); fix with
  `hangar destroy NAME && hangar up NAME`. Mounts don't affect `healthy`.
- `bays[].cache`: the bay's package cache folder and size in bytes, or
  `null` when `cache` is off.
- `bays[].ports`: each app's ports. `url` has `http://` for HTTP ports.
  `reachable` comes from an HTTP GET for HTTP ports and a TCP connect
  otherwise; it's `null` for the proxy, which only the bays use. Ports
  don't affect `healthy`.
- `leftovers`: bays with a folder in `stateDir` but no config entry; fix
  with `hangar destroy NAME` (then `--state` once the VM is gone). They
  don't affect `healthy`.
- `failed` (`up` only): each bay whose steps failed, with the error; `up`
  then exits 1. The other bays still came up.

### `credential list --json`

```json
{ "version": 1, "credentials": [
  { "name": "GITHUB_TOKEN", "source": "config" },
  { "name": "CLAUDE_CODE_OAUTH_TOKEN", "source": "user" } ] }
```

`config` entries come from `tower.credentialFiles` and the apps' fixed
`credentials` (e.g. `GITHUB_GIT_USER`); `user` ones from `hangar
credential set`.

### `vault-ui --json`

```json
{ "version": 1, "url": "http://127.0.0.1:14321",
  "login": "owner@hangar.local", "password": "clipboard",
  "passwordFile": "/Users/you/.local/share/hangar/owner-password" }
```

`password` is `clipboard` or `file`; the value itself is never printed.

### Errors (`--json`)

```json
{ "error": "the vault isn't running", "hint": "run 'hangar up' first" }
```

`hint` is the next step when there's an obvious one, otherwise `null`.

## Flows

- **First run**: `hangar up --json` (generates the master password into the
  OS keychain; no input needed). Check with `hangar status --json`.
- **Ensure it's up**: `hangar status --json >/dev/null || hangar up`.
- **Add a token**: `hangar credential set NAME < file` (or pipe it in). Name
  it after the variable the tool reads, e.g. `CLAUDE_CODE_OAUTH_TOKEN`; the
  bays get a placeholder automatically. Then `hangar up` if a route needs
  to reference it.
- **A 403 from the vault**: the message names the host. Add a route for it
  under `tower.routes` (or the app that brings it to a bay's `apps`),
  then `hangar up`.
- **Get config files into the VM** (e.g. `~/.claude/CLAUDE.md`): add them
  under `files` (VM path → host path), then `hangar copy` (or `up`).
  One-off: `hangar copy SRC [DEST]`, same checks, not managed. A refusal
  names the file and why (credentials file, secret-looking content,
  symlink, limits); fix the config, never work around it.
- **Set up a tool**: add its app (a built-in from
  [configuration.md](configuration.md#built-in-apps), or one of
  `appDefinitions`) to a bay's `apps`, then
  `hangar up`. A bay with packages needs `nix` and `github` (or
  `github-token`). A config error names the app and key
  (`app paperclip: env.PATH: reserved for hangar`).
- **App data that should survive a new VM**, and **a new VM without
  downloading every package again**: nothing to do; the bay's `home` and
  `cache` are kept on the host
  ([A bay's home](configuration.md#a-bays-home),
  [Package cache](configuration.md#package-cache)).
- **Share another host folder with the VM**: add a dedicated host
  directory under `mounts` (`"writable": true` if the VM should write),
  then `hangar destroy NAME && hangar up NAME`.
  [Mounting folders](configuration.md#mounting-folders) has the rules a
  mount must pass.
- **Apply changed config to a running app**: `up` never restarts; when it
  warns `<name>'s inputs changed`, run `hangar restart <name>`
  (`--no-copy` to keep the VM's current files).
- **Switch a bay's image or VM sizes/ports**: `hangar destroy NAME &&
  hangar up NAME`.
- **Add a bay**: add it to `bays`, then `hangar up NAME`. **Remove one**:
  drop it from `bays`, then `hangar destroy NAME` (and `--state` for its
  home and records).
- **Start over**: `hangar destroy --state --yes` (also deletes the keychain
  item and every bay's home), then `hangar up`.

## Rules

- Never pass a secret as an argument, in `env` or through
  `files`: use `hangar credential set NAME` with the value on stdin.
- Never mount a directory a host program reads config from as writable;
  hangar refuses the known ones, but a dedicated directory is the rule.
- Never copy or mount the user's own config such as `~/.claude` by
  default: the bay's home is hangar's own.
- `destroy --state` needs `--yes` without a terminal.
- `copy`, `restart` and `setup` need the bay running (`hangar up`
  first), and `--bay NAME` when there are several bays.
- `setup NAME` is interactive: it runs the app's setup command with a
  terminal. While an app's setup check fails, `up` warns
  `NAME needs setup: run 'hangar setup NAME'` and doesn't start its run
  entry, so `status` shows it `stopped` (unhealthy, exit 3).
- `shell` passes everything after the command through unchanged, including
  `--help`.
