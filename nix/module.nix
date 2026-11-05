# Options and the CLI with its rendered hangar.json, shared by the nixos
# and darwin modules; they put it on PATH and add autostart.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.hangar;
  inherit (lib) mkOption types;

  msb = pkgs.callPackage ./microsandbox.nix { };

  nonEmpty = lib.filterAttrs (_: value: value != null && value != [ ] && value != { });

  # Only what's set here; the CLI fills in config/defaults.json for the rest.
  settings = nonEmpty {
    inherit (cfg) proxies apps;
    defaults = nonEmpty { inherit (cfg.defaults) proxy; };
    sandbox = nonEmpty { inherit (cfg.sandbox) backend; };
    bays = map (bay: bay.rendered) cfg.bays;
  };

  mountModule = types.submodule {
    options = {
      host = mkOption {
        type = types.str;
        description = "Host path (absolute or `~/…`); a missing directory is created (0700).";
      };
      mode = mkOption {
        type = types.enum [
          "ro"
          "rw"
          "copy"
        ];
        default = "ro";
        description = ''
          `ro`: mounted read-only. `rw`: mounted writable, only for a
          dedicated directory in your home. `copy`: copied in on every
          `hangar up`, never secrets.
        '';
      };
    };
  };

  bayModule = types.submodule (
    { config, options, ... }:
    {
      options = {
        name = mkOption {
          type = types.str;
          example = "work";
          description = ''
            The bay's name: lowercase letters and digits, joined by single
            `-`, at most 32 characters. Its VM is `hangar-bay-<name>`.
          '';
        };

        # No default: unset means `defaults.proxy`, null the open internet.
        proxy = mkOption {
          type = types.nullOr types.str;
          example = "vault";
          description = ''
            The proxy (a name in `proxies`) that is the bay's only way out.
            Unset: `defaults.proxy`. `null`: the open internet.
          '';
        };

        apps = mkOption {
          type = types.listOf types.str;
          default = [ ];
          example = [
            "claude-code"
            "paperclip"
          ];
          description = ''
            Apps to set up in the bay: built-ins (see docs/configuration.md) or
            names in `apps`. Each brings its packages, env, run entry and
            ports.
          '';
        };

        packages = mkOption {
          type = types.listOf types.str;
          default = [ ];
          example = [
            "github:numtide/llm-agents.nix#codex"
            "nixpkgs#rtk"
          ];
          description = ''
            Flake installables `hangar up` keeps installed in the bay;
            removing one uninstalls it. Listing an unfree package (e.g.
            claude-code) means accepting its license.
          '';
        };

        env = mkOption {
          type = types.attrsOf types.str;
          default = { };
          example = {
            GIT_AUTHOR_NAME = "agent";
          };
          description = "Plain variables for the bay's login shells, never secrets.";
        };

        mounts = mkOption {
          type = types.attrsOf mountModule;
          default = { };
          example = {
            "~/projects" = {
              host = "~/work/projects";
              mode = "rw";
            };
            "~/.claude/CLAUDE.md" = {
              host = "~/dotfiles/claude/CLAUDE.md";
              mode = "copy";
            };
          };
          description = "VM path (in `~`) -> host path and mode.";
        };

        ports = mkOption {
          type = types.attrsOf types.port;
          default = { };
          example = {
            paperclip = 3200;
          };
          description = "App port name -> host port, for two bays running the same app.";
        };

        image = mkOption {
          type = types.nullOr types.str;
          default = null;
          example = "ghcr.io/zahidkizmaz/hangar-bay@sha256:...";
          description = ''
            Bay image ref. Null: the image released with this hangar,
            `ghcr.io/zahidkizmaz/hangar-bay:v<version>`.
          '';
        };

        imagePackage = mkOption {
          type = types.nullOr types.package;
          default = null;
          description = ''
            Build the bay image locally instead, e.g.
            `inputs.hangar.packages.''${system}.bay-image`. It's Linux only:
            on macOS use `inputs.hangar.packages.aarch64-linux.bay-image`,
            which needs a Linux builder.
          '';
        };

        cpus = mkOption {
          type = types.nullOr types.ints.positive;
          default = null;
          description = "CPUs for the bay's VM. Null: the CLI's default (4).";
        };

        memory = mkOption {
          type = types.nullOr types.str;
          default = null;
          example = "8G";
          description = "Memory for the bay's VM. Null: the CLI's default (`6G`).";
        };

        disk = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = "Disk size of the bay's VM and its data volume. Null: the CLI's default (`40G`).";
        };

        rendered = mkOption {
          type = types.attrs;
          internal = true;
          readOnly = true;
          description = "The bay as hangar.json has it.";
        };
      };

      config.rendered =
        nonEmpty (
          {
            inherit (config)
              name
              apps
              packages
              env
              ports
              image
              cpus
              memory
              disk
              ;
            mounts = lib.mapAttrs (_: mount: { inherit (mount) host mode; }) config.mounts;
          }
          // lib.optionalAttrs (config.imagePackage != null) {
            image = "${config.imagePackage.imageName}:${config.imagePackage.imageTag}";
            imageLoader = "${config.imagePackage}";
          }
        )
        # After the filter: null is a choice here.
        // lib.optionalAttrs options.proxy.isDefined { inherit (config) proxy; };
    }
  );
in
{
  options.services.hangar = {
    enable = lib.mkEnableOption "sandboxed bays whose only way out is a proxy";

    autoStart = mkOption {
      type = types.bool;
      default = false;
      description = "Run `hangar up` at login (off by default: it boots VMs).";
    };

    msbPackage = mkOption {
      type = types.nullOr types.package;
      default = if msb.meta.available then msb else null;
      defaultText = lib.literalExpression "hangar's packaged microsandbox release";
      description = "microsandbox package; null uses `msb` from PATH.";
    };

    # Free-form: each kind checks its own keys when hangar loads.
    proxies = mkOption {
      type = types.attrsOf (types.attrsOf types.anything);
      default = { };
      example = {
        vault = {
          kind = "infisical";
          run = "managed";
          proxyTokenFile = "/run/agenix/infisical-proxy-token";
          sessionTokenFile = "/run/agenix/infisical-session-token";
        };
      };
      description = ''
        Proxies by name (see docs/proxy-kinds.md). Token files are strings,
        never paths, so the secrets stay out of the Nix store.
      '';
    };

    defaults.proxy = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "vault";
      description = "The proxy of every bay that doesn't set `proxy`.";
    };

    bays = mkOption {
      type = types.listOf bayModule;
      default = [ ];
      example = lib.literalExpression ''
        [
          { name = "work"; apps = [ "claude-code" ]; }
          { name = "scratch"; proxy = null; cpus = 2; }
        ]
      '';
      description = ''
        The bays, in the order `up` and `ps` take them. Empty: one bay,
        `default`. Two modules setting this concatenate their lists; a
        duplicate name fails when hangar loads the config.
      '';
    };

    apps = mkOption {
      type = types.attrsOf types.attrs;
      default = { };
      example = {
        web = {
          packages = [ "nixpkgs#nodejs" ];
          run = "npx serve -l 3000";
          ports = [
            {
              name = "web";
              vm = 3000;
              purpose = "Web UI";
            }
          ];
        };
      };
      description = ''
        Your own apps (name -> { packages, env, setup, run, ports }),
        enabled by listing them in a bay's `apps`. One named like a
        built-in replaces it whole.
      '';
    };

    sandbox.backend = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "The sandbox backend. Null: the CLI's default (`msb`).";
    };

    cli = mkOption {
      type = types.package;
      default = pkgs.callPackage ./cli.nix { };
      defaultText = lib.literalExpression "hangar built with your nixpkgs";
      description = "The `hangar` CLI the module wraps with its settings.";
    };

    package = mkOption {
      type = types.package;
      readOnly = true;
      internal = true;
      # The CLI with the settings baked into one JSON file.
      default =
        let
          json = pkgs.writeText "hangar.json" (builtins.toJSON settings);
        in
        pkgs.runCommand "hangar"
          {
            nativeBuildInputs = [ pkgs.makeWrapper ];
            passthru.config = json;
            meta.mainProgram = "hangar";
          }
          ''
            makeWrapper ${lib.getExe cfg.cli} $out/bin/hangar \
              --set HANGAR_NIX_CONFIG ${json} \
              ${lib.optionalString (cfg.msbPackage != null) "--prefix PATH : ${cfg.msbPackage}/bin"}
          '';
    };
  };
}
