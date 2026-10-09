# Design: OAuth credentials in the tower

Status: implemented (the four steps below), after review round 1. Main
use: remote MCP servers. The Atlassian spike runs after merge.

Adding an MCP server to a bay takes three manual steps today: a
credential in the vault, a route in `tower.routes`, and the server in each
tool's own config. The step that has no good answer is the first one when
the server uses OAuth (Atlassian, for example): the tower can't hold a
login, so the user would have to log in inside the bay and leave a refresh
token there.

This design adds one generic piece: **an OAuth credential kind that the
tower holds and refreshes**, for any route. MCP is the main example.
hangar learns nothing about MCP or about any tool's config format.

## Goals

- `hangar credential login NAME URL` logs a credential in once, from the
  host's browser. The tower keeps the tokens and refreshes them, and a
  route injects the credential like any other.
- No access token, refresh token or client secret is ever in a bay, in
  argv, in a log, or in a file hangar writes.
- It works for any OAuth 2.0 API with Authorization Code + PKCE, not only
  MCP servers.
- Adding an MCP server becomes: one route, the server in the user's own
  tool config (the way they always add it), and one `credential login`.

## Non-goals (v0.1)

- An `mcpServers` concept, or any MCP parsing in hangar.
- Writing or merging any tool's config (`~/.claude.json`, `opencode.json`,
  Codex's `config.toml`, Paperclip). That config is the user's, and the
  existing `files`/`mounts` (or the tool's own CLI in `hangar shell`) put it
  in the bay. It holds no secrets, because the tower injects them.
- A `credential logout` verb: `credential rm` is the logout.
- Pasting tokens (agent-vault's `POST /v1/credentials/oauth/tokens`).
- Confidential clients from automatic registration: registration asks
  for a public client (`none`) or fails with "pass --client-id".
- A `missing` state in `credential list`.
- Revoking grants at the provider. The refresh token never leaves the
  vault, so hangar can't send it to a revocation endpoint.
- Moving logins between machines. Vault state doesn't carry over today
  either (usage.md, "The master password").
- New crates.

## What exists today (grounding)

- **Routes** are `{name, host, auth}` (`src/broker/mod.rs`, `Route`, `Auth`).
  `auth.type` is `bearer`, `basic`, `api-key`, `custom` or `passthrough`,
  and auth names credentials, never values. `up` replaces the whole set
  with `vault service set -f -` (`agent_vault.rs`, `set_routes`). A host may
  carry a path glob (`slack.com/api/*`): agent-vault's matcher
  (`internal/broker/broker.go`, `MatchService`, `matchPathGlob`). In deny
  mode an unmatched path on the same host gets `403`
  (`docs/learn/services.mdx`).
- **Placeholders.** Every credential name a route references becomes
  `NAME='hangar-placeholder'` in the bay's `/etc/hangar/bay.env`
  (`bay.rs`, `vm_env`, `write_env`), and login shells source it.
- **Injected always wins.** agent-vault strips the client's copy of every
  header it injects before forwarding (`internal/brokercore/brokercore.go`,
  `ApplyInjection`). So a placeholder in a tool's `Authorization` header is
  replaced, never forwarded.
- **The user's credentials** go through `hangar credential set|list|rm`
  (`src/credential.rs`) to `POST|GET|DELETE /v1/credentials`
  (`agent_vault.rs`, `Admin`). `up` deletes only names listed in
  `credential-keys` (`broker/mod.rs`, `apply_credentials`, `stale_keys`), so
  credentials the user adds survive.
- **The admin client** (`src/http.rs`) speaks plain HTTP/1.1 to
  `127.0.0.1` only. hangar has no TLS.
- **The OAuth callback reaches the host** since PR #9 (merged as
  `fd9a5ef`): the tower starts agent-vault with
  `AGENT_VAULT_ADDR=http://127.0.0.1:<adminPort>` (`agent_vault.rs`,
  `START_SERVER`, `host_address`). agent-vault builds its redirect URI from
  that address: `s.baseURL + "/v1/oauth/callback"` (`handle_oauth.go`). A
  tower started before PR #9 still has `0.0.0.0` in it.

### agent-vault 0.40.0 OAuth (source at tag `v0.40.0`)

| What | Where |
|---|---|
| `POST /v1/credentials/oauth/connect` `{vault, key, authorization_url, token_url, client_id, client_secret?, scopes?, token_auth_method?}` saves the client and returns `{authorization_url}` with state + PKCE S256 | `internal/server/server.go:834`, `handle_oauth.go` `handleOAuthConnect` |
| A `client_secret` of `••••••••` keeps the stored secret, but only while `token_url` stays the same | `handleOAuthConnect`, `oauthSecretSentinel` |
| `GET /v1/oauth/callback` exchanges the code (no auth: state is the CSRF check, 10 min TTL) and shows the result in the browser (`/oauth/complete`) | `server.go:835`, `handleOAuthCallback`, `oauthStateTTL`, `redirectOAuthComplete` |
| `GET /v1/credentials` lists `type`, `connected_at`, `last_refreshed_at`, `last_refresh_error`, the URLs, client id, scopes and auth method. It masks the client secret, access token and refresh token with `••••••••` | `handle_credentials.go`, `credentialEntry`, `enrichOAuthEntry` |
| Tables `credential_oauth` and `credential_oauth_states`. The OAuth row cascades on credential delete, and foreign keys are on | `internal/store/048_credential_oauth.go`; `sql_store.go:82` `foreign_keys(on)` |
| Refresh happens within 5 minutes of expiry, once per key (singleflight). It keeps a rotated refresh token, or the old one if none comes back | `internal/brokercore/credential.go` `oauthRefreshBuffer`, `maybeRefreshOAuth`; `sql_store.go` `UpdateCredentialOAuthTokens` |
| A token response without `expires_in` stores no expiry, so that token is never refreshed early: it's used until the upstream rejects it | `oauth.go` `doTokenRequest`; `maybeRefreshOAuth` (`TokenExpiresAt == nil`) |
| The token endpoint goes through the SSRF guard (netguard), with no proxy | `server.go:799-803` |
| An OAuth credential with no token yet (`oauth_not_connected`), a failed refresh (`oauth_refresh_failed`) and a missing credential (`credential_not_found`) all answer the bay with `502`. None of them is forwarded upstream, so none can produce the upstream's `401` | `brokercore.go:204-212` |
| A public client is `client_secret_post` with an empty secret, which sends only `client_id` | `internal/oauth/oauth.go`, `applyClientAuth` |
| Query parameters already in `authorization_url` are kept (so `resource=` can ride along). The token request sends no `resource` | `internal/oauth/pkce.go` `BuildAuthorizationURL`; `oauth.go` `Exchange` |
| Re-connecting with the same `token_url` keeps the stored tokens until the new callback succeeds | `sql_store.go`, `SetCredentialOAuth` upsert |
| Every token update sets `last_refreshed_at`. `connected_at` keeps its first value (`COALESCE`) | `UpdateCredentialOAuthTokens` |
| Credential keys must match `^[A-Z][A-Z0-9_]*$`; hangar's `valid_key` now does too | `internal/broker/broker.go:108`, `config.rs` `valid_key` |

### Atlassian (checked on 2026-10-10)

- `https://mcp.atlassian.com/.well-known/oauth-authorization-server`
  advertises:
  - endpoints: `issuer` `https://mcp.atlassian.com`, `/v1/authorize`,
    `/v1/token`, `registration_endpoint` `/v1/register`, and
    `revocation_endpoint` `/v1/token`;
  - `code_challenge_methods_supported` `["plain","S256"]`;
  - grants `authorization_code` and `refresh_token`;
  - `token_endpoint_auth_methods_supported` including `none`.
- RFC 9728 metadata is missing: both
  `/.well-known/oauth-protected-resource/v1/mcp` and
  `/.well-known/oauth-protected-resource` return 404.
- `POST /v1/mcp` without a token returns `401` with
  `WWW-Authenticate: Bearer realm="OAuth", error="invalid_token"` and no
  `resource_metadata`.
- An API-token alternative exists at `/v2/mcp` (Basic `email:token`,
  enabled by an admin), which a plain `basic` route covers today.

## How tools configure a remote MCP server

None of them needs a real secret, because each can send a header built
from an environment variable. That variable is the bay's placeholder, and
the tower replaces the header.

| Tool | Where | Remote server | Auth from env | Own OAuth |
|---|---|---|---|---|
| Claude Code | `~/.claude.json` (user and local scope), `.mcp.json` (project scope); `claude mcp add` | `{"type":"http","url":…}` | `"headers":{"Authorization":"Bearer ${NAME}"}`: `${VAR}` is expanded in `url`, `headers`, `env`, `command`, `args` | on `401`/`403`, except when you set `Authorization` yourself ([docs](https://code.claude.com/docs/en/mcp)) |
| opencode | `opencode.json` `mcp`; `OPENCODE_CONFIG` names a file ([docs](https://opencode.ai/docs/config/)) | `{"type":"remote","url":…}` | `"headers":{"Authorization":"Bearer {env:NAME}"}` | on `401`; `"oauth": false` turns it off. Tokens go to `~/.local/share/opencode/mcp-auth.json` ([docs](https://opencode.ai/docs/mcp-servers/)) |
| Codex | `~/.codex/config.toml` `[mcp_servers.NAME]` | `url = …` | `bearer_token_env_var = "NAME"` or `env_http_headers` ([docs](https://learn.chatgpt.com/docs/extend/mcp?surface=cli)) | not checked |
| Paperclip 2026.722.0 | its Apps area (`/apps/browse`) and `POST /companies/:companyId/tools/mcp/import-json`, behind the instance setting `enableApps` | its own | its own | reported by earlier research, not verified here |

So the rule for users is: **point the tool's auth at the credential's
placeholder, and turn the tool's own OAuth off where it can be turned
off.** A `401` from upstream (say, a revoked grant) then can't start a
login inside the bay. That login would put a refresh token in the kept
home. Path-scoped routes (below) are the second guard: the bay can't reach
the provider's `/register` or `/token`.

## The design

### Config: nothing new

A route references an OAuth credential like any other:

```json
{ "name": "atlassian-mcp", "host": "mcp.atlassian.com/v1/mcp",
  "auth": { "type": "bearer", "token": "ATLASSIAN" } }
```

The login's URLs, client id and scopes live in the vault
(`credential_oauth`), the same place as its tokens. So `hangar.json`
doesn't change, and a re-login reads them back from `GET
/v1/credentials`.

**Path-scoped routes guard only while no other route covers the host.**
A bare `mcp.atlassian.com` route, or a `*.atlassian.com` one, matches
every path, so the path scope no longer keeps the bay off `/v1/register`
and `/v1/token`. hangar warns about such a pair when it loads the config.
Path scoping also makes a server that splits its endpoint across paths (an
SSE server's `/sse` plus `/messages`) need a glob such as `/v1/*`.

### `hangar credential login`

Three forms:

```
hangar credential login NAME URL [--scope S]... [--client-id ID [--client-secret]]
hangar credential login NAME --authorization-url U --token-url U \
                             --client-id ID [--client-secret] [--scope S]...
hangar credential login NAME [--scope S]...
```

- **`NAME URL`**: discover the provider from URL, then register a client
  unless `--client-id` names one.
- **The endpoint flags**: no discovery, for providers without metadata
  (GitHub, for example), with a client the user registered.
- **`NAME` alone**: a re-login with the client the vault already holds
  (URLs, client id and scopes from `GET /v1/credentials`). If that client
  has a secret, hangar sends agent-vault's keep-marker (`••••••••`), which
  the vault honors because `token_url` is unchanged. The first two forms
  replace the stored client.

Steps:

1. **Checks.**
   - The name is checked like `credential set` checks it
     (`check_user_key`), and the vault must be running (`vault_ready`).
   - A static credential of that name is refused with "run 'hangar
     credential rm NAME' first". The connect call would turn the row into
     `oauth` and keep the static value as its access token
     (`SetCredentialOAuth` upserts `credentials` with `type='oauth'`).
   - For the same reason, `credential set` refuses an OAuth credential.
2. **Discovery** (`NAME URL` only). Every URL hangar fetches or hands to
   the vault must be `https` with a host name: IP literals and `localhost`
   (`*.localhost` too) are refused.
   1. RFC 9728 §3.1: try `<origin>/.well-known/oauth-protected-resource<path>`.
      If found, its `resource` must equal URL. Otherwise, if URL has a
      path, try the root form `<origin>/.well-known/oauth-protected-resource`;
      its `resource` must equal the origin. A found document gives the
      issuer (`authorization_servers[0]`) and the `resource` that is added
      to the authorization URL (RFC 8707).
   2. Without one, the issuer is URL's origin (the MCP fallback, and what
      Atlassian needs).
   3. RFC 8414 §3: try `<issuer origin>/.well-known/oauth-authorization-server<issuer path>`,
      then OpenID `<issuer>/.well-known/openid-configuration`. `issuer`
      must equal the expected one (§3.3). If
      `code_challenge_methods_supported` is present and lacks `S256`, that's
      an error.
3. **Client** (`NAME URL` without `--client-id`). Register with RFC 7591
   at `registration_endpoint`:
   `{"client_name":"hangar","redirect_uris":[<callback>],"grant_types":["authorization_code","refresh_token"],"response_types":["code"],"token_endpoint_auth_method":"none"}`.
   - The callback is `Broker::oauth_redirect_uri()`. For agent-vault that's
     `http://127.0.0.1:<adminPort>/v1/oauth/callback`, a loopback redirect
     (RFC 8252 §7.3).
   - These fail with "pass --client-id", naming the callback to register:
     - no `registration_endpoint`;
     - `token_endpoint_auth_methods_supported` without `none`;
     - a response that isn't a public client.
   - Only `client_id` and `token_endpoint_auth_method` are read from the
     response. `registration_access_token` is never parsed or logged.
   - A `--client-secret` is read like `credential set` reads a value
     (hidden prompt or stdin) and goes only into the connect request body.
4. **Connect** (under the lock). `Broker::oauth_connect` saves the client
   and returns the consent URL. hangar checks two things before opening it:
   - the URL starts with `https://`;
   - its `redirect_uri` equals `oauth_redirect_uri()`. Otherwise the tower
     runs an agent-vault started before PR #9, and the error says
     "restart the tower: hangar down && hangar up".

   Then hangar prints the URL to the terminal (stderr, only when it is a
   terminal, never into logs or `--json`). It opens the URL with the same
   opener `vault-ui` tries (`open`, then `xdg-open`). The browser comes back
   to the vault's callback on loopback, and the vault shows the result
   there.
5. **Wait** (no lock: it only reads).
   - Before connecting, read NAME's `last_refreshed_at` (`None` for a new
     credential).
   - After connecting, poll `Broker::credentials()` every second until the
     value differs. There is no clock comparison, so clock skew between
     the host and the tower doesn't matter.
   - `last_refresh_error` is ignored while waiting: an error from an
     earlier login must not end this one.
   - After 10 minutes (the vault's state TTL) the wait fails with "no
     login yet: the browser page shows the result; check 'hangar
     credential list'".
   - Ctrl-C leaves any old tokens in place.

### `credential list`: state

`credential list` reads `Broker::credentials()`:

```
ATLASSIAN                user    oauth: connected
CLAUDE_CODE_OAUTH_TOKEN  user    set
GITHUB_TOKEN             config  set
JIRA                     user    oauth: refresh failed (…)
```

- `set`: a static value. Unknown kinds (agent-vault's `dynamic`) count as
  static.
- `oauth: connected` means a token arrived and the last refresh didn't
  fail.
- `oauth: not connected` means no token yet.
- `oauth: refresh failed (…)` is `last_refresh_error`, cut to one line. The
  vault answers the bay `502 oauth_refresh_failed`, and the fix is
  `credential login NAME`.

The vault doesn't list `token_expires_at`, so "expired" shows only after a
refresh fails. `--json` adds `type` (`static` or `oauth`) and `state` per
entry. `output::VERSION` stays 1 before the first release (AGENTS.md).
`status --all` is unchanged.

### Logout

`hangar credential rm NAME` is the logout. Deleting the credential
cascades to its `credential_oauth` row, and every route that injects it
then fails in the vault (`502 credential_not_found`). The provider-side
grant stays until it expires or the user revokes it in the provider's UI.
hangar never reads the refresh token back.

### Broker trait additions

Provider-side work (discovery, registration) isn't the broker's job and
stays in the core (`src/oauth.rs`). Only what touches stored credentials
goes behind the trait:

```rust
pub(crate) struct OAuthClient {             // all public
    pub(crate) authorization_url: String,
    pub(crate) token_url: String,
    pub(crate) client_id: String,
    pub(crate) scopes: String,              // space-separated
    pub(crate) token_auth_method: String,   // empty: the broker's default
}

pub(crate) enum ClientSecret { None, Keep, New(Secret) }

pub(crate) struct StoredCredential {
    pub(crate) key: String,
    pub(crate) oauth: Option<OAuthLogin>,   // None: static
}

pub(crate) struct OAuthLogin {
    pub(crate) connected: bool,
    pub(crate) refreshed_at: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) client: Option<OAuthClient>,
    pub(crate) has_secret: bool,            // the vault shows only a mask
}

trait Broker {
    // replaces credential_keys()
    fn credentials(&self) -> Result<Vec<StoredCredential>>;
    // both default to "this broker can't hold OAuth credentials"
    fn oauth_redirect_uri(&self) -> Result<String>;
    fn oauth_connect(&self, key: &str, client: &OAuthClient,
                     secret: &ClientSecret) -> Result<String>;
}
```

The agent-vault side is two `Admin` calls, using the existing
`http::call` and session (`agent_vault.rs`): the connect and list
endpoints above. A registered public client is sent as
`client_secret_post` with no secret. The fake broker (`broker/fake.rs`)
records `oauth_connect KEY` and holds states, so command tests need no
vault.

### HTTPS: host `curl`

Discovery and registration are HTTPS calls to the provider, made from the
host. `src/http.rs` is loopback plain HTTP only, and a TLS stack would mean
many new crates. So hangar runs `curl`, by absolute path, like the
keychain tool (`keychain.rs`, `TOOL`).

- **Path.** The Nix package bakes `HANGAR_CURL="${curl}/bin/curl"` on every
  system, macOS included, so the package never depends on the host's
  curl. Other builds use `/usr/bin/curl`, which macOS ships and most
  Linux distributions install. curl is needed only for `credential login
  NAME URL`, and a missing one is an error that names that path.
- **Test override.** Debug builds read `HANGAR_TEST_CURL`. `nix/cli.nix`'s
  `postInstall` and the release workflow fail if that name shows up in a
  release binary, like `HANGAR_TEST_KEYCHAIN_TOOL`.
- **Flags.**
  - `-q` first (no `.curlrc`), then `-sS`, `--proto =https`, `--noproxy '*'`,
    `--max-time 30`, `--max-filesize 1048576`, and `-w` for the status
    code.
  - No `-L`: redirects are never followed, so a `3xx` is just a failed
    lookup.
  - The URL goes in argv (public). The registration body goes on stdin,
    and the response comes back on a pipe.
- **Alternative considered:** busybox `wget` in the tower VM (alpine image,
  open egress). Rejected: busybox `wget` hides 4xx bodies, so registration
  errors would be opaque. It would also run user-chosen URLs in the VM
  that holds every secret.

Every call is logged as method + URL, never a body or a header (AGENTS.md).

## MCP servers: usage (the main example)

```jsonc
// hangar.json: tower.routes
[
  // Atlassian, OAuth. Path-scoped, so the bay can reach /v1/mcp only,
  // never /v1/register or /v1/token.
  { "name": "atlassian-mcp", "host": "mcp.atlassian.com/v1/mcp",
    "auth": { "type": "bearer", "token": "ATLASSIAN" } },

  // Freshdesk with an API key, which Freshdesk sends as the Basic
  // username. A local MCP server in the bay calls this host.
  { "name": "freshdesk", "host": "yourcompany.freshdesk.com",
    "auth": { "type": "basic", "username": "FRESHDESK_API_KEY" } },

  // Context7 as a local (stdio) server: @upstash/context7-mcp 4.3.0 calls
  // https://context7.com/api with "Authorization: Bearer
  // $CONTEXT7_API_KEY" and honors HTTPS_PROXY (dist/lib/constants.js,
  // dist/lib/api.js). npx also needs the `node` app.
  { "name": "context7", "host": "context7.com",
    "auth": { "type": "bearer", "token": "CONTEXT7_API_KEY" } },

  // A public server that needs no auth.
  { "name": "docs-mcp", "host": "mcp.example.com",
    "auth": { "type": "passthrough" } }
]
```

```sh
hangar up
hangar credential login ATLASSIAN https://mcp.atlassian.com/v1/mcp
hangar credential set FRESHDESK_API_KEY
hangar credential set CONTEXT7_API_KEY
```

Then the user adds the servers to their tools as usual, with placeholders
for auth:

```json
{ "mcpServers": {
  "atlassian": { "type": "http", "url": "https://mcp.atlassian.com/v1/mcp",
                 "headers": { "Authorization": "Bearer ${ATLASSIAN}" } },
  "context7":  { "command": "npx", "args": ["-y", "@upstash/context7-mcp"] },
  "docs":      { "type": "http", "url": "https://mcp.example.com/mcp" } } }
```

- Claude Code keeps user-scope servers in `~/.claude.json`, a file it
  rewrites itself. Copying that file with `files` would overwrite Claude
  Code's own state on every `up`. Instead:
  - add servers once with
    `hangar shell claude mcp add-json --scope user NAME '<json>'` (the home
    is kept: configuration.md, "A bay's home");
  - or keep a project `.mcp.json` in the repo.
- opencode's `opencode.json` and Codex's `config.toml` are user-maintained
  files, so `files` copies them as it would any dotfile. The secret scan
  (`secret::find_secret`) still refuses a real token in them.
- `CONTEXT7_API_KEY` reaches the stdio server as the placeholder from
  `bay.env`, and the tower swaps it.

## Paperclip

Nothing in hangar is specific to Paperclip. Its MCP servers are
configured in its own UI or import API, and the same routes and
credentials serve it. Two things are reported but unverified, and the
spike checks them:

- Paperclip's agents (its `claude_local` adapter) may or may not see
  Claude Code's user-scope servers. The adapter is reported to
  `delete settings.mcpServers` in some settings.
- Paperclip's own MCP gateway may need its servers imported, with
  `enableApps` on.

## Security

- **No token in the bay.** Access and refresh tokens and client secrets
  live only in the vault, encrypted. The bay gets placeholders (`vm_env`).
  Injected headers replace the bay's (`ApplyInjection`).
- **No token on the host's disk or argv.**
  - The connect request carries a client secret in an in-process body,
    like `credential set` (AGENTS.md invariant).
  - curl argv holds only public URLs.
  - The consent URL (state, PKCE challenge, client id) is shown on the
    terminal and passed to the opener. The state is a single-use CSRF
    value that expires in 10 minutes, and the code verifier stays in the
    vault.
  - The callback lands on `127.0.0.1` only.
- **Nothing read back.** hangar never calls `reveal=true` and never
  stores the masked values.
- **In-bay OAuth is closed off three ways.**
  - The tool's own OAuth is off, or doesn't fire because an
    `Authorization` header is set.
  - Path-scoped routes deny `/register` and `/token` (`403`, deny mode),
    while no other route covers the host (warned about).
  - Every vault-side failure is a `502`, never a `401`, so the vault
    never invites a client login.

  A `401` from upstream still reaches the tool. That happens with a
  revoked grant, or with a token from a provider that sends no
  `expires_in` and then expires it.
- **Routes still come only from config the user wrote.** `credential
  login` never adds a route. The existing source checks still apply: two
  sources can't route one host, and a `tower.routes` entry can't take an
  app route's name (`apps.rs`, `merge_routes`, `check_routes`).
- **Discovery is checked, not trusted.**
  - On the host: https only, host names only (no IP literals, no
    `localhost`), no redirects, a size cap, and issuer and `resource`
    must match (RFC 8414 §3.3, RFC 9728 §3.3).
  - In the vault: the token endpoint goes through netguard.
- **Registered clients are public.** A client id from registration is not
  a secret (RFC 7591), and a public client (`none`) has no secret to
  leak.
- **Rotation and revocation.**
  - The vault keeps a rotated refresh token (`maybeRefreshOAuth`), with
    one refresh per key at a time.
  - `credential rm` deletes everything in the tower.
  - A revoked grant shows up as `refresh failed` in `credential list`
    once a refresh fails.
- **User credentials only.** OAuth credentials are never recorded in
  `credential-keys`, so `up` never deletes them. Names that
  `credentialFiles` or an app manages are refused.

## Test plan (fakes only)

No test touches the real network, keychain, msb, a `hangar-*` VM or
`~/.config`.

- `src/oauth.rs`, against a fake `Https`:
  - well-known URLs for root and path issuers;
  - parsing: Atlassian's real document as a fixture, an OpenID document,
    PRM path and root forms;
  - refusals: issuer mismatch, a `resource` mismatch, `http`, an IP
    literal, `localhost`, no `S256`;
  - the registration body and response, including the refusals that ask
    for `--client-id`;
  - `resource=` added to the consent URL.
- `src/https.rs`: the curl argv (`-q` first, no `-L`, no body in argv),
  the status and body split, and a curl failure. A fake script stands in,
  the way `keychain.rs` tests its tool.
- `agent_vault.rs`: the list and connect calls against `serve_each`, the
  existing scripted TCP server. Checks: path, method, the body (the
  keep-marker), and the masked list parse.
- `credential.rs` with `FakeBroker`:
  - the wait sees `last_refreshed_at` change, ignores an old error, and
    times out;
  - re-login reuses the stored client and keeps its secret;
  - refusals: static and managed names, a non-https consent URL, a wrong
    `redirect_uri`;
  - list states.
- `tests/cli.rs`, with `FakeVault` learning the connect and list
  endpoints, and a fake curl through `HANGAR_TEST_CURL`:
  - `login` with the endpoint flags, and `login NAME URL` end to end;
  - the vault down gives the hint, `--help` has examples, and `credential
    list --json` has the new fields;
  - no secret shows up at any log level.

## Spike: Atlassian end to end (after merge)

Done by hand on a real machine. Each step records yes or no and keeps any
surprise. Steps 1 to 4 and 7 to 9 gate the feature. Steps 5 and 6 are
informational: they shape the usage docs, not the code.

1. **Login.** `hangar credential login ATLASSIAN
   https://mcp.atlassian.com/v1/mcp`. Check:
   - registration accepts the `http` loopback redirect;
   - the consent page loads;
   - `credential list` shows `oauth: connected`.

   Note whether Atlassian needs `resource` or particular scopes.
2. **Injection.** Add the route above, run `up`, then from a bay:
   - `curl -X POST https://mcp.atlassian.com/v1/mcp` with an MCP
     `initialize` gives `200`;
   - `/v1/register` and `/v1/token` give `403`.
3. **Refresh.** Note `expires_in` and wait past it, then call again.
   Check that `last_refreshed_at` moved and whether the refresh token
   rotated (`credential list` stays `connected`).
4. **Re-login.** Run `credential login ATLASSIAN` alone: the bay keeps
   working throughout, and the wait ends on the new token.
5. **Claude Code** (informational). Add the server with
   `claude mcp add-json --scope user atlassian '{…"Bearer ${ATLASSIAN}"}'`,
   then check `claude mcp list` and a tool call. Also check whether Claude
   Code's HTTP MCP transport honors `HTTPS_PROXY` and
   `NODE_EXTRA_CA_CERTS`.
6. **Paperclip** (informational). Do its agents see the user-scope
   server? If not, does its import or gateway path work through the same
   route? Check the reported `delete settings.mcpServers`.
7. **Removal.** `credential rm ATLASSIAN` gives the bay `502
   credential_not_found`.
8. **Revocation.** Revoke the app at Atlassian. The bay should get
   Atlassian's `401` until the next refresh, and then `credential list`
   should show `refresh failed`.
9. **Old tower.** On a tower started before PR #9, `credential login`
   fails with the restart hint and opens nothing.

## Implementation steps

Each step is one green commit with its docs, and each has a caller.

1. **`feat(credential): show each credential's kind and state`.**
   - `Broker::credentials()` replaces `credential_keys()`.
   - `credential list` shows states.
   - `credential set` refuses an OAuth credential.
   - `valid_key` follows agent-vault's pattern.
   - cli.md and usage.md.
2. **`feat(credential): log OAuth credentials in through the vault`.**
   - `Broker::oauth_redirect_uri` and `oauth_connect`, with
     default-refusing trait methods.
   - `credential login NAME --authorization-url U --token-url U
     --client-id ID [--client-secret] [--scope S]` and `credential login
     NAME`, with the consent URL checks and the wait.
   - usage.md and architecture.md.
3. **`feat(credential): discover providers and register clients`.**
   - `src/https.rs` (curl, `HANGAR_CURL`, `HANGAR_TEST_CURL` and its
     release guard) and `src/oauth.rs` (discovery, registration).
   - `credential login NAME URL`.
   - usage.md's "MCP servers" section, development.md and the file map.
4. **`feat(config): warn when a path-scoped route is shadowed`.** The
   warning about a bare or wildcard route that covers a path-scoped
   route's host, and configuration.md.

## Review log

Round 1 (no blockers, three majors):

- **M1, the wait:** comparing `last_refreshed_at` with the start time
  depends on the tower's clock, and an old `last_refresh_error` could end a
  new login. Now hangar reads the value before connecting, waits for any
  change, and ignores errors while waiting.
- **M2, the secret wipe:** a re-login without the secret would have
  cleared a stored client secret. Now `OAuthClient.secret` has a `Keep`
  case that sends agent-vault's keep-marker.
- **M3, green steps:** the old plan added unused code in early steps. The
  steps are now sliced so each has a caller and its docs.
- **Cuts:** the `logout` alias, paste mode, confidential-client
  registration and the `missing` state.
- **Kept:** the OpenID fallback and `--scope`.
- **Transport:** curl is baked by Nix on every system, with a debug-only
  test override guarded in release builds, `-q` first, and no redirects.
  `registration_access_token` is never read.
- **Minors applied:**
  - the `502` wording, now naming all three cases;
  - the `expires_in` caveat;
  - the root-form `resource` compared with the origin;
  - the path-scope caveat and its warning;
  - host-side SSRF checks (https, no IP literals, no `localhost`);
  - the consent URL on the terminal only, checked to be `https` and to
    carry the vault's `redirect_uri`;
  - `valid_key` tightened to agent-vault's pattern;
  - `token_auth_method` and `scopes` in `OAuthClient`, and unknown kinds
    counted as static;
  - default-refusing trait methods;
  - clearer re-login forms;
  - the spike's informational steps and extra cases.
