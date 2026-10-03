# Options and the CLI, shared by the nixos and darwin modules.
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
  settings = lib.recursiveUpdate cfg.settings (
    {
      tower = {
        inherit (cfg.tower) credentialFiles;
        routes = map renderRoute cfg.tower.routes;
      }
      // lib.optionalAttrs (cfg.tower.masterPasswordFile != null) {
        inherit (cfg.tower) masterPasswordFile;
      };
    }
    // lib.optionalAttrs (cfg.bays != [ ]) { bays = map renderBay cfg.bays; }
    // lib.optionalAttrs (cfg.appDefinitions != { }) { inherit (cfg) appDefinitions; }
    // lib.optionalAttrs (cfg.stateDir != null) {
      inherit (cfg) stateDir;
    }
  );

  renderRoute =
    route:
    lib.filterAttrs (_: value: value != null) {
      inherit (route)
        name
        host
        auth
        extra
        ;
    };

  renderBay =
    bay:
    nonEmpty (
      {
        inherit (bay)
          name
          apps
          ports
          image
          cpus
          memory
          disk
          packages
          env
          run
          files
          mounts
          ;
      }
      // lib.optionalAttrs (bay.imagePackage != null) {
        image = "${bay.imagePackage.imageName}:${bay.imagePackage.imageTag}";
        imageLoader = "${bay.imagePackage}";
      }
    )
    # On is the CLI's default; only off is written, after the filter.
    // lib.optionalAttrs (!bay.home) { home = false; }
    // lib.optionalAttrs (!bay.cache) { cache = false; };

  routeModule = types.submodule {
    options = {
      name = mkOption {
        type = types.str;
        description = "Unique among routes; an app's route names are taken.";
      };
      host = mkOption {
        type = types.str;
        example = "api.example.com";
        description = "The host the tower forwards to (`*.example.com` matches one label).";
      };
      auth = mkOption {
        type = types.attrs;
        example = {
          type = "bearer";
          token = "EXAMPLE_TOKEN";
        };
        description = "What the tower injects: `{ type; … }` with credential names only.";
      };
      extra = mkOption {
        type = types.nullOr types.attrs;
        default = null;
        description = "Broker backend fields, passed through unchecked.";
      };
    };
  };

  bayModule = types.submodule {
    options = {
      name = mkOption {
        type = types.str;
        example = "work";
        description = ''
          The bay's name: lowercase letters and digits, joined by single
          `-`, at most 32 characters. Its VM is `hangar-bay-<name>`.
        '';
      };

      apps = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [
          "nix"
          "github"
          "claude-code"
          "paperclip"
        ];
        description = ''
          Apps to set up in the bay: built-ins (see the README) or
          `appDefinitions` names. Each brings its packages, routes, env,
          run entry and ports.
        '';
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
          `inputs.hangar.packages.''${system}.bay-image`. On macOS this
          needs a Linux builder.
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
        description = "Root disk size of the bay's VM. Null: the CLI's default (`40G`).";
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
        description = ''
          Plain variables for the bay's login shells, never secrets. Every
          injected credential already gets a placeholder variable.
        '';
      };

      run = mkOption {
        type = types.attrsOf types.str;
        default = { };
        example = {
          worker = "npm run worker";
        };
        description = ''
          Long-running commands (name -> command) `hangar up` starts in the
          bay; one named like an app replaces the app's.
        '';
      };

      files = mkOption {
        type = types.attrsOf types.str;
        default = { };
        example = {
          "~/.claude/CLAUDE.md" = "~/dotfiles/claude/CLAUDE.md";
          "~/.claude/agents" = "~/dotfiles/claude/agents";
        };
        description = ''
          Config files or directories copied into the bay on every
          `hangar up` (VM path -> host path); secrets are refused.
        '';
      };

      home = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Keep the bay's home in `<stateDir>/bays/<name>/home`, so apps'
          data survives a new VM; false keeps it on the VM's disk.
        '';
      };

      cache = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Keep a cache of the bay's packages in
          `$XDG_CACHE_HOME/hangar/bays/<name>`, so a new VM doesn't download
          them again; only correctly signed paths are used from it.
        '';
      };

      mounts = mkOption {
        type = types.attrsOf (
          types.submodule {
            options = {
              host = mkOption {
                type = types.str;
                description = "Host directory (absolute or `~/…`); created (0700) if missing.";
              };
              writable = mkOption {
                type = types.bool;
                default = false;
                description = ''
                  Let the VM write to it. Only for a dedicated directory in
                  your home, never one a host program reads its config from.
                '';
              };
            };
          }
        );
        default = { };
        example = {
          "~/projects" = {
            host = "~/work/projects";
            writable = true;
          };
        };
        description = ''
          Host directories mounted into the bay when it's created (VM path
          -> source), read-only unless `writable`.
        '';
      };
    };
  };
in
{
  options.services.hangar = {
    enable = lib.mkEnableOption "sandboxed bays behind a credential tower";

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

    stateDir = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = ''
        Vault data, tokens, the bays' records and home folders. Null:
        the CLI uses `$XDG_DATA_HOME/hangar` (`~/.local/share/hangar`) of
        whoever runs it.
      '';
    };

    bays = mkOption {
      type = types.listOf bayModule;
      default = [ ];
      example = lib.literalExpression ''
        [
          { name = "work"; apps = [ "nix" "github" "claude-code" ]; }
          { name = "oss"; apps = [ "nix" "github" "claude-code" ]; cpus = 2; }
        ]
      '';
      description = ''
        The bays, in the order `up` and `status` take them. Empty: one bay,
        `default`. Two modules setting this concatenate their lists; a
        duplicate name fails when hangar loads the config.
      '';
    };

    tower = {
      routes = mkOption {
        type = types.listOf routeModule;
        default = [ ];
        example = [
          {
            name = "jira";
            host = "example.atlassian.net";
            auth = {
              type = "basic";
              username = "JIRA_USER";
              password = "JIRA_TOKEN";
            };
          }
        ];
        description = ''
          Hosts the tower forwards to, next to the ones the bays' apps
          bring; hangar ships no routes of its own. A name an app's route
          has is an error, and so is a second route for a host.
        '';
      };

      credentialFiles = mkOption {
        type = types.attrsOf types.str;
        default = { };
        example = {
          GITHUB_TOKEN = "/run/agenix/github-token";
        };
        description = "Vault credential key -> file holding its value.";
      };

      # A string, not a path: a path literal would copy the secret into the store.
      masterPasswordFile = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = lib.literalExpression "config.sops.secrets.hangar-master-password.path";
        description = ''
          Explicit opt-in: a file holding the vault's master password, e.g.
          a sops-nix or agenix secret on Linux. By default the password
          lives in the OS keychain, generated on the first `hangar up`.
        '';
      };
    };

    appDefinitions = mkOption {
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
        Your own apps (name -> { packages, routes, env, credentials, setup,
        run, ports }), enabled by listing them in a bay's `apps`. One named
        like a built-in replaces it whole.
      '';
    };

    settings = mkOption {
      type = types.attrs;
      default = { };
      example = {
        tower.agentVault.adminPort = 14421;
      };
      description = ''
        Any other hangar.json setting (see the README), such as the sandbox
        and the tower's backend settings (`sandbox`, `tower.backend`,
        `tower.agentVault`). Lists here replace the generated ones whole,
        so set bays and routes with the typed options.
      '';
    };

    # The flake's modules set this to hangar's own build, so the CLI never
    # depends on the consumer's (possibly older) Rust toolchain.
    cli = mkOption {
      type = types.package;
      internal = true;
      default = pkgs.callPackage ./cli.nix { };
    };

    package = mkOption {
      type = types.package;
      readOnly = true;
      internal = true;
      default = pkgs.callPackage ./package.nix {
        inherit settings;
        inherit (cfg) cli msbPackage;
      };
    };
  };

  config = lib.mkIf cfg.enable {
    # msb on PATH too, for `msb ls` and friends.
    environment.systemPackages = [
      cfg.package
    ]
    ++ lib.optional (cfg.msbPackage != null) cfg.msbPackage;
  };
}
