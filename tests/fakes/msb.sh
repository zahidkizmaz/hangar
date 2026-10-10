#!/bin/sh
# A stand-in for the msb CLI. VM state lives in files under $HANGAR_FAKE;
# every call is logged (argv only: secrets must never show up there). A file
# named after a failure (e.g. `$HANGAR_FAKE/fail-setup`) triggers it.
fake=${HANGAR_FAKE:?}
# One line per call, even when an argument (a script) spans lines.
printf '%s\n' "$*" | tr '\n' ' ' >>"$fake/msb.log"
echo >>"$fake/msb.log"
fails() { [ -f "$fake/fail-$1" ]; }

case $1 in
inspect)
  fails inspect && { echo "error: permission denied" >&2; exit 1; }
  [ -f "$fake/vm-$2" ] || { echo "error: sandbox not found: $2" >&2; exit 1; }
  printf '{"status":"%s"}\n' "$(cat "$fake/vm-$2")"
  exit 0 ;;
create)
  # A new bay has a fresh disk: no profile, no package record. Its home
  # may be a host mount, which outlives it.
  case $4 in hangar-bay-*) rm -rf "$fake/profile" "$fake/tracked" ;; esac
  echo Running >"$fake/vm-$4"
  exit 0 ;;
start) echo Running >"$fake/vm-$3"; exit 0 ;;
stop) echo Stopped >"$fake/vm-$3"; exit 0 ;;
rm)
  fails rm && { echo "rm refused" >&2; exit 1; }
  rm -f "$fake/vm-$4"; exit 0 ;;
image) [ -f "$fake/image" ]; exit ;;
load) cat >"$fake/image"; exit 0 ;;
exec)
  shift 2
  user=
  while :; do
    case $1 in
    --user) user=$2; shift 2 ;;
    --workdir) shift 2 ;;
    *) break ;;
    esac
  done
  vm=$1
  shift 2 ;;
*) echo "unexpected msb call: $*" >&2; exit 2 ;;
esac

# Every bay exec names its user: the apps' commands run as pilot,
# hangar's own steps (files, packages) as root. Root never looks a
# program up on PATH or starts a login shell: pilot can sway both.
want=root
case "$1 $2" in
"sh -c")
  case $4 in
  hangar-run | hangar-run-status | hangar-setup-check | hangar-file | hangar-docker)
    want=pilot ;;
  esac ;;
"rm -f" | "sh -l" | "sh -lc") want=pilot ;;
esac
case $vm in hangar-bay-*)
  [ "$user" = "$want" ] ||
    { echo "fake msb: ran as '$user', not $want: $*" >&2; exit 3; }
  if [ "$user" = root ]; then
    case $1 in /*) ;; *) echo "fake msb: root ran $1 from PATH" >&2; exit 3 ;; esac
    case $2 in -l*) echo "fake msb: root in a login shell" >&2; exit 3 ;; esac
  fi ;;
esac
# Root's programs by name, past the check above.
prog=${1##*/}
shift
set -- "$prog" "$@"

# exec: what runs inside the VM.
case "$1 $2" in
"sh -c")
  if [ "$vm" = hangar-tower ]; then
    fails server && { echo "server did not start" >&2; exit 1; }
    IFS= read -r password
    printf '%s' "$password" >"$fake/vault-password"
    fails start || touch "$fake/vault-started"
    exit 0
  fi
  # Bay scripts, told apart by their $0.
  case $4 in
  hangar-env) cat >"$fake/bay-env" ;;
  hangar-ca) touch "$fake/ca-bundle" ;;
  hangar-proxy-env)
    # Replaced only when it changed: each replace restarts the daemons.
    fails proxy-env && { echo "read-only file system" >&2; exit 1; }
    cat >"$fake/proxy-env.new"
    if cmp -s "$fake/proxy-env.new" "$fake/proxy-env"; then
      rm "$fake/proxy-env.new"
    else
      mv "$fake/proxy-env.new" "$fake/proxy-env"
      echo restart >>"$fake/daemon-restarts"
    fi ;;
  hangar-docker) ;;
  hangar-booted) echo running ;;
  hangar-install)
    # . <proxy env> && exec nix profile add … --profile <p>
    # [--extra-substituters <cache>] <installable>
    for installable; do :; done
    [ "$6" = --extra-substituters ] && echo "$7" >>"$fake/substituters"
    mkdir -p "$fake/profile"
    touch "$fake/profile/${installable##*#}" ;;
  hangar-packages) cat "$fake/tracked" 2>/dev/null || echo '{}' ;;
  hangar-packages-save) cat >"$fake/tracked" ;;
  hangar-profile)
    mkdir -p "$fake/profile"
    printf '{"elements":{'
    sep=
    for element in "$fake/profile"/*; do
      [ -e "$element" ] || continue
      printf '%s"%s":{}' "$sep" "${element##*/}"
      sep=,
    done
    printf '}}\n' ;;
  hangar-cache)
    # nix copy --to <cache> <profile>
    fails cache && { echo "error: no space left on device" >&2; exit 1; }
    echo "$5 $6" >>"$fake/cache-fills" ;;
  hangar-file)
    fails file && { echo "no space left" >&2; exit 1; }
    mkdir -p "$fake/vmfs$(dirname "$5")"
    cat >"$fake/vmfs$5"
    chmod "$6" "$fake/vmfs$5" ;;
  hangar-run)
    # Pilot's user units: hangar-run <unit> <command> <description> <mode>.
    # A file per active unit, named after its entry.
    entry=${5#hangar-run-} && entry=${entry%.service}
    unit=$fake/run-$entry
    if [ "$8" = restart ]; then
      echo "$entry" >>"$fake/restarts"
    elif [ -f "$unit" ]; then
      cat "$unit.description"; exit 0
    fi
    printf '%s' "$6" >"$unit"
    printf '%s\n' "$7" >"$unit.description"
    echo started ;;
  hangar-setup-check)
    # Only `test -f PATH` checks, against the files the fake VM has.
    path=$(printf '%s' "${5#test -f }" | sed 's|^~|/home/pilot|')
    if [ -f "$fake/vmfs$path" ]; then echo "done"; else echo needed; fi ;;
  hangar-run-status)
    fails run-status && { echo "Failed to connect to bus" >&2; exit 1; }
    entry=${5#hangar-run-} && entry=${entry%.service}
    if [ -f "$fake/run-$entry" ]; then echo active; else echo inactive; fi ;;
  *) echo "unexpected bay script: $4" >&2; exit 2 ;;
  esac ;;
"nix profile")
  # nix profile remove --profile <profile> <element>…
  shift 5
  for element in "$@"; do rm -f "$fake/profile/$element"; done ;;
"sh -lc")
  shift 4
  if [ "$1 $2" = "sh -lc" ]; then
    # hangar setup: a `touch PATH` setup command creates PATH.
    fails setup && { echo "onboarding aborted" >&2; exit 1; }
    case $3 in touch\ *)
      path=$(printf '%s' "${3#touch }" | sed 's|^~|/home/pilot|')
      mkdir -p "$fake/vmfs$(dirname "$path")"
      touch "$fake/vmfs$path" ;;
    esac
    echo "setup: $3"
  else
    fails shell && { echo "No journal files were opened" >&2; exit 1; }
    echo "shell: $*"
  fi ;;
"sh -l") echo "interactive shell" ;;
"rm -f")
  shift 3
  for path in "$@"; do rm -f "$fake/vmfs$path"; done ;;
*) echo "unexpected exec: $*" >&2; exit 2 ;;
esac
