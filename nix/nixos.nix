{ config, lib, ... }:
let
  cfg = config.services.hangar;
  # The keychain is the Secret Service on the session bus, unlocked at a
  # graphical login. Without a password file, start with that session; a
  # headless host sets tower.masterPasswordFile and starts at boot.
  target =
    if cfg.tower.masterPasswordFile == null then "graphical-session.target" else "default.target";
in
{
  imports = [ ./module.nix ];

  config = lib.mkIf (cfg.enable && cfg.autoStart) {
    systemd.user.services.hangar = {
      description = "hangar: sandboxed bays behind a credential tower";
      wantedBy = [ target ];
      after = [ target ];
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
