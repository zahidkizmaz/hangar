{
  description = "hangar: sandboxed workspaces for AI agents";

  inputs.nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";

  outputs =
    { nixpkgs, ... }:
    let
      inherit (nixpkgs) lib;
      forAllSystems = lib.genAttrs [
        "aarch64-darwin"
        "x86_64-linux"
        "aarch64-linux"
      ];
    in
    {
      nixosModules.default = ./nix/nixos.nix;
      darwinModules.default = ./nix/darwin.nix;

      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          cli = pkgs.callPackage ./nix/cli.nix { };
          msb = pkgs.callPackage ./nix/microsandbox.nix { };
          bay = lib.nixosSystem {
            modules = [
              ./nix/bay/configuration.nix
              { nixpkgs.hostPlatform = system; }
            ];
          };
        in
        {
          # hangar plus the msb it drives; it reads ~/.config/hangar.
          default = pkgs.symlinkJoin {
            name = "hangar-${cli.version}";
            paths = [
              cli
              msb
            ];
            nativeBuildInputs = [ pkgs.makeWrapper ];
            postBuild = "wrapProgram $out/bin/hangar --prefix PATH : ${msb}/bin";
            meta.mainProgram = "hangar";
          };
        }
        # A bay is a Linux VM; macOS builds aarch64-linux's (docs/nix.md).
        // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          bay-image = pkgs.callPackage ./nix/image.nix { inherit (bay.config.system.build) toplevel; };
        }
      );

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

      checks = forAllSystems (system: {
        # The package's checkPhase runs the tests (nextest); fmt and clippy
        # run in prek with the pinned toolchain only.
        rust = nixpkgs.legacyPackages.${system}.callPackage ./nix/cli.nix { };
        module = nixpkgs.legacyPackages.${system}.callPackage ./nix/tests/module.nix { };
      });
    };
}
