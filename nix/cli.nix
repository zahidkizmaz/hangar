# The `hangar` binary on its own; module.nix adds the module's settings.
{
  lib,
  stdenv,
  rustPlatform,
  libsecret,
  curl,
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

  # The keychain tool and curl are called by these absolute paths, never
  # via PATH. curl is baked in on every system, so the package doesn't
  # depend on the host's.
  env = {
    HANGAR_CURL = "${curl}/bin/curl";
  }
  // lib.optionalAttrs stdenv.hostPlatform.isLinux {
    HANGAR_SECRET_TOOL = "${libsecret}/bin/secret-tool";
  };

  # No release build may honor the tests' keychain or curl redirect.
  postInstall = ''
    for name in HANGAR_TEST_KEYCHAIN_TOOL HANGAR_TEST_CURL; do
      if grep -q "$name" $out/bin/hangar; then
        echo "release binary reads $name" >&2
        exit 1
      fi
    done
  '';

  meta = {
    inherit (cargo.package) description;
    license = lib.licenses.mit;
    mainProgram = "hangar";
  };
}
