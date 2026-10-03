{
  description = "hangar: sandboxed workspaces for AI agents";

  inputs.nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;

      # A bay is always Linux: macOS gets the image of its own arch.
      linuxOf = builtins.replaceStrings [ "darwin" ] [ "linux" ];

      settings = {
        enable = true;
        autoStart = true;
        tower.credentialFiles.GITHUB_TOKEN = "/run/secrets/github-token";
        appDefinitions.web.ports = [
          {
            name = "web";
            vm = 3100;
            purpose = "Example web UI";
          }
        ];
        bays = [
          {
            name = "default";
            apps = [
              "nix"
              "github-token"
              "claude-code"
              "web"
            ];
            image = "ghcr.io/example/hangar-bay:latest";
            packages = [ "github:numtide/llm-agents.nix#codex" ];
            env.EXAMPLE_MODE = "demo";
            run.web = "serve --port 3100";
            files."~/.config/app/config.toml" = "~/dotfiles/app/config.toml";
            mounts."~/.app-data" = {
              host = "~/work/app-data";
              writable = true;
            };
            cache = false;
          }
          {
            name = "oss";
            home = false;
            # A stub: the fixture checks the rendering, not the image.
            imagePackage = {
              type = "derivation";
              outPath = "/nix/store/00000000000000000000000000000000-bay-image";
              imageName = "example/bay";
              imageTag = "dev";
            };
          }
        ];
      };

      withCli =
        module:
        { pkgs, ... }:
        {
          imports = [ module ];
          services.hangar.cli = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        };

      # nix-darwin isn't an input: a stub of the options darwin.nix uses.
      evalDarwin =
        pkgs: hangar:
        (pkgs.lib.evalModules {
          specialArgs = { inherit pkgs; };
          modules = [
            self.darwinModules.default
            ./nix/tests/darwin-stub.nix
            { services.hangar = hangar; }
          ];
        }).config;

      evalNixos =
        system: hangar:
        (nixpkgs.lib.nixosSystem {
          inherit system;
          modules = [
            self.nixosModules.default
            {
              boot.loader.grub.enable = false;
              fileSystems."/" = {
                device = "none";
                fsType = "tmpfs";
              };
              system.stateVersion = "26.05";
              users.users.tester.isNormalUser = true;
              services.hangar = hangar;
            }
          ];
        }).config;
    in
    {
      nixosModules.default = withCli ./nix/nixos.nix;
      darwinModules.default = withCli ./nix/darwin.nix;

      packages = forAllSystems (system: {
        default = nixpkgs.legacyPackages.${system}.callPackage ./nix/cli.nix { };
        bay-image = nixpkgs.legacyPackages.${linuxOf system}.callPackage ./nix/image.nix {
          toplevel =
            (nixpkgs.lib.nixosSystem {
              modules = [
                ./nix/bay/configuration.nix
                { nixpkgs.hostPlatform = linuxOf system; }
              ];
            }).config.system.build.toplevel;
        };
      });

      devShells = forAllSystems (system: {
        default =
          with nixpkgs.legacyPackages.${system};
          mkShell {
            # Rust and its tools come from rust-toolchain.toml and
            # Cargo.toml (cargo-run-bin), the same as without Nix.
            packages = [
              rustup
              cargo-run-bin
              cargo-binstall
              stdenv.cc
              nixfmt
              yamlfmt
              yamllint
              shellcheck
              actionlint
              zizmor
              prek
              gitleaks
            ];
          };
      });

      checks = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          eval = if pkgs.stdenv.hostPlatform.isDarwin then evalDarwin pkgs else evalNixos system;
          config = eval settings;
          cli = config.services.hangar.package;
          noBays = (eval (settings // { bays = [ ]; })).services.hangar.package.config.text;
          msbOnPath = builtins.elem config.services.hangar.msbPackage config.environment.systemPackages;
          autoStart =
            if pkgs.stdenv.hostPlatform.isDarwin then
              config.launchd.user.agents ? hangar
            else
              config.systemd.user.services ? hangar;
          # The tests run (via nextest) in the package's checkPhase. fmt and
          # clippy run in CI's prek with the pinned toolchain only.
          rust = self.packages.${system}.default;
        in
        # The module writes no `bays` when none are set: the CLI's default.
        assert !(builtins.fromJSON noBays ? bays);
        # Root's `sh -c` steps find cat, mv and co. on the image's PATH: a
        # bay package must never shadow them.
        assert !pkgs.lib.any (pkgs.lib.hasInfix "profiles/hangar") self.packages.${system}.bay-image.env;
        {
          inherit rust;
          module = pkgs.runCommand "hangar-module-check" { nativeBuildInputs = [ pkgs.jq ]; } ''
            ${pkgs.lib.optionalString (!autoStart || !msbOnPath) "exit 1"}
            export HOME=$TMPDIR
            # Under the module, init refuses: its file would be ignored.
            if ${cli}/bin/hangar init; then exit 1; fi
            HANGAR_CONFIG=$HOME/standalone.json ${cli}/bin/hangar init
            jq -e '.bays[0].name == "default" and (keys == ["bays", "tower"])' \
              $HOME/standalone.json
            # The module only generates hangar.json: same as writing it by hand.
            diff <(jq -S . ${cli.config}) <(jq -S . ${./tests/module.json})
            touch $out
          '';
        }
      );
    };
}
