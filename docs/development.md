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
gitleaks. Without Nix, install those yourself. Run `prek install` once to get the hooks on
every commit.

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
against `tests/module.json`). fmt, clippy and shellcheck run only through
prek, with the pinned toolchain, so one Rust version gates the code. Add
`--all-systems --no-build` to evaluate Linux on macOS.

CI fails when line coverage drops below the floor in `ci.yml` (measured on
macOS). Raise the floor as tests are added; never lower it.

`tests/cli.rs` drives the real binary against two fakes in `tests/fakes/`:
`msb.sh` stands in for msb (VM state in files, every call logged, a
`fail-<step>` file triggers a failure) and `FakeVault` is a loopback HTTP
server recording admin API calls. Tests assert on argv, requests and files,
so they also prove secrets stay off the command line. A real `msb` on PATH
is hidden from them.

No test ever calls the real keychain. Production calls it by absolute path,
so a script on PATH can't stand in for it. `src/keychain.rs`'s unit tests
give `OsKeychain` a fake tool script and cover every path. The CLI tests
get theirs through `HANGAR_TEST_KEYCHAIN_TOOL`, which only debug builds
read (`#[cfg(debug_assertions)]`): it never has the item, takes any store
and logs each call. Tests that set `HANGAR_MASTER_PASSWORD` never reach it.
A release build can't be redirected: the lookup is compiled out, and
`nix/cli.nix` (its `postInstall`) and the release workflow fail if the
variable's name shows up in the binary. So the CLI tests need a debug
build: `tests/fakes` doesn't compile without `debug_assertions`, and the Nix
package runs them with `checkType = "debug"`.

No test reaches the network either: unit tests give `src/oauth.rs` a
fake `Https`, and `src/http.rs` is tested against loopback servers.

## End-to-end tests

They need msb and must never touch real state:

```sh
export HANGAR_STATE_DIR=$(mktemp -d)
export HANGAR_KEYCHAIN_SERVICE=hangar-e2e-$$
export HANGAR_CONFIG=$HANGAR_STATE_DIR.json # a scratch hangar.json
hangar up && hangar status
hangar destroy --state --yes # also removes the keychain item
```

Never create or read the real `hangar` keychain item. The bay steps
(VM `hangar-bay-default`; the tower is `hangar-tower`) need the bay
image, published or through `imagePackage`/`imageLoader`: a bay boots its
systemd. Use dummy credential values only.

## Logging

Diagnostics go through the `log` facade to a small stderr logger
(`src/logger.rs`). Levels: `-q` errors only, default info (`==>` progress),
`-v` debug (every child command's argv), `-vv` trace (each vault request's
method and path). `HANGAR_LOG=debug` sets the level when flags can't, e.g.
for the launchd/systemd autostart. Never log headers, bodies or values:
argv and paths are safe only because secrets travel on stdin and in
bodies (`no_secret_reaches_any_log_level` checks this).

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
  is 14 days old. That needs the Renovate GitHub app and a required CI
  status check on `main`.
