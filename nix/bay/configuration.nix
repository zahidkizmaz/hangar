# The bay as a NixOS system. msb's agentd sets up the VM, then hands PID 1
# to /sbin/init (`--init`): NixOS stage 2, then systemd
# (docs/architecture.md, "The bay image").
{
  config,
  lib,
  pkgs,
  modulesPath,
  ...
}:
let
  systemctl = "${config.systemd.package}/bin/systemctl";
in
{
  imports = [ "${modulesPath}/profiles/minimal.nix" ];

  # msb brings the kernel and agentd is stage 1: no initrd, bootloader or
  # hardware units.
  boot.isContainer = true;
  networking.hostName = "";
  # agentd writes resolv.conf and /etc/hosts and configures eth0.
  networking.resolvconf.enable = false;
  environment.etc.hosts.enable = false;
  networking.dhcpcd.enable = false;
  networking.firewall.enable = false;
  # agentd owns hvc0; the gettys would wait 90 s for it.
  systemd.services =
    lib.genAttrs
      [
        "serial-getty@hvc0"
        "console-getty"
        "getty@tty1"
        "autovt@tty1"
      ]
      (_: {
        enable = false;
      })
    // {
      # hangar writes proxy.env at `up`; `-` lets the daemons start before.
      nix-daemon.serviceConfig.EnvironmentFile = "-/etc/hangar/proxy.env";
      hangar-proxy-env.serviceConfig = {
        Type = "oneshot";
        ExecStart = [
          "${systemctl} try-restart nix-daemon.service"
          "${systemctl} --user --machine=pilot@.host restart docker.service"
        ];
      };
    };
  # hangar replaces proxy.env only when it changed (it names the tower's CA
  # too): restart the daemons that read it.
  systemd.paths.hangar-proxy-env = {
    wantedBy = [ "paths.target" ];
    pathConfig.PathChanged = "/etc/hangar/proxy.env";
  };

  # Nobody logs in: hangar reaches root through `msb exec`.
  users.mutableUsers = false;
  users.allowNoPasswordLogin = true;
  security.sudo.enable = false;
  # Left: newuidmap/newgidmap (capabilities) for rootless Docker, and
  # setuid unix_chkpwd, without which PAM fails user@1000.
  security.wrappers =
    lib.genAttrs
      [
        "su"
        "sg"
        "newgrp"
        "mount"
        "umount"
      ]
      (_: {
        enable = lib.mkForce false;
      });
  users.users.pilot = {
    isNormalUser = true;
    uid = 1000;
    group = "pilot";
    # Often a host mount: NixOS must never create or chmod it.
    createHome = false;
    # Starts user@1000, and so pilot's dockerd, without a login.
    linger = true;
  };
  users.groups.pilot.gid = 1000;

  virtualisation.docker.rootless = {
    enable = true;
    setSocketVariable = true;
    daemon.settings.data-root = "/var/lib/pilot/docker";
  };
  systemd.user.services.docker.serviceConfig = {
    EnvironmentFile = "-/etc/hangar/proxy.env";
    # Overrides the module's TimeoutSec = 0 for starts, so a restart can't
    # hang `up`.
    TimeoutStartSec = "60s";
  };

  nix.settings = {
    experimental-features = [
      "nix-command"
      "flakes"
    ];
    sandbox = false;
    # llm-agents.nix's binary cache, for agent CLIs in a bay's packages.
    extra-substituters = [ "https://cache.numtide.com" ];
    extra-trusted-public-keys = [
      "niks3.numtide.com-1:DTx8wZduET09hRmMtKdQDxNNthLQETkc/yaX7M4qK0g="
    ];
  };
  nix.channel.enable = false;
  # Image size: no nixos-rebuild and no copy of nixpkgs.
  system.disableInstallerTools = true;
  nixpkgs.flake.setNixPath = false;
  nixpkgs.flake.setFlakeRegistry = false;

  # agentd appends to both on every boot, before activation: through a
  # store symlink it would write into /nix/store.
  environment.etc.profile.mode = "0644";
  environment.etc."ssl/certs/ca-certificates.crt".mode = "0644";
  # Untouched by agentd: hangar adds the tower's CA to it.
  environment.etc."hangar/system-ca.crt".source = config.security.pki.caBundle;

  systemd.tmpfiles.rules = [
    "d /var/lib/hangar 0755 root root -"
    "z /dev/net/tun 0666 root root -"
    "d /var/lib/pilot 0755 pilot pilot -"
    "d /var/log/hangar 0755 pilot pilot -"
    "d /run/hangar-run 0755 pilot pilot -"
  ];
  environment.profiles = lib.mkBefore [ "/nix/var/nix/profiles/hangar" ];

  # `msb exec` is no PAM login. hangar writes proxy.env and bay.env, both
  # KEY='value' lines.
  environment.extraInit = lib.mkBefore ''
    : "''${USER:=$(id -un)}" "''${LOGNAME:=$USER}"
    export USER LOGNAME
    if [ -z "''${XDG_RUNTIME_DIR:-}" ] && [ -d "/run/user/$(id -u)" ]; then
      export XDG_RUNTIME_DIR="/run/user/$(id -u)"
    fi
    for f in /etc/hangar/proxy.env /etc/hangar/bay.env; do
      if [ -r "$f" ]; then set -a; . "$f"; set +a; fi
    done
    unset f
  '';
  programs.git = {
    enable = true;
    # agent-vault swaps in the real token; git only needs a placeholder.
    config.credential."https://github.com".helper =
      "!f() { echo username=x-access-token; echo password=__placeholder__; }; f";
  };
  # With nix-direnv.
  programs.direnv.enable = true;

  # On top of NixOS's core packages (coreutils, curl, …), nix and docker.
  environment.systemPackages = with pkgs; [
    file
    unzip
    gh
    uv
    gnumake
    just
    ripgrep
    fd
    jq
    yq-go
    ast-grep
  ];

  system.stateVersion = "26.05";
}
