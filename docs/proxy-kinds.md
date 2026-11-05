# Proxy kinds

A proxy is a bay's one way out. Its `kind` says how hangar runs or
reaches it, how a bay logs in to it, and where the bay gets the CA it
trusts. Every entry under `proxies` has these keys:

| Key | Meaning |
| --- | --- |
| `kind` | `infisical`, `agent-vault` or `http` |
| `run` | `"managed"`: hangar runs it in its own VM, `hangar-proxy-<name>` |
| `url` | it already runs: `http://127.0.0.1:<port>` |

Set `run` or `url`, not both. The other keys belong to the kind; an
unknown one is an error.

## `infisical`

[Infisical Agent Vault](https://infisical.com/docs/documentation/platform/agent-vault/proxies)'s
proxy, managed by hangar in its own VM.

| Key | Default |
| --- | --- |
| `run` | must be `"managed"` |
| `proxyTokenFile` | required: a file holding the proxy token |
| `sessionTokenFile` | required: a file holding the session token the bays log in with |
| `domain` | none: your Infisical instance's URL, passed as `--domain` |
| `hostPort` | `17323`: the host port the proxy is published on (`127.0.0.1` only) |
| `image` | `infisical/cli:0.44.15`, pinned by digest |

On `up`, hangar creates the VM (1 CPU, 512M) with the proxy's data
folder, `$XDG_DATA_HOME/hangar/proxies/<name>/data/data`, mounted at
`/data`. Then it starts `infisical agent-vault proxy` with the proxy
token on stdin; the proxy enrols with Infisical on every start. A bay
logs in as `x-agent-vault` with the session token as its password, and
trusts the CA the proxy serves at `/_agent-vault/ca`. `hangar logs NAME`
shows the proxy's log, `data/proxy.log`; each start moves a log past
10 MiB to `proxy.log.1`, keeping one old log.

Both token files must stay present for every hangar command, since the
proxy is built when the config loads. An empty file is an error.

### Infisical setup

1. Set up Agent Vault and create a proxy token, as
   [Infisical's proxy docs](https://infisical.com/docs/documentation/platform/agent-vault/proxies)
   describe. Put it in a file only you can read, e.g.
   `~/.secrets/infisical-proxy-token`.
2. Create a session token, shown only once: in the dashboard, **Agent
   Vault > Sessions > Create Session**, or with
   [`infisical agent-vault run … --ttl never --keep-session`](https://infisical.com/docs/cli/commands/agent-vault).
   A `never` TTL lasts until you revoke the session
   ([sessions](https://infisical.com/docs/documentation/platform/agent-vault/sessions)).
   Put it in `~/.secrets/infisical-session-token`.
3. Point `proxyTokenFile` and `sessionTokenFile` at the two files and run
   `hangar up`.

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

**Renewing the session token:** replace the file, then run `hangar up`.
Each bay updates in place (`updated (proxy login)`) and its apps restart
with the new login.

**Rotating the proxy token:** replace the file, then run `hangar up`. The
proxy's VM is recreated (`recreated (fingerprint)`); its data folder is
kept.

## `agent-vault`

An [Agent Vault](https://github.com/Infisical/agent-vault) you run
yourself on this machine.

| Key | Default |
| --- | --- |
| `url` | required: `http://127.0.0.1:<port>`, its proxy port |
| `ca` | required: see [The `ca` object](#the-ca-object) |
| `tokenFile` | required: a file holding the token the bays log in with |
| `vault` | `"default"`: the vault name |

A bay logs in with the token as the user and the vault name as the
password. Health is a TCP connect to the port.

## `http`

Any HTTP proxy on this machine.

| Key | Default |
| --- | --- |
| `url` | required: `http://127.0.0.1:<port>` |
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

hangar trusts whatever answers on the proxy's loopback host port. Any
process on your machine that binds that port first would get the bays'
traffic and logins. Set `ca.sha256` to pin the CA where the kind supports
it (`agent-vault`, `http`).

## Remote proxies

Not supported yet: a `url` whose host isn't loopback is an error. A bay's
egress is one port on the host. msb 0.7.6 accepts a per-domain egress
rule, but TLS to the allowed host then fails (Spike A4), so remote
proxies need IP and port rules: the sandbox's egress spec gets a variant
for a remote address, which msb turns into a rule like today's
`allow@host:tcp:<port>`.

## Adding a kind

A kind is one file, `src/proxy/<kind>.rs`, plus one line in
`proxy::kinds()`. Nothing else in `src/` may name a kind; the test
`no_core_module_names_a_kind` checks that.

The file exports `build(&Spec) -> Result<Box<dyn Proxy>>`. `Spec` holds
the entry's fields, where it runs (`Location`), your home (for `~` in
paths) and the proxy's own data folder. `build` checks its keys with
`proxy::only`, reads token files with `proxy::read_token`, and gets an
external kind's port from `proxy::external_url` and
`proxy::loopback_port`.

The `Proxy` trait:

| Method | Returns |
| --- | --- |
| `kind` | the kind's name |
| `egress` | the host port that is the bay's one way out |
| `ca` | the CA PEM a bay trusts, or `None` |
| `login` | the bay's login, or `None` |
| `health` | `Up` or `Down(why)` |
| `managed` | optional: `Some(self)` for a kind hangar runs |

A managed kind implements `Managed`: `vm()` (image, CPUs, memory, the
published port, mounts and a fingerprint), `start()` and `log_file()`.
The core creates, recreates, starts and stops the VM. A capability a kind
lacks is `None`, never an "unsupported" error.

The security contract, which reviews check first:

- Secrets travel on stdin only (`Sandbox::exec`'s `stdin`) or as
  `Secret`s in `Login`. Never in argv, `VmSpec.env`, a log line or
  `--json`.
- A bay gets only its login, inside its proxy env, written as root over
  stdin. Enrolment tokens and CA keys never enter a bay.
- A managed proxy runs only in its own VM. Its mounts must be under its
  data folder; the core refuses anything else.
- The fingerprint changes whenever the VM must be recreated. Derive it
  from a secret only through a hash (`ca::sha256_hex`), never the secret
  itself.
- Tests use `FakeProxy`, `FakeSandbox` and loopback servers only.
