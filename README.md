# hangar

Locked-down VMs for AI coding agents, with your secrets kept outside them.

## Why

Coding agents do their best work when they can run real tools: clone repos,
install packages, start Docker, run the test suite, open a PR. Doing that
on your own machine hands them your files, your network and every token in
your environment. hangar gives each agent a disposable VM, a bay, whose
only way out is a proxy you choose. The proxy holds the real credentials;
the bay holds placeholders.

## Advantages

- **One egress.** A bay with a proxy can reach that proxy's port and
  nothing else: no direct internet, DNS or LAN.
- **Secrets stay out.** A bay gets only its proxy login, on stdin.
  Enrolment tokens never enter a bay.
- **Pluggable proxies.** A managed Infisical Agent Vault proxy in its own
  VM, an Agent Vault you run yourself, or any HTTP proxy. A new kind is
  one Rust file.
- **No root for apps.** Agents, shells and copied files run as the user
  `pilot`, with rootless Docker and no sudo.
- **Declarative.** One `hangar.json` or the NixOS / nix-darwin module.
  `hangar up` creates, updates or recreates whatever changed and says why.

## Quick start

You need [microsandbox](https://microsandbox.dev) (`msb`); on Linux also
access to `/dev/kvm`. You also need Infisical Agent Vault set up, with a
proxy token and a session token (see
[docs/proxy-kinds.md](docs/proxy-kinds.md#infisical)).

```sh
brew install superradcompany/tap/microsandbox # or see microsandbox.dev
cargo install --git https://github.com/zahidkizmaz/hangar
hangar init # writes ~/.config/hangar/hangar.json
```

Replace `~/.config/hangar/hangar.json` with:

```json
{
  "proxies": {
    "vault": { "kind": "infisical", "run": "managed" }
  },
  "defaults": { "proxy": "vault" },
  "bays": [{ "name": "work", "apps": ["claude-code"] }]
}
```

Until a hangar release is tagged, the default bay image isn't
published: build and load it, or use the Nix module's `imagePackage`
([docs/nix.md](docs/nix.md#local-images)). The Infisical access bundle
must allow the hosts a bay fetches from
([list](docs/proxy-kinds.md#infisical-setup)).

```sh
hangar up         # asks for the proxy's two tokens once (kept in the
                  # OS keychain), then starts the proxy's VM and the bay
hangar ps         # what runs, and anything wrong
hangar exec work  # a shell in the bay
```

On NixOS or nix-darwin, the module takes the same keys:
[docs/nix.md](docs/nix.md).

## Docs

- [Using hangar](docs/usage.md): install, the security model, the
  commands, Docker in a bay, troubleshooting.
- [Configuration](docs/configuration.md): every setting, apps, mounts and
  the bay image.
- [Proxy kinds](docs/proxy-kinds.md): `infisical`, `agent-vault`, `http`,
  and how to add one.
- [Nix](docs/nix.md): the NixOS and nix-darwin modules.
- [CLI for scripts and agents](docs/cli.md): `--json` output and exit
  codes.
- [Architecture](docs/architecture.md),
  [Sandbox backends](docs/sandbox-backends.md) and
  [Development](docs/development.md). Coding agents start with
  [AGENTS.md](AGENTS.md).

## License

[MIT](LICENSE)
