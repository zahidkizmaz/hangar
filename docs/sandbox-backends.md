# Sandbox backends

hangar talks to its sandbox only through the `Sandbox` trait in
`src/sandbox/mod.rs`: `describe`/`state`, `create(&VmSpec)`, `start`,
`stop`, `remove`, `exec`, `shell`, `image_present`, `load_image` and
`host_address`. `VmSpec` says what a VM gets in backend-neutral terms:
`Egress::OnlyHostPort(port)` or `Egress::Open`, published ports, mounts
with a `MountMode` and an optional owner, non-secret env, the root disk
size, folders that need a filesystem of their own (not an overlay) and
an `init` to hand PID 1 to.

Each backend is one file, `src/sandbox/<backend>.rs`, and only that file
knows its CLI, flags, status strings, host alias and traps. Today there is
one, msb (microsandbox, `src/sandbox/msb.rs`), selected by
`"sandbox": {"backend": "msb"}` (the default); `Hangar::load` holds the
only `match` on the value, and an unknown one fails at load. The msb-only
Nix bits (`msbPackage`, `nix/microsandbox.nix`) stay msb-specific.

The core doesn't trust a backend with the security-critical parts: it
records what it created each bay with (`bays/<name>/vm`) and the tower
with (`tower-vm`), and refuses a VM whose egress it can't verify (see
`architecture.md`).

## Contract

hangar's security model depends on every backend providing all of these:

1. Host-side, non-bypassable egress control: nothing inside the VM, root
   or containers included, reaches anything but the allowed port.
   Ingress rules for published ports are fine; extra egress never is.
2. Allowing exactly one host port while blocking the rest of the host.
3. Publishing ports on the host's `127.0.0.1` only.
4. Read-only file mounts and persistent disks across restarts.
5. Running commands with stdin (for secrets) and with a TTY (for shells),
   as a named guest user (`exec` takes `user`, `None` keeping the image's
   default; `shell` always names one and its workdir). Bays always name
   one; the tower doesn't.
6. Nested mounts apply enclosing ones first (`home` relies on it).
7. Every child process starts via `process::command`, so the master
   password env var is stripped.
8. `VmSpec.env` and `exec` argv are visible on the host: non-secret only.
   A failed `exec`'s error carries the command's stderr.

The bay image (not the backend) is a NixOS system
(`nix/bay/configuration.nix`): systemd at `/sbin/init`, `nix`,
`systemctl` and `hangar-start` in `/run/current-system/sw/bin`, and the
user `pilot` (uid 1000, gid 1000, home `/home/pilot`). The backend hands
PID 1 to `VmSpec.init` once its own setup is done, on a `/run` tmpfs of
its own, so its mounts under `/run` stay visible; it never restores a
snapshot (msb's lose `--init`). It runs `exec` as `pilot` by name and
presents the bay's home and user mounts as owned by it (`Mount.owner`).

A firewall inside the VM doesn't satisfy 1: whoever can run docker there
is effectively root.

## Adding one

One file under `src/sandbox/` implementing `Sandbox`, one `Hangar::load`
arm, its defaults, a real-wire fake (like `tests/fakes/msb.sh`) and its
docs. Orchestration tests use the in-process `FakeSandbox`
(`src/sandbox/fake.rs`) and don't change.
