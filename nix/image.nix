# The bay image: the NixOS system of ./bay/configuration.nix behind
# /sbin/init, generic tools only. Agent CLIs are the user's choice (each
# bay's `packages`), so this image redistributes none of them.
{
  lib,
  dockerTools,
  runCommand,
  toplevel,
}:
let
  root = runCommand "hangar-bay-root" { } ''
    mkdir -p $out/sbin
    ln -s ${toplevel}/init $out/sbin/init
  '';
  imageConfig.Env = [
    "PATH=/run/wrappers/bin:/run/current-system/sw/bin"
  ];
in
# Root's `sh -c` steps find cat, mv and co. on this PATH: a bay package
# must never shadow them.
assert !lib.any (lib.hasInfix "profiles/hangar") imageConfig.Env;
dockerTools.streamLayeredImage {
  name = "hangar-bay";
  contents = [ root ];
  includeNixDB = true;
  # /home/pilot is there for bays with `home = false`. agentd makes /tmp
  # and tmpfiles /var/empty.
  extraCommands = ''
    mkdir -p etc root home/pilot
    chmod 0700 root
  '';
  fakeRootCommands = ''
    chown 1000:1000 home/pilot
  '';
  config = imageConfig;
  passthru = { inherit toplevel; };
}
