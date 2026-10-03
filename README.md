# hangar

Locked-down VMs for AI coding agents, with your secrets kept outside them.

## Why

Coding agents do their best work when they can run real tools: clone repos,
install packages, start Docker, run the test suite, open a PR. Doing that
on your own machine hands them your files, your network and every token in
your environment. hangar gives each agent a disposable VM instead, and
lets it use your credentials without ever seeing them.

## Advantages

- **Secrets never enter the VM.** A bay holds placeholders only; the tower
  swaps in the real credential on the way out, for allowlisted hosts only.
- **One-port egress.** A bay's only allowed connection is the tower's
  proxy port: no direct internet, DNS, LAN or vault admin port.
- **Deny by default.** hangar ships no routes; every other host gets a
  `403` that names it, so you add exactly what you need.
- **Isolated bays.** Each bay has its own home, package cache and vault
  token, and bays never reach each other.
- **No root for apps.** Agents, shells and copied files run as the user
  `pilot`, with rootless Docker and no sudo.
- **Declarative config.** Nix modules for NixOS and nix-darwin, or one
  `hangar.json`; apps are data, not code.
- **Pluggable backends.** The sandbox (microsandbox) and the credential
  broker (agent-vault) sit behind traits, so others can be added.

## How it works

```
 your machine (macOS / Linux)
 ├─ hangar-bay-work ─┐   bays: microsandbox VMs where agents run
 ├─ hangar-bay-oss  ─┤   (NixOS, systemd, user pilot)
 │                   │   only egress: the tower's proxy port
 │                   ▼
 └─ hangar-tower ──────▶ internet (allowlisted hosts only)
      agent-vault: holds the real credentials, injects them per host,
      refuses everything else
```

`hangar up` creates the tower and one VM per bay, applies the routes and
credentials, installs each bay's packages and starts its apps. Details are
in [docs/architecture.md](docs/architecture.md).

## Quick start

You need [microsandbox](https://microsandbox.dev) (`msb`); on Linux also
access to `/dev/kvm`. On NixOS or nix-darwin, the module installs both
hangar and microsandbox:

```nix
# flake.nix: inputs.hangar.url = "github:zahidkizmaz/hangar";
# then inputs.hangar.nixosModules.default or .darwinModules.default
services.hangar = {
  enable = true;
  bays = [ { name = "default"; apps = [ "nix" "github" "claude-code" ]; } ];
};
```

Without Nix, install microsandbox and build hangar from source:

```sh
brew install superradcompany/tap/microsandbox # or see microsandbox.dev
cargo install --git https://github.com/zahidkizmaz/hangar
hangar init # writes ~/.config/hangar/hangar.json
```

and list your bays in `~/.config/hangar/hangar.json`:

```json
{
  "bays": [{ "name": "default", "apps": ["nix", "github", "claude-code"] }],
  "tower": { "credentialFiles": {} }
}
```

Then:

```sh
hangar up                                     # create the tower and bays
claude setup-token                            # on your machine
hangar credential set CLAUDE_CODE_OAUTH_TOKEN # paste it (hidden)
hangar status                                 # health, bays and ports
hangar shell                                  # a shell in the bay
```

The master password is generated into your OS keychain on the first `up`.
When an agent hits a host that isn't allowed yet, the `403` names it: add
a route under `tower.routes`
([Allowing another host](docs/configuration.md#allowing-another-host)).

## Docs

- [Using hangar](docs/usage.md): install, the security model, first run,
  credentials and the vault UI, ports, commands, Paperclip and Claude
  Code, Docker in a bay, troubleshooting.
- [Configuration](docs/configuration.md): every setting, apps, routes,
  packages, the bay's home and cache, files, mounts and the bay image.
- [Nix](docs/nix.md): the NixOS and nix-darwin modules, autostart and
  local images.
- [CLI for scripts and agents](docs/cli.md): `--json` output, exit codes
  and flows.
- [Architecture](docs/architecture.md): how `up` works, state, the bay
  image and the file map.
- [Sandbox backends](docs/sandbox-backends.md): the contract a sandbox
  backend must meet.
- [Development](docs/development.md): checks, tests, CI and releases.
  Coding agents start with [AGENTS.md](AGENTS.md).

## License

[MIT](LICENSE)
