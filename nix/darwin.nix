{ config, lib, ... }:
let
  cfg = config.services.hangar;
in
{
  imports = [ ./module.nix ];

  config = lib.mkIf cfg.enable {
    # msb on PATH too, for `msb ls` and friends.
    environment.systemPackages = [
      cfg.package
    ]
    ++ lib.optional (cfg.msbPackage != null) cfg.msbPackage;

    launchd.user.agents.hangar = lib.mkIf cfg.autoStart {
      serviceConfig = {
        # hangar writes its own log, $XDG_STATE_HOME/hangar/hangar.log.
        ProgramArguments = [
          "${cfg.package}/bin/hangar"
          "up"
        ];
        RunAtLoad = true;
        # launchd starts with a bare PATH; only matters when msbPackage = null.
        EnvironmentVariables.PATH = lib.concatStringsSep ":" [
          "/opt/homebrew/bin"
          "/run/current-system/sw/bin"
          "/usr/bin"
          "/bin"
        ];
      };
    };
  };
}
