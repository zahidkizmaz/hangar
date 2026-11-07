# Local Infisical

A self-hosted Infisical with Agent Vault, all on this machine:

- `infisical`, a bay with the open internet that runs Infisical, Postgres
  and Redis with Docker Compose, on `http://127.0.0.1:8080`;
- `vault`, a managed `infisical` proxy that enrols with it;
- `work`, a bay whose only way out is `vault`.

Each image is pinned by digest. Infisical's own keys are made inside the
`infisical` bay on its first start and stay in its home.

```sh
mkdir -p ~/.config/hangar/infisical
cp compose.yml run.sh ~/.config/hangar/infisical/
cp hangar.json ~/.config/hangar/hangar.json   # or merge it into yours
hangar up
```

`up` starts `infisical` first. When it asks for `vault`'s proxy token,
open `http://127.0.0.1:8080`, create the admin account, an Agent Vault
project and a proxy, and paste its token. It asks for the session token
before `work`. Set the proxy's Traffic Policy to "Access bundle hosts
only", or it forwards any host
([proxy-kinds.md](../../docs/proxy-kinds.md#infisical-setup)).

Infisical's database is on the `infisical` bay's data volume: changing
that bay's `disk` replaces it. Infisical's audit logs need a license,
even self-hosted.
