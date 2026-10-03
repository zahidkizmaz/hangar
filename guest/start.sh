# shellcheck shell=bash
# Run as root by `hangar up` on every boot: only what is unknown until then.
# The proxy URL comes as one stdin line: it carries the bay's token, which
# must never sit in argv or in /run/hangar (the host mount: ca.pem only).

IFS= read -r proxy || true
if [ -z "$proxy" ]; then
  echo "hangar-start: expected the proxy URL on stdin" >&2
  exit 1
fi
bundle=/var/lib/hangar/ca-bundle.crt
env_file=/etc/hangar/proxy.env
# Outlives a failed restart, so the next `up` still restarts.
pending=/var/lib/hangar/restart-pending

# The daemons read both only when they start: restart them on a change.
replace() {
  if cmp -s "$1.tmp" "$1"; then
    rm -f "$1.tmp"
  else
    touch "$pending"
    mv "$1.tmp" "$1"
  fi
}

trust_vault_ca() {
  cat "$CACERT_BUNDLE" /run/hangar/ca.pem >"$bundle.tmp"
  chmod 0644 "$bundle.tmp"
  replace "$bundle"
}

quote() {
  printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

# The daemons' EnvironmentFile, so KEY='value' lines only: systemd rejects
# `export`. Readable by pilot, whose apps need the proxy.
write_proxy_env() {
  quoted=$(quote "$proxy")
  (
    umask 077
    {
      for name in HTTPS_PROXY HTTP_PROXY https_proxy http_proxy; do
        echo "$name=$quoted"
      done
      for name in NO_PROXY no_proxy; do
        echo "$name=localhost,127.0.0.1"
      done
      for name in SSL_CERT_FILE NIX_SSL_CERT_FILE CURL_CA_BUNDLE \
        NODE_EXTRA_CA_CERTS GIT_SSL_CAINFO; do
        echo "$name=$bundle"
      done
    } >"$env_file.tmp"
  )
  chgrp pilot "$env_file.tmp"
  chmod 0640 "$env_file.tmp"
  replace "$env_file"
}

pilot_units() {
  systemctl --user --machine=pilot@.host "$@"
}

start_daemons() {
  if [ -e "$pending" ]; then
    systemctl try-restart nix-daemon.service
    pilot_units restart docker.service
    rm -f "$pending"
  else
    pilot_units start docker.service
  fi
}

# Ready means it answers: the unit is active before its socket is.
wait_for_docker() {
  for _ in $(seq 1 60); do
    setpriv --reuid=pilot --regid=pilot --init-groups \
      env DOCKER_HOST=unix:///run/user/1000/docker.sock \
      docker info >/dev/null 2>&1 && return 0
    sleep 1
  done
  echo "hangar-start: dockerd did not start, see" \
    "'journalctl --user -M pilot@ -u docker'" >&2
  exit 1
}

trust_vault_ca
write_proxy_env
start_daemons
wait_for_docker
echo "bay ready"
