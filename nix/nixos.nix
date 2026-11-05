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

    systemd.user.services.hangar = lib.mkIf cfg.autoStart {
      description = "hangar: sandboxed bays for AI agents";
      wantedBy = [ "default.target" ];
      # A user unit: each user who logs in runs their own hangar.
      path = [ "/run/current-system/sw" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = "${cfg.package}/bin/hangar up";
        ExecStop = "${cfg.package}/bin/hangar down";
      };
    };
  };
}
