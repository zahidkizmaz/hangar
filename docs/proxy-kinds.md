# Proxy kinds

A proxy is a bay's one way out. Its `kind` says how hangar runs or
reaches it, how a bay logs in to it, and where the bay gets the CA it
trusts. Every entry under `proxies` has these keys:

| Key | Meaning |
| --- | --- |
| `kind` | `infisical`, `agent-vault` or `http` |
| `run` | `"managed"`: hangar runs it in its own VM, `hangar-proxy-<name>` |
| `url` | it already runs: `http://<host>:<port>`, see [Remote proxies](#remote-proxies) |
| `secrets` | optional: a command per token, see [Proxy tokens](#proxy-tokens) |

Set `run` or `url`, not both. The other keys belong to the kind; an
unknown one is an error.

## Proxy tokens

A kind that needs tokens (to enrol, or for the bays to log in) reads
them from the OS keychain: the macOS Keychain, or the Secret Service on
Linux. Each is an item of service `hangar`, account `<proxy>/<token>`,
e.g. `vault/session-token`.

- `hangar up` asks for each token that's missing, typed unseen, and
  stores it: a proxy's own just before it starts, the one its bays log
  in with just before the first of them, when Enter skips it. Without a
  terminal it doesn't ask: what needs the token fails with
  `run 'hangar secret set <proxy>'`.
- `hangar secret set <proxy> [token]` asks for one, or all of them
  again. Piped, it reads one token per line, in the order listed below:
  `op read op://… | hangar secret set vault session-token`.
- `hangar rm <proxy> --data` deletes them.

A token is read only when it's needed, so `ps`, `down` and `logs` work
without them. It can't hold spaces, quotes or non-ASCII characters.

To fetch a token from a password manager instead, name a command; its
first line is the token, and it runs on every `up`:

```json
"vault": {
  "kind": "infisical", "run": "managed",
  "secrets": {
    "session-token": { "command": ["op", "read", "op://Private/infisical/session"] }
  }
}
```

## `infisical`

[Infisical Agent Vault](https://infisical.com/docs/documentation/platform/agent-vault/proxies)'s
proxy, managed by hangar in its own VM.

| Key | Default |
| --- | --- |
| `run` | must be `"managed"` |
| `domain` | none: your Infisical instance's URL, passed as `--domain`; one on this machine (`http://127.0.0.1:<port>`) is reached through the host, and its VM may reach that port |
| `hostPort` | `17323`: the host port the proxy is published on (`127.0.0.1` only) |
| `image` | `infisical/cli:0.45.3`, pinned by digest |

On `up`, hangar creates the VM (1 CPU, 512M) with the proxy's data
folder, `$XDG_DATA_HOME/hangar/proxies/<name>/data/data`, mounted at
`/data`. Then it starts `infisical agent-vault proxy` with the proxy
token on stdin; the proxy enrols with Infisical on every start. A bay
logs in as `x-agent-vault` with the session token as its password, and
trusts the CA the proxy serves at `/_agent-vault/ca`. `hangar logs NAME`
shows the proxy's log, `data/proxy.log`; each start moves a log past
10 MiB to `proxy.log.1`, keeping one old log.

Tokens, in the order `secret set` asks for them: `proxy-token`, then
`session-token`. `up` asks for the session token when a bay behind the
proxy comes up.

### Infisical setup

1. Set up Agent Vault and create a proxy token, as
   [Infisical's proxy docs](https://infisical.com/docs/documentation/platform/agent-vault/proxies)
   describe.
2. Create a session token, shown only once: in the dashboard, **Agent
   Vault > Sessions > Create Session**, or with
   [`infisical agent-vault run … --ttl never --keep-session`](https://infisical.com/docs/cli/commands/agent-vault).
   A `never` TTL lasts until you revoke the session
   ([sessions](https://infisical.com/docs/documentation/platform/agent-vault/sessions)).
   The proxy can't create one itself.
3. Run `hangar up` and paste the two tokens when it asks.

Which hosts a bay may reach, and which credential each gets, you set in
Infisical. A bay needs at least:

- Nix (packages, apps): `cache.nixos.org`, `channels.nixos.org`,
  `cache.numtide.com`, `github.com`, `api.github.com`,
  `codeload.github.com`, `objects.githubusercontent.com`;
- npm: `registry.npmjs.org`;
- the apps' model APIs: `api.anthropic.com` (Claude Code),
  `api.openai.com` (Codex), `openrouter.ai` if you use it;
- Docker pulls, if you run Docker: `registry-1.docker.io`,
  `auth.docker.io`, `production.cloudflare.docker.com`.

**Renewing the session token:** `hangar secret set NAME session-token`,
then `hangar up`. Each bay updates in place (`updated (proxy login)`)
and its apps restart with the new login.

**Rotating the proxy token:** `hangar secret set NAME proxy-token`, then
`hangar up`. The proxy's VM is recreated (`recreated (fingerprint)`)
with an empty data folder, since what the proxy saved belongs to the
old token.

## `agent-vault`

Infisical's open-source [Agent Vault](https://github.com/Infisical/agent-vault)
server, with a free per-request log and OAuth credentials it refreshes
itself. Infisical marks it deprecated, so pin it and keep an exit.

Managed, `"run": "managed"`, hangar runs it in its own VM:

| Key | Default |
| --- | --- |
| `hostPort` | `14322`: the proxy port the bays use (`127.0.0.1` only) |
| `uiPort` | `14321`: its web UI and API (`127.0.0.1` only) |
| `image` | `infisical/agent-vault:0.40.2`, pinned by digest |
| `vault` | `"default"`: the vault the bays log in to |
| `bays` | `{}`: bay → vault, for bays kept apart, see below |

Its data, a SQLite database and its CA, lives in
`$XDG_DATA_HOME/hangar/proxies/<name>/data/data` and is never emptied.
`hangar up` starts it with the master password on stdin. The bays trust
the CA it serves at `/v1/mitm/ca.pem`, and `hangar logs NAME` shows
`server.log`.

Tokens: `master-password`, asked on the first `up` of the proxy, and
`token`, asked when a bay behind it first comes up. To set it up:

1. `hangar up NAME`, and pick a master password. Changing it later
   takes `agent-vault master-password change`, not `secret set`.
2. Open the UI (`hangar ps` shows its URL), register (the first account
   owns the server), and add services and credentials to the vault.
   An OAuth credential's "Connect" runs in your browser, so no OAuth
   token ever enters a bay.
3. On the **Agents** page, create an agent and give it the proxy role
   on the vault; a vault's own Agents tab only adds agents that already
   exist. Paste its token (`av_agt_…`) when `hangar up` asks, just
   before the first bay behind the proxy starts. `up` checks every
   bay's login with the proxy, so a wrong token fails that bay with the
   proxy's reason and the `secret set` that fixes it.

One you run yourself, `"url"`:

| Key | Default |
| --- | --- |
| `url` | required: `http://<host>:<port>`, its proxy port |
| `unsafe` | `false`; `true` to allow a `url` on another machine |
| `ca` | required: see [The `ca` object](#the-ca-object) |
| `vault` | `"default"`: the vault name |
| `bays` | `{}`: bay → vault, as below |

Either way, a bay logs in with the proxy's `token` as the user and the
vault name as the password. Health is a TCP connect to the proxy port.

A bay listed in `bays` logs in to its own vault with its own token,
`token@<bay>`, asked for when that bay first comes up:

```json
"av": { "kind": "agent-vault", "run": "managed",
        "bays": { "oss": "oss-vault" } }
```

Give that token's agent the proxy role on that vault only. A shared
token with access to several vaults isn't a boundary: any bay can name
another vault in its login. Every bay not listed shares `vault` and
`token`. A listed bay must go through this proxy.

## `http`

Any HTTP proxy on this machine.

| Key | Default |
| --- | --- |
| `url` | required: `http://<host>:<port>` |
| `unsafe` | `false`; `true` to allow a `url` on another machine |
| `ca` | none: a CA the bays trust on top of the system's, see below |

There is no login. Health is a TCP connect to the port.

## The `ca` object

| Key | Meaning |
| --- | --- |
| `file` | a PEM file on your machine |
| `url` | where to download it on every `up` |
| `field` | with `url`: the JSON field of the answer that holds the PEM |
| `sha256` | the SHA-256 of the certificate (DER), as hex; colons and a `sha256:` prefix are fine |

Set `file` or `url`. The PEM must be exactly one certificate, and with
`sha256` set it must match.

## Trust

hangar trusts whatever answers on the proxy's address. Any process on
your machine that binds a loopback proxy port first would get the bays'
traffic and logins, and so would anyone on the path to a remote one. Set `ca.sha256` to pin the CA where the kind supports
it (`agent-vault`, `http`).

## Remote proxies

A `url` on another machine needs `"unsafe": true` on the proxy: the
proxy port is plain HTTP, so a bay's login crosses the network
unencrypted. Use it over a network you trust (a VPN, a private subnet);
`up` warns every time.

At `up`, hangar resolves the host to an IPv4 address on this machine. The
bay can reach that IP and port only, and gets no DNS. msb rules by
name are no use here: in msb 0.7.13 a rule by name breaks TLS and a
CONNECT through it. The bay's record keeps the IP, so when the proxy
moves the next `up` recreates the bay.

## Adding a kind

A kind is one file, `src/proxy/<kind>.rs`, plus one line in
`proxy::kinds()`. Nothing else in `src/` may name a kind; the test
`no_core_module_names_a_kind` checks that.

The file exports `build(&Spec) -> Result<Box<dyn Proxy>>`. `Spec` holds
the entry's fields, where it runs (`Location`), your home (for `~` in
paths), the proxy's own data folder and its `Secrets`. `build` checks
its keys with `proxy::only` and gets an external kind's egress from
`proxy::external_egress`. A kind that needs
tokens lists them as `Slot`s from `Proxy::secrets` and reads each with
`Secrets::get` when it needs it, never in `build`: the keychain, the
prompts and `secret set` come from the core.

The `Proxy` trait:

| Method | Returns |
| --- | --- |
| `kind` | the kind's name |
| `egress` | the host port that is the bay's one way out |
| `ca` | the CA PEM a bay trusts, or `None` |
| `login` | the bay's login, or `None` |
| `health` | `Up` or `Down(why)` |
| `managed` | optional: `Some(self)` for a kind hangar runs |
| `secrets` | optional: the tokens it reads from its `Secrets` |

A managed kind implements `Managed`: `vm()` (image, CPUs, memory, the
published ports, the user it runs as, mounts and an extra host port it
may reach), `fingerprint()`, `start()` and `log_file()`. A new
fingerprint recreates the VM with empty mounts, so a kind whose data
must survive keeps it constant.
The core creates, recreates, starts and stops the VM. A capability a kind
lacks is `None`, never an "unsupported" error.

The security contract, which reviews check first:

- Secrets travel on stdin only (`Sandbox::exec`'s `stdin`, the
  keychain tool's) or as `Secret`s in `Login`. Never in argv,
  `VmSpec.env`, a file, a log line or `--json`.
- A bay gets only its login, inside its proxy env, written as root over
  stdin. Enrolment tokens and CA keys never enter a bay.
- A managed proxy runs only in its own VM. Its mounts must be under its
  data folder; the core refuses anything else.
- The fingerprint changes whenever the VM must be recreated. Derive it
  from a secret only through a hash (`ca::sha256_hex`), never the secret
  itself.
- Tests use `FakeProxy`, `FakeSandbox`, `FakeKeychain` and loopback
  servers only.
