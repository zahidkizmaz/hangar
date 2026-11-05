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
  proxies.vault = {
    kind = "infisical";
    run = "managed";
    # NixOS with agenix (owner = your user, see below):
    proxyTokenFile = "/run/agenix/infisical-proxy-token";
    sessionTokenFile = "/run/agenix/infisical-session-token";
    # nix-darwin: plain files only you can read (chmod 600):
    # proxyTokenFile = "~/.secrets/infisical-proxy-token";
    # sessionTokenFile = "~/.secrets/infisical-session-token";
  };
  defaults.proxy = "vault";
  bays = [
    { name = "work"; apps = [ "claude-code" "paperclip" ]; cpus = 6;
      mounts."~/projects" = { host = "~/work/projects"; mode = "rw"; }; }
    { name = "scratch"; proxy = null; }
  ];
};
```

hangar runs as your user (a user service or agent), so it must be able
to read the token files. agenix's default files are root-owned 0400,
which it can't: on NixOS set `age.secrets.<name>.owner` to your user. On
nix-darwin, keep plain files instead:

```sh
mkdir -p ~/.secrets && chmod 700 ~/.secrets
# paste each token into its file, then:
chmod 600 ~/.secrets/infisical-proxy-token ~/.secrets/infisical-session-token
```

hangar never mounts or copies these files into a bay.

## Options

`services.hangar` takes the [configuration](configuration.md) keys as
options and renders them 1:1 into `hangar.json`:

- `proxies`: free-form per kind (see [proxy-kinds.md](proxy-kinds.md));
- `defaults.proxy`;
- `bays`: `name`, `proxy`, `apps`, `packages`, `env`, `mounts` (`host`
  and `mode`, `ro` by default), `ports`, `image`, `imageRepository`,
  `imageLoader`, `cpus`, `memory`, `disk`, plus the Nix-only
  `imagePackage`, which sets `image` and `imageLoader`. Null options are
  left out. Two modules' `bays` lists concatenate. A bay's `proxy` is
  written only when you set it: unset means `defaults.proxy`, `null` the
  open internet;
- `apps`: your own apps, free-form (every app key);
- `sandbox.backend`;

plus:

- `enable`: install `hangar`;
- `autoStart`: run `hangar up` at login (default `false`), see
  [Autostart](#autostart);
- `msbPackage`: microsandbox (default: the packaged release; `null` uses
  `msb` from `PATH`);
- `cli`: the `hangar` package the module wraps.

Each option's description is in `nix/module.nix`. With the module,
`hangar init` refuses to write a config: change `services.hangar` instead,
or set `HANGAR_CONFIG` for a separate one.

Token files are **strings**, never Nix paths, so the secrets stay out of
the Nix store. Keep them readable by the user who runs hangar.

To keep the config in `~/.config/hangar/hangar.json` instead, skip the
module and install the package, which brings `msb` along:

```nix
environment.systemPackages = [ inputs.hangar.packages.${pkgs.system}.default ];
```

The modules build the CLI with your system's nixpkgs (NixOS 26.05 or
newer). To use hangar's own pinned build, set
`services.hangar.cli = inputs.hangar.packages.${pkgs.system}.default;`.

For a local checkout, use `git+file:///path/to/hangar` (a `path:` input
copies `.git`, which fails when git's fsmonitor socket exists) and run
`nix flake update hangar` after editing it.

## Autostart

With `autoStart = true`, `hangar up` runs at login, so the VMs come back
after a reboot. On NixOS it's a user unit of `default.target`; to start it
at boot without a login, enable lingering (`loginctl enable-linger
<user>`). hangar writes its log to `$XDG_STATE_HOME/hangar/hangar.log`
either way. `HANGAR_LOG=debug` raises its level.

## Local images

- Nix: a bay's `imagePackage = inputs.hangar.packages.${pkgs.system}.bay-image;`
  builds the image and loads it into microsandbox once per build. The
  image is Linux only: on macOS use
  `inputs.hangar.packages.aarch64-linux.bay-image`, which needs a Linux
  builder, e.g. nix-darwin's `nix.linux-builder.enable = true;`.
- Without the module, on Linux: load the image under a local tag and
  point hangar at it.

  ```sh
  nix build .#bay-image && ./result | msb load --tag hangar-bay:dev
  ```

  Then set the bay's `image = "hangar-bay:dev"` and run `hangar up NAME`,
  which recreates the bay with it.

What any image must contain is in
[The bay image](configuration.md#the-bay-image).
