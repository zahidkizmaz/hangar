# The shared module renders the same hangar.json as tests/module.json, and
# writes no `bays` when none are set (the CLI's default).
{
  lib,
  pkgs,
  runCommand,
  jq,
}:
let
  render =
    hangar:
    (lib.evalModules {
      specialArgs = { inherit pkgs; };
      modules = [
        ../module.nix
        { services.hangar = hangar; }
      ];
    }).config.services.hangar.package.config;

  settings = {
    proxies.vault = {
      kind = "infisical";
      run = "managed";
      proxyTokenFile = "/run/secrets/infisical-proxy-token";
      sessionTokenFile = "/run/secrets/infisical-session-token";
    };
    defaults.proxy = "vault";
    apps.web.ports = [
      {
        name = "web";
        vm = 3100;
        purpose = "Example web UI";
      }
    ];
    sandbox.backend = "msb";
    bays = [
      {
        name = "default";
        apps = [
          "claude-code"
          "web"
        ];
        image = "ghcr.io/example/hangar-bay:latest";
        packages = [ "github:numtide/llm-agents.nix#codex" ];
        env.EXAMPLE_MODE = "demo";
        ports.web = 3200;
        cpus = 2;
        memory = "8G";
        mounts = {
          "~/notes".host = "~/notes";
          "~/projects" = {
            host = "~/work/projects";
            mode = "rw";
          };
          "~/.config/app" = {
            host = "~/dotfiles/app";
            mode = "copy";
          };
        };
      }
      {
        name = "oss";
        proxy = null;
        disk = "60G";
        # A stub: the test checks the rendering, not the image.
        imagePackage = {
          type = "derivation";
          outPath = "/nix/store/00000000000000000000000000000000-bay-image";
          imageName = "example/bay";
          imageTag = "dev";
        };
      }
    ];
  };
in
runCommand "hangar-module-check" { nativeBuildInputs = [ jq ]; } ''
  diff <(jq -S . ${render settings}) <(jq -S . ${../../tests/module.json})
  jq -e 'has("bays") | not' ${render (settings // { bays = [ ]; })}
  touch $out
''
