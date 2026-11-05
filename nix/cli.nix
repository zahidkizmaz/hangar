# The `hangar` binary on its own; module.nix adds the module's settings.
{
  lib,
  rustPlatform,
}:
let
  cargo = lib.importTOML ../Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = cargo.package.name;
  inherit (cargo.package) version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../clippy.toml
      ../config
      ../rustfmt.toml
      ../src
      ../tests/cli.rs
      ../tests/fakes
      ../tests/module.json
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;
  useNextest = true;

  meta = {
    inherit (cargo.package) description;
    license = lib.licenses.mit;
    mainProgram = "hangar";
  };
}
