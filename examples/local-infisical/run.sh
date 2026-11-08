#!/bin/sh
# Starts Infisical; makes its keys on the first run, inside the bay.
set -eu
cd ~/infisical
if [ ! -f .env ]; then
  umask 077
  hex() { od -An -N"$1" -tx1 /dev/urandom | tr -d ' \n'; }
  password=$(hex 16)
  cat >.env <<ENV
ENCRYPTION_KEY=$(hex 16)
AUTH_SECRET=$(head -c 32 /dev/urandom | base64)
POSTGRES_USER=infisical
POSTGRES_DB=infisical
POSTGRES_PASSWORD=$password
DB_CONNECTION_URI=postgres://infisical:$password@db:5432/infisical
REDIS_URL=redis://redis:6379
SITE_URL=http://localhost:8080
ENV
fi
exec docker-compose -f compose.yml up
