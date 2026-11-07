# hangar

hangar runs AI coding agents such as Claude Code and Codex in microVMs,
called bays, on your own machine. It runs on macOS (Apple Silicon) and on
Linux with KVM.

## Why

Agents work best when they can clone repos, install packages, run Docker
and run tests. Doing that on your machine gives them your files, your
network and every token in your environment. In hangar, a bay's only way
out is a proxy you choose. The proxy holds the real credentials; the bay
holds only placeholders. Nothing an agent does runs on your machine:
writable host mounts need an explicit `"unsafe": true`.

- **Declarative.** One `hangar.json`, or the NixOS / nix-darwin module.
  `hangar up` creates, updates or recreates whatever changed and says why.
- **Pluggable proxies.** A managed Agent Vault, Infisical's Agent Vault
  proxy, or any HTTP proxy. Proxy tokens live in the OS keychain.
- **Pluggable sandbox.** Bays run on microsandbox (`msb`) by default.

## Install

You need [microsandbox](https://microsandbox.dev) (`msb`). On Linux you
also need access to `/dev/kvm` and `secret-tool` (libsecret).

```sh
brew install superradcompany/tap/microsandbox # or see microsandbox.dev
cargo install --git https://github.com/zahidkizmaz/hangar
```

On NixOS or nix-darwin, the module installs both: [docs/nix.md](docs/nix.md).

## Quick start

`hangar init` writes a starter config; `hangar config edit` opens it.
A managed Agent Vault proxy and one bay:

```json
{
  "proxies": { "vault": { "kind": "agent-vault", "run": "managed" } },
  "defaults": { "proxy": "vault" },
  "bays": [{ "name": "work", "apps": ["claude-code"] }]
}
```

```sh
hangar up         # starts the proxy, then the bay; asks for tokens once
hangar ps         # what runs, and anything wrong
hangar exec work  # a shell in the bay
```

Until a hangar release is tagged, the default bay image isn't published:
build and load it, or use the Nix module's `imagePackage`
([docs/nix.md](docs/nix.md#local-images)). Setting up the proxy itself is
covered in [docs/proxy-kinds.md](docs/proxy-kinds.md#agent-vault).

Other commands: `down`, `inspect`, `logs`, `cp`, `setup`, `secret set`,
`config show` and `rm`. See `hangar --help`.

## Documentation

- [Using hangar](docs/usage.md): security model, commands, troubleshooting
- [Configuration](docs/configuration.md): every setting, apps and mounts
- [Proxy kinds](docs/proxy-kinds.md): `agent-vault`, `infisical`, `http`
- [Nix](docs/nix.md): the NixOS and nix-darwin modules
- [CLI for scripts and agents](docs/cli.md): `--json` output, exit codes
- [Architecture](docs/architecture.md),
  [sandbox backends](docs/sandbox-backends.md),
  [development](docs/development.md); coding agents start with
  [AGENTS.md](AGENTS.md)

## License

[MIT](LICENSE)
