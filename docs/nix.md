# Nix

hangar ships system modules for NixOS (`systemd` user service) and
nix-darwin (`launchd` user agent). No home-manager needed.

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

## Options

`services.hangar` takes the [configuration](configuration.md) keys as
options (`bays`, `tower.routes`, `tower.credentialFiles`,
`tower.masterPasswordFile`, `appDefinitions`, `stateDir`; two modules'
`bays` lists concatenate), plus:

- `enable`: install `hangar`;
- `autoStart`: run `hangar up` at login (default `false`), see
  [Autostart](#autostart);
- `msbPackage`: microsandbox (default: the packaged release, installed for
  you; `null` uses `msb` from `PATH`);
- `bays.*.imagePackage`: build the bay image locally (see
  [Local images](#local-images));
- `settings`: any other `hangar.json` key, e.g.
  `{ tower.agentVault.adminPort = 14421; }`; its lists replace the
  generated ones whole.

Each option's description is in `nix/module.nix`. The module renders
`hangar.json`; `hangar init` then refuses to write one, so change
`services.hangar` instead (or set `HANGAR_CONFIG` for a separate config).

Secret options take **strings**, so the files are never copied into the Nix
store. On Linux the package builds in libsecret's `secret-tool` for the
keychain.

On Nix, microsandbox comes with the module: no Homebrew needed.

The modules build the CLI with your system's nixpkgs (NixOS 26.05 or
newer). To use hangar's own pinned build instead, set
`services.hangar.cli = inputs.hangar.packages.${pkgs.system}.default;`.

For a local checkout, use `git+file:///path/to/hangar` (a `path:` input
copies `.git`, which fails when git's fsmonitor socket exists) and run
`nix flake update hangar` after editing it.

## Autostart

With `autoStart = true`, `hangar up` runs at login, so the VMs come back
after a reboot. On NixOS it starts with the graphical session, where the
Secret Service is unlocked; a headless host sets `tower.masterPasswordFile`
and starts at boot instead, which for a systemd user unit also needs
lingering (`loginctl enable-linger <user>`). On macOS its log is
`$XDG_STATE_HOME/hangar/hangar.log` (`~/.local/state/hangar`); on Linux,
the journal. `HANGAR_LOG=debug` raises its log level.

## Local images

- Nix: a bay's `imagePackage = inputs.hangar.packages.${pkgs.system}.bay-image;`
  builds the image and loads it into microsandbox once per build. The
  image is Linux only: on macOS use
  `inputs.hangar.packages.aarch64-linux.bay-image`, which needs a Linux
  builder, e.g. nix-darwin's `nix.linux-builder.enable = true;`.
- Without the module, on Linux: load the image under a local tag and
  point hangar at it. A cached tag is used without contacting a registry.

  ```sh
  nix build .#bay-image && ./result | msb load --tag hangar-bay:dev
  ```

  Then set the bay's `image = "hangar-bay:dev"` and run
  `hangar destroy NAME && hangar up NAME`.

What any image must contain is in
[The bay image](configuration.md#the-bay-image).
