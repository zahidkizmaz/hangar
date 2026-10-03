# microsandbox's official release binaries.
{
  lib,
  stdenv,
  fetchurl,
  autoPatchelfHook,
  makeWrapper,
  libcap_ng,
}:
let
  # Renovate bumps the version; update the three hashes in the same PR.
  version = "0.7.6";

  releases = {
    aarch64-darwin = {
      asset = "darwin-aarch64";
      hash = "sha256-pPFyL+5kYNTigT5itg2uTAkgIdgSC3qqnC9LVnnZfYk=";
    };
    x86_64-linux = {
      asset = "linux-x86_64";
      hash = "sha256-2qacamRCibu374ib2GL/unrN35NNReck+hYO3NmlQOA=";
    };
    aarch64-linux = {
      asset = "linux-aarch64";
      hash = "sha256-R+R4dCOxL1byQRn2V8VJdLGGnAhTs9eghzFXZo0pqgM=";
    };
  };

  release =
    releases.${stdenv.hostPlatform.system}
      or (throw "microsandbox: unsupported system ${stdenv.hostPlatform.system}");
in
stdenv.mkDerivation {
  pname = "microsandbox";
  inherit version;

  src = fetchurl {
    url = "https://github.com/superradcompany/microsandbox/releases/download/v${version}/microsandbox-${release.asset}.tar.gz";
    inherit (release) hash;
  };

  sourceRoot = ".";

  nativeBuildInputs = [ makeWrapper ] ++ lib.optional stdenv.hostPlatform.isLinux autoPatchelfHook;

  buildInputs = lib.optionals stdenv.hostPlatform.isLinux [
    libcap_ng
    stdenv.cc.cc.lib
  ];

  # Patching the binary would void the signature that carries the
  # hypervisor entitlement, and macOS kills it at launch.
  dontFixup = stdenv.hostPlatform.isDarwin;

  # msb loads libkrunfw from beside its own binary.
  installPhase = ''
    runHook preInstall
    mkdir -p $out/libexec $out/bin
    cp msb libkrunfw.* $out/libexec/
    makeWrapper $out/libexec/msb $out/bin/msb
    runHook postInstall
  '';

  meta = {
    description = "Local-first microVM runtime for untrusted workloads";
    homepage = "https://microsandbox.dev";
    license = lib.licenses.asl20;
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
    platforms = lib.attrNames releases;
    mainProgram = "msb";
  };
}
