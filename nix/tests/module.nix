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
    tower.credentialFiles.GITHUB_TOKEN = "/run/secrets/github-token";
    appDefinitions.web.ports = [
      {
        name = "web";
        vm = 3100;
        purpose = "Example web UI";
      }
    ];
    bays = [
      {
        name = "default";
        apps = [
          "nix"
          "github-token"
          "claude-code"
          "web"
        ];
        image = "ghcr.io/example/hangar-bay:latest";
        packages = [ "github:numtide/llm-agents.nix#codex" ];
        env.EXAMPLE_MODE = "demo";
        run.web = "serve --port 3100";
        files."~/.config/app/config.toml" = "~/dotfiles/app/config.toml";
        mounts."~/.app-data" = {
          host = "~/work/app-data";
          writable = true;
        };
        cache = false;
      }
      {
        name = "oss";
        home = false;
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
