# hangar CLI for agents and scripts

Everything here works without a terminal. Add `--json` to any command
(before or after it) for machine-readable output.

## Streams and exit codes

- **stdout**: the command's result, nothing else. With `--json`, at most
  one JSON document per command.
- **stderr**: progress and warnings (`==> …`, `warning: …`; more with
  `-v`/`-vv` or `HANGAR_LOG=debug`). With `--json`, an error is the last
  stderr line, as JSON.
- **`$XDG_STATE_HOME/hangar/hangar.log`** (0600): every info line and every
  change, timestamped, whatever the flags.

| Code | Meaning |
| --- | --- |
| 0 | success |
| 1 | error; for `up`, `down`, `restart` and `rm`, any change that failed |
| 2 | usage error (clap's text, not JSON) |
| 3 | `ps` ran and found something wrong (its warnings say what) |

`exec` replaces hangar with the shell, so its exit code is the command's.

`up`, `down`, `restart`, `rm`, `cp`, `secret` and `setup` hold a lock
(`$XDG_STATE_HOME/hangar/lock`); a second one waits until the first is
done. `ps`, `inspect`, `logs`, `exec` and `init` never wait.

## Documents

Every document has `"version": 1`. It stays 1 until the first release;
after that it goes up when a field changes meaning or disappears. Key
order isn't significant.

| Command | `--json` |
| --- | --- |
| `up`, `down`, `restart`, `rm`, `secret set` | the changes, below |
| `ps` | the overview, below |
| `inspect NAME` | the detail, below |
| `cp` | `{"version":1,"copied":[…],"removed":[…]}` |
| `init` | `{"version":1,"ok":true}` |
| `exec`, `logs`, `setup` | the command's own output; `--json` changes only errors |

### Changes

```json
{ "version": 1, "changes": [
  { "name": "vault", "type": "proxy", "outcome": "created",
    "reasons": [], "error": null },
  { "name": "work", "type": "bay", "outcome": "recreated",
    "reasons": ["image", "mounts"], "error": null },
  { "name": "docs", "type": "bay", "outcome": "failed", "reasons": [],
    "error": { "error": "proxy corp failed", "hint": null } } ] }
```

`outcome` is `created`, `recreated`, `updated`, `unchanged`, `stopped`,
`removed` or `failed`. `reasons` says why a VM was recreated (`image`,
`proxy`, `mounts`, `ports`, `resources`, `fingerprint`, `no record`,
`data disk replaced`) or what an update changed (`started`, `proxy CA`,
`proxy login`, `packages …`, `copies …`, `env`, `started APP`,
`restarted APP`, `stopped APP`).

### `ps`

```json
{ "version": 1,
  "bays": [
    { "name": "work", "state": "running", "proxy": "vault",
      "image": "ghcr.io/zahidkizmaz/hangar-bay:v0.1.0",
      "apps": [ { "name": "paperclip", "state": "running" } ],
      "ports": [ { "name": "paperclip", "app": "paperclip", "host": 3100,
        "vm": 3100, "address": "http://127.0.0.1:3100",
        "purpose": "Paperclip web UI" } ] } ],
  "proxies": [
    { "name": "vault", "kind": "infisical", "state": "running",
      "image": "infisical/cli:0.45.3@sha256:…", "health": "up",
      "port": 17323 },
    { "name": "corp", "kind": "http", "state": null, "image": null,
      "health": "unreachable", "port": 3128 } ],
  "leftovers": [ { "name": "old", "type": "bay" } ] }
```

- `state` is `running`, `stopped`, `missing`, or `unknown` when the sandbox
  can't tell. An external proxy's `state` and `image` are `null`.
- A bay's `proxy` is `null` for the open internet. Its `apps` lists the
  apps with a run entry while it runs: `running`, `exited` or
  `needs-setup`.
- `health` is `up`, `unreachable`, or `null` when a managed proxy's VM
  isn't running. `port` is the host port its bays reach it on.
- A bay's `ports` are published on `127.0.0.1`; `address` is a URL for an
  HTTP port, else `127.0.0.1:<port>`.
- `leftovers`: bays and proxies with a folder in the data dir but no config
  entry; `hangar rm NAME --data` removes one.

### `inspect`

A bay:

```json
{ "version": 1, "name": "work", "type": "bay", "state": "running",
  "proxy": "vault", "proxy_port": 17323,
  "image": "ghcr.io/zahidkizmaz/hangar-bay:v0.1.0",
  "vm": "hangar-bay-work", "cpus": 4, "memory": "6G", "disk": "40G",
  "home": "/home/you/.local/share/hangar/bays/work/home",
  "cache": "/home/you/.cache/hangar/bays/work",
  "volume": "hangar-bay-work-data",
  "apps": ["claude-code"], "app_states": [], "ports": [],
  "packages": ["github:numtide/llm-agents.nix#claude-code"],
  "env": ["CLAUDE_CODE_OAUTH_TOKEN"],
  "mounts": [
    { "vm": "/home/pilot", "host": "/home/you/.local/share/hangar/bays/work/home",
      "mode": "rw", "kind": "home", "applied": true } ] }
```

`proxy_port` is the bay's only way out, `null` for the open internet.
`env` holds names only. `mounts` lists every mount, hangar's own
(`kind`: `user`, `home` or `cache`) and the `copy` ones (`mode`: `ro`,
`rw` or `copy`). `applied` says whether the VM was created with it;
`null` when there's no record, and always for a `copy`.

A proxy:

```json
{ "version": 1, "name": "vault", "type": "proxy", "kind": "infisical",
  "state": "running", "health": "up", "health_error": null, "port": 17323,
  "url": null, "image": "infisical/cli:0.45.3@sha256:…",
  "vm": "hangar-proxy-vault",
  "data": "/home/you/.local/share/hangar/proxies/vault/data",
  "log": "/home/you/.local/share/hangar/proxies/vault/data/data/proxy.log",
  "bays": ["work"],
  "tokens": [ { "name": "proxy-token", "source": "keychain" },
              { "name": "session-token", "source": "keychain" } ] }
```

An external proxy has `url` set and `state`, `image`, `vm` and `log`
`null`. A token's `source` is `keychain`, `command`, `missing`, or
`unknown` when the keychain couldn't be asked; the token itself is
never shown.

### Errors

```json
{ "error": "bay work isn't running", "hint": "run 'hangar up work' first" }
```

`hint` is the next step when there's an obvious one, otherwise `null`.

## Flows

- **Bring everything up**: `hangar up --json`; exit 1 means a change
  failed, and its `error` says why.
- **Check**: `hangar ps --json`; exit 3 means something needs doing, and
  the warnings on stderr name the command.
- **Change the config**: edit it, then `hangar up`. Read `reasons` to see
  whether a VM was recreated.
- **Renew an Infisical session token**: pipe it into `hangar secret set
  NAME session-token`, then `hangar up`
  ([proxy-kinds.md](proxy-kinds.md#infisical-setup)).
- **Get config files into a bay**: a `copy` mount, then `hangar up`. Once:
  `hangar cp FILE BAY:~/path`. A refusal names the file and the rule; fix
  the config, never work around it.
- **Start over with a bay**: `hangar rm NAME --data --yes`, then
  `hangar up NAME`.

## Rules

- Never pass a secret as an argument, in `env` or through a `copy` mount.
  Secrets belong in the proxy; its own tokens stay in the host's
  keychain.
- Never mount a directory a host program reads config from as writable;
  hangar refuses the known ones, but a dedicated directory is the rule.
- `rm --data` needs `--yes` without a terminal.
- `setup` is interactive: it runs the app's setup command with a
  terminal.
