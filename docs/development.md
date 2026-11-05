# Development

## Environment

Rust is handled by Cargo, the same with or without Nix:

- `rust-toolchain.toml` pins the toolchain version and components; rustup
  installs them on the first `cargo` command, and Renovate bumps the
  version.
- `Cargo.toml` `[package.metadata.bin]` pins cargo-nextest, cargo-llvm-cov
  and cargo-deny. With cargo-run-bin and cargo-binstall installed (`cargo
  install cargo-run-bin cargo-binstall`), `cargo nextest`, `cargo llvm-cov`
  and `cargo deny` run those versions, downloaded prebuilt, via the aliases
  in `.cargo/config.toml` (regenerate with `cargo bin --sync-aliases`).

`nix develop` adds rustup, cargo-run-bin, cargo-binstall and the system
tools: prek, nixfmt, yamlfmt, yamllint, shellcheck, actionlint, zizmor and
gitleaks. Without Nix, install those yourself. Run `prek install` once to
get the hooks on every commit.

## Checks

Iterate with targeted tests (`cargo nextest run <name>`). Before calling
work done:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo nextest run
cargo llvm-cov nextest --summary-only
cargo deny check
prek run --all-files
nix flake check
```

`nix flake check` builds the package and runs its tests (via nextest) and
the module check (`nix/tests/module.nix`: the rendered `hangar.json`
against `tests/module.json`, which `src/config.rs` also loads as a valid
config). fmt, clippy and shellcheck run only through prek, with the
pinned toolchain, so one Rust version gates the code. Add `--all-systems
--no-build` to evaluate Linux on macOS.

CI fails when line coverage drops below the floor in `ci.yml` (measured on
macOS). Raise the floor as tests are added; never lower it.

## Fakes

No test touches a real VM, the network, `$HOME` or `~/.config`:

- `sandbox::fake::FakeSandbox`: an in-process sandbox that records every
  call, for the orchestration tests.
- `proxy::fake::FakeProxy`: a proxy kind (`"kind": "fake"`, registered
  only under `cfg(test)`) whose every answer is a field: egress, CA,
  login, health, and a managed VM when wanted.
- `tests/fakes/msb.sh`: stands in for msb in `tests/cli.rs`, which drives
  the real binary. VM state lives in files, every call is logged, and a
  `fail-<step>` file triggers a failure. Tests assert on argv and files,
  so they also prove secrets stay off the command line. A real `msb` on
  `PATH` is hidden from them.
- `testing::serve_each`: a loopback HTTP server answering one request per
  canned response, for CA downloads and health probes.
- XDG variables point at a scratch dir in every test.

## End-to-end tests

They need msb and must never touch real state or the user's VMs:

```sh
export XDG_DATA_HOME=$(mktemp -d) XDG_CACHE_HOME=$(mktemp -d) \
  XDG_STATE_HOME=$(mktemp -d)
export HANGAR_CONFIG=$XDG_DATA_HOME/hangar.json # a scratch config
hangar up && hangar ps
hangar rm NAME --data --yes # for each bay and proxy
```

Use bay and proxy names no real config uses: VM names are
`hangar-bay-<name>` and `hangar-proxy-<name>`. A bay boots its systemd, so
it needs the bay image, published or through `imageLoader`. Use test
tokens only.

## Logging

Diagnostics go through the `log` facade (`src/logger.rs`) to stderr and
to `$XDG_STATE_HOME/hangar/hangar.log`. Levels: `-q` errors only, default
info (`==>` progress), `-v` debug (every child command's argv), `-vv`
trace. The file keeps info and above even under `-q`. Only hangar's own
targets are logged; ureq's and rustls' lines are dropped, since they may
name a request's headers. Never log headers, bodies or values: argv and
paths are safe only because secrets travel on stdin.

## Security checks

| Tool               | Checks                                                 | Where                                                |
| ------------------ | ------------------------------------------------------ | ---------------------------------------------------- |
| gitleaks           | committed secrets                                      | prek (staged), CI (full history)                     |
| zizmor, actionlint | workflow security and correctness                      | prek                                                 |
| cargo-deny         | licenses, banned/duplicate crates, sources; advisories | prek (no advisories), CI, weekly `audit.yml`         |
| sbomnix            | SBOM of the bay image                                  | `image.yml` artifact                                 |

The image's SBOM lists every package in its Nix closure, the input for a
CVE scan. trivy and grype don't see Nix packages; vulnix does, but it
downloads the whole NVD feed on every run, so it's left out for now.

## CI, Renovate, releases

- `ci.yml`: `nix flake check`, `prek run --all-files`, `cargo deny check`
  and a full-history gitleaks scan, on Linux and macOS.
- `audit.yml`: weekly RustSec advisory check.
- `image.yml`: on every push to `main` or a `v*` tag, builds the bay
  image on native amd64 and arm64 runners, writes its SBOM, and pushes a
  multi-arch image to `ghcr.io/zahidkizmaz/hangar-bay`; the run summary
  prints the digest. A `v*` tag's image is the one that hangar version
  uses by default.
- `release.yml`: on `v*` tags, builds the binaries (aarch64-darwin,
  x86_64-linux, aarch64-linux) in a matrix and attaches them with
  `SHA256SUMS` to the GitHub release from one release job. Release builds
  use the runner's Rust, because a Nix-built Linux binary links Nix's
  glibc.
- Renovate (`renovate.json5`): Cargo, GitHub Actions and flake inputs, with
  lock file maintenance; minor and patch updates automerge once a release
  is 14 days old. Renovate also bumps the pinned Infisical image in
  `src/proxy/infisical.rs`. That needs the Renovate GitHub app and a
  required CI status check on `main`.
