# The few nix-darwin options darwin.nix touches, so checks can evaluate it
# without pulling in nix-darwin.
{ lib, ... }:
let
  inherit (lib) mkOption types;
in
{
  options = {
    environment.systemPackages = mkOption {
      type = types.listOf types.package;
      default = [ ];
    };
    launchd.user.agents = mkOption {
      type = types.attrsOf types.anything;
      default = { };
    };
  };
}
