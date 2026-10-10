#!/bin/sh
# A stand-in for the msb CLI. VM state lives in files under $HANGAR_FAKE;
# every call is logged (argv only: secrets must never show up there). A file
# `$HANGAR_FAKE/fail-<subcommand or bay script>` fails it with its text.
fake=${HANGAR_FAKE:?}
# One line per call, even when an argument (a script) spans lines.
printf '%s\n' "$*" | tr '\n' ' ' >>"$fake/msb.log"
echo >>"$fake/msb.log"
fails() { [ ! -f "$fake/fail-$1" ] || { cat "$fake/fail-$1" >&2; exit 1; }; }
fails "$1"

case $1 in
inspect)
  [ -f "$fake/vm-$2" ] || { echo "error: sandbox not found: $2" >&2; exit 1; }
  printf '{"status":"%s"}\n' "$(cat "$fake/vm-$2")"
  exit 0 ;;
create)
  # A new bay has a fresh disk: no profile, no package record. Its home
  # may be a host mount, which outlives it.
  case $4 in
  hangar-bay-*) rm -rf "$fake/profile" "$fake/hangar-packages-save" ;;
  esac
  echo Running >"$fake/vm-$4"
  exit 0 ;;
start) echo Running >"$fake/vm-$3"; exit 0 ;;
stop) echo Stopped >"$fake/vm-$3"; exit 0 ;;
rm) rm -f "$fake/vm-$4"; exit 0 ;;
load) exit 0 ;;
exec)
  # exec <tty> [--user U] [--workdir W] VM -- COMMAND…
  until [ "${1--}" = -- ]; do
    [ "$1" != --user ] || user=$2
    vm=$1
    shift
  done
  shift ;;
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

# exec: what runs inside the VM. Scripts are told apart by their $0; one
# not named below keeps its stdin in `$HANGAR_FAKE/<$0>`.
case "$1 $2" in
"sh -c")
  fails "$4"
  case $4 in
  hangar-booted) echo running ;;
  hangar-install)
    # . <proxy env> && exec nix profile add … <installable>
    for installable; do :; done
    mkdir -p "$fake/profile"
    touch "$fake/profile/${installable##*#}" ;;
  hangar-packages) cat "$fake/hangar-packages-save" 2>/dev/null || echo '{}' ;;
  hangar-profile)
    elements=$(find "$fake/profile" -type f 2>/dev/null |
      sed 's|.*/\(.*\)|"\1":{}|' | paste -sd, -)
    printf '{"elements":{%s}}\n' "$elements" ;;
  hangar-file)
    mkdir -p "$fake/vmfs$(dirname "$5")"
    cat >"$fake/vmfs$5"
    chmod "$6" "$fake/vmfs$5" ;;
  hangar-run)
    # Pilot's user units: hangar-run <unit> <command> <description> <mode>.
    # A file per active unit holds its description.
    if [ "$8" = restart ]; then
      echo "$5" >>"$fake/restarts"
    elif [ -f "$fake/$5" ]; then
      cat "$fake/$5"; exit 0
    fi
    echo "$7" >"$fake/$5"
    echo started ;;
  hangar-run-status) if [ -f "$fake/$5" ]; then echo active; fi ;;
  hangar-setup-check) cat "$fake/setup-check" 2>/dev/null || echo needed ;;
  *) cat >"$fake/$4" ;;
  esac ;;
"nix profile")
  # nix profile remove --profile <profile> <element>…
  shift 5
  for element in "$@"; do rm -f "$fake/profile/$element"; done ;;
"sh -l" | "sh -lc") echo "shell: $*" ;;
"rm -f")
  shift 3
  for path in "$@"; do rm -f "$fake/vmfs$path"; done ;;
*) echo "unexpected exec: $*" >&2; exit 2 ;;
esac
