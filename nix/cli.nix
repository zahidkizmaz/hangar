# The `hangar` binary on its own; package.nix adds the module's settings.
{
  lib,
  stdenv,
  rustPlatform,
  libsecret,
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
  # Only a debug build takes the CLI tests' fake keychain; a release one
  # would call the real keychain.
  checkType = "debug";

  # The keychain tool is called by this absolute path, never via PATH.
  env = lib.optionalAttrs stdenv.hostPlatform.isLinux {
    HANGAR_SECRET_TOOL = "${libsecret}/bin/secret-tool";
  };

  # No release build may honor the tests' keychain redirect.
  postInstall = ''
    if grep -q HANGAR_TEST_KEYCHAIN_TOOL $out/bin/hangar; then
      echo "release binary reads HANGAR_TEST_KEYCHAIN_TOOL" >&2
      exit 1
    fi
  '';

  meta = {
    inherit (cargo.package) description;
    license = lib.licenses.mit;
    mainProgram = "hangar";
  };
}
