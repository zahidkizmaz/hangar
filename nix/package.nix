# The `hangar` CLI with the module's settings baked into one JSON file.
{
  lib,
  runCommand,
  makeWrapper,
  writeText,
  cli,
  settings,
  msbPackage ? null,
}:
let
  config = writeText "hangar.json" (builtins.toJSON settings);
in
runCommand "hangar"
  {
    nativeBuildInputs = [ makeWrapper ];
    passthru.config = config;
    meta.mainProgram = "hangar";
  }
  ''
    makeWrapper ${lib.getExe cli} $out/bin/hangar \
      --set HANGAR_NIX_CONFIG ${config} \
      ${lib.optionalString (msbPackage != null) "--prefix PATH : ${msbPackage}/bin"}
  ''
