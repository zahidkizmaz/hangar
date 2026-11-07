# shellcheck shell=sh
# Run with a "#!/bin/sh" and "missing=<exit code>" line before it, the code
# the real tool exits with when an item is not there (tests/fakes/mod.rs).
# shellcheck disable=SC2154
# A fake macOS `security` / Linux `secret-tool`: each item is a file in
# $HANGAR_FAKE/keychain, named by its account with '/' as '%'.
dir=$HANGAR_FAKE/keychain
mkdir -p "$dir"
item() { printf '%s/%s' "$dir" "$(printf '%s' "$1" | tr / %)"; }
case $1 in
-i)
  read -r _cmd _u _s _service _a account _w token
  printf '%s\n' "$token" >"$(item "$account")"
  ;;
store) cat >"$(item "$6")" ;;
find-generic-password | lookup)
  file=$(item "$5")
  [ -f "$file" ] || exit "$missing"
  cat "$file"
  ;;
delete-generic-password | clear) rm -f "$(item "$5")" ;;
*) echo "fake keychain: unknown $*" >&2; exit 2 ;;
esac
