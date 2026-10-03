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
        # The log goes to the XDG state dir, resolved when the agent
        # starts; launchd would neither expand it nor create its folder.
        ProgramArguments = [
          "/bin/sh"
          "-c"
          ''
            log="''${XDG_STATE_HOME:-$HOME/.local/state}/hangar"
            mkdir -p "$log" && exec ${cfg.package}/bin/hangar up >>"$log/hangar.log" 2>&1
          ''
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
