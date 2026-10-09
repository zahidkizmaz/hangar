# Design: OAuth credentials in the tower

Status: proposal. Main use: remote MCP servers. Nothing here is built yet.

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
- No secret, access token, refresh token or client secret is ever in a
  bay, in argv, in a log, or in a file hangar writes.
- It works for any OAuth 2.0 API with Authorization Code + PKCE, not only
  MCP servers.
- Adding an MCP server becomes: one route, the server in the user's own
  tool config (the way they always add it), and one `credential login`.

## Non-goals

- An `mcpServers` concept, or any MCP parsing in hangar.
- Writing or merging any tool's config (`~/.claude.json`, `opencode.json`,
  Codex's `config.toml`, Paperclip). That config is the user's, and the
  existing `files`/`mounts` (or the tool's own CLI in `hangar shell`) put it
  in the bay. It holds no secrets, because the tower injects them.
- Revoking grants at the provider. The refresh token never leaves the
  vault, so hangar can't send it to a revocation endpoint (see Logout).
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
  that address: `s.baseURL + "/v1/oauth/callback"` (`handle_oauth.go`).

### agent-vault 0.40.0 OAuth (source at tag `v0.40.0`)

| What | Where |
|---|---|
| `POST /v1/credentials/oauth/connect` `{vault, key, authorization_url, token_url, client_id, client_secret?, scopes?, token_auth_method?}` saves the client and returns `{authorization_url}` with state + PKCE S256 | `internal/server/server.go:834`, `handle_oauth.go` `handleOAuthConnect` |
| `GET /v1/oauth/callback` exchanges the code (no auth: state is the CSRF check, 10 min TTL) | `server.go:835`, `handleOAuthCallback`, `oauthStateTTL` |
| `GET /v1/credentials/oauth/status?key=` returns `{connected, connected_at, last_error}` | `server.go:836`, `handleOAuthStatus` |
| `POST /v1/credentials/oauth/tokens` (paste mode) checks a refresh token by refreshing it at once and rejects it if that fails | `server.go:837`, `handleOAuthTokenUpload` |
| `GET /v1/credentials` lists `type`, `connected_at`, `last_refreshed_at`, `last_refresh_error`, the URLs and client id. It masks the client secret, access token and refresh token with `••••••••` | `handle_credentials.go`, `credentialEntry`, `enrichOAuthEntry` |
| Tables `credential_oauth` and `credential_oauth_states`. The OAuth row cascades on credential delete, and foreign keys are on | `internal/store/048_credential_oauth.go`; `sql_store.go:82` `foreign_keys(on)` |
| Refresh happens within 5 minutes of expiry, once per key (singleflight). It keeps a rotated refresh token, or the old one if none comes back | `internal/brokercore/credential.go` `oauthRefreshBuffer`, `maybeRefreshOAuth`; `sql_store.go` `UpdateCredentialOAuthTokens` |
| The token endpoint goes through the SSRF guard (netguard), with no proxy | `server.go:799-803` |
| "Not connected" and "refresh failed" answer the client with `502`, not `401` | `brokercore.go:204-209` |
| A public client is `client_secret_post` with an empty secret, which sends only `client_id` | `internal/oauth/oauth.go`, `applyClientAuth` |
| Query parameters already in `authorization_url` are kept (so `resource=` can ride along). The token request sends no `resource` | `internal/oauth/pkce.go` `BuildAuthorizationURL`; `oauth.go` `Exchange` |
| Re-connecting with the same `token_url` keeps the stored tokens until the new callback succeeds | `sql_store.go`, `SetCredentialOAuth` upsert |
| A new connect resets `last_refresh_error`. `connected_at` keeps its first value (`COALESCE`), and every token update sets `last_refreshed_at` | `SetCredentialOAuth`, `UpdateCredentialOAuthTokens` |
| Credential keys must be SCREAMING_SNAKE_CASE (hangar's `valid_key` is stricter) | `handleOAuthConnect`, `config.rs` `valid_key` |

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

The login's URLs and client id live in the vault (`credential_oauth`), the
same place as its tokens. So `hangar.json` doesn't change, and a
re-login reads them back from `GET /v1/credentials`. A route field such as
`auth.issuer` was considered and rejected: it would hold one URL that
only `credential login` reads, and that URL is already in the vault.

### `hangar credential login NAME [URL]`

```
hangar credential login NAME URL [--scope S]... [--client-id ID]
                                  [--client-secret] [--paste]
hangar credential login NAME --authorization-url U --token-url U
                             --client-id ID [--client-secret] [--scope S]...
hangar credential login NAME            # again, with what the vault has
```

1. **Checks**, the same as `credential set` (`check_user_key`,
   `vault_ready`). If NAME exists as a static credential, it's refused with
   "run 'hangar credential rm NAME' first". The connect call would turn
   the row into `oauth` and keep the static value as the access token
   (`SetCredentialOAuth` upserts `credentials` with `type='oauth'`). For
   the same reason, `credential set` refuses an OAuth credential.
2. **Discovery** (skipped when both endpoint flags are given, or on a
   re-login):
   1. RFC 9728 §3.1: try `<origin>/.well-known/oauth-protected-resource<path>`,
      then `<origin>/.well-known/oauth-protected-resource`. If found, its
      `resource` must equal URL (§3.3), and the issuer is
      `authorization_servers[0]`. The `resource` is then also added to the
      authorization URL as `resource=` (RFC 8707).
   2. Otherwise the issuer is URL's origin (the MCP fallback, and what
      Atlassian needs).
   3. RFC 8414 §3: try `<issuer origin>/.well-known/oauth-authorization-server<issuer path>`,
      then OpenID `<issuer>/.well-known/openid-configuration`. `issuer`
      must equal the expected one (§3.3). Every endpoint must be `https`.
      If `code_challenge_methods_supported` is present and lacks `S256`,
      that's an error.
3. **Client.**
   - Use `--client-id` if given. A `--client-secret` is read like
     `credential set` reads a value (hidden prompt or stdin) and goes only
     into the connect request body.
   - On a re-login, use the stored client.
   - Otherwise do RFC 7591 registration at `registration_endpoint`:
     `{"client_name":"hangar","redirect_uris":[<callback>],"grant_types":["authorization_code","refresh_token"],"response_types":["code"],"token_endpoint_auth_method":"none"}`,
     or `client_secret_post` when `none` isn't advertised.
   - The callback is `Broker::oauth_redirect_uri()`. For agent-vault that's
     `http://127.0.0.1:<adminPort>/v1/oauth/callback`, a loopback redirect
     (RFC 8252 §7.3).
   - The registration response comes back on a pipe. Any `client_secret`
     in it is treated as a secret.
   - With no `registration_endpoint` and no `--client-id`, the error names
     the redirect URI to register with the provider.
4. **Connect.** `Broker::oauth_connect` saves the client and returns the
   consent URL. hangar opens it with the opener `vault-ui` already tries
   (`commands.rs`: `open`, then `xdg-open`) and prints it too, for a
   headless host. The browser comes back to the vault's callback on
   loopback.
5. **Wait.** Poll `Broker::credentials()` every second, for up to 10
   minutes (agent-vault's state TTL), until NAME's `last_refreshed_at` is
   later than the start. `connected_at` can't be used: it keeps its first
   value on re-login. A `last_refresh_error` stops the wait with that
   message. Ctrl-C leaves any old tokens in place.
6. **Paste mode** (`--paste`) is for providers that refuse a loopback or
   `http` redirect, or have no browser flow. It does discovery for
   `token_endpoint`, then reads a refresh token (hidden prompt or stdin)
   and calls `Broker::oauth_tokens`. agent-vault refreshes it at once and
   rejects a bad one. The refresh token passes through hangar's memory
   once, like a `credential set` value, and never reaches a bay.

Locking: as for `credential set`, the prompts run first and the lock is
held only for the broker calls. The wait doesn't hold the lock: it only
reads.

### `credential list`: state

`credential list` reads `Broker::credentials()` and adds each route's
credential that the vault lacks:

```
NAME                     SOURCE  STATE
ATLASSIAN                user    oauth: connected
CLAUDE_CODE_OAUTH_TOKEN  user    set
GITHUB_TOKEN             config  set
JIRA                     user    oauth: refresh failed (invalid_grant)
FRESHDESK_API_KEY        -       missing (route freshdesk)
```

- `oauth: connected` means a refreshed-at time and no error.
- `oauth: not connected` means no token yet.
- `oauth: refresh failed (…)` is `last_refresh_error`. The vault answers
  the bay `502 oauth_refresh_failed`, and the fix is `credential login
  NAME`.
- `missing` means a route references the name but the vault doesn't have
  it.

The vault doesn't list `token_expires_at`, so "expired" shows only after a
refresh fails. That's enough, because a valid refresh token renews on the
next request. `--json` adds `type` and `state` per entry. `output::VERSION`
stays 1 before the first release (AGENTS.md). `status --all` stays as it
is. It already names each route's credentials, and calling the vault for
state belongs to `credential list`.

### Logout

`hangar credential rm NAME` is the logout. Deleting the credential
cascades to its `credential_oauth` row, and every route that injects it
then fails in the vault. The provider-side grant stays until it expires or
the user revokes it in the provider's UI. hangar never reads the refresh
token back: the list masks it, and reading it with `reveal` would be a new
path that exposes secrets. Whether a `logout` alias is worth adding is an
open question.

### Broker trait additions

Provider-side work (discovery, registration) isn't the broker's job and
stays in the core. Only what touches stored credentials goes behind the
trait:

```rust
pub(crate) struct OAuthClient {
    pub(crate) authorization_url: Option<String>, // None in paste mode
    pub(crate) token_url: String,
    pub(crate) client_id: String,
    pub(crate) client_secret: Option<Secret>,     // never read back
    pub(crate) scopes: Vec<String>,
}

pub(crate) enum CredentialState {
    Static,
    OAuth { refreshed_at: Option<String>, error: Option<String>,
            client: OAuthClient },                // secret: None
}

trait Broker {
    // replaces credential_keys(); keys stay the names
    fn credentials(&self) -> Result<Vec<(String, CredentialState)>>;
    /// Where the provider sends the browser back.
    fn oauth_redirect_uri(&self) -> String;
    /// Saves the client; returns the consent URL to open.
    fn oauth_connect(&self, key: &str, client: &OAuthClient)
        -> Result<String>;
    /// Paste mode; the broker validates the refresh token.
    fn oauth_tokens(&self, key: &str, client: &OAuthClient,
                    refresh_token: &Secret) -> Result<()>;
}
```

The agent-vault side is three `Admin` calls, using the existing
`http::call` and session (`agent_vault.rs`): the connect, tokens and list
endpoints above. `token_auth_method` is `client_secret_post` (empty secret
for a public client) unless the registration said `client_secret_basic`.
The fake broker (`broker/fake.rs`) records `oauth-connect KEY` and
`oauth-tokens KEY` and holds states, so command tests need no vault.

### HTTPS without a new crate

Discovery and registration are HTTPS calls to the provider, made from the
host. `src/http.rs` is loopback plain HTTP only, and a TLS stack would mean
many new crates. Options:

- **Host `curl` by absolute path (chosen).** It follows the keychain-tool
  precedent (`keychain.rs`, `TOOL`; `nix/cli.nix` bakes `HANGAR_SECRET_TOOL`).
  - Path: `/usr/bin/curl` on macOS and other Linux; the Nix package bakes
    `HANGAR_CURL`.
  - Flags: `-q` (no `.curlrc`), `--proto =https --proto-redir =https`,
    `--noproxy '*'`, `--max-time 30`, `--max-filesize 1M`, `-w` for the
    status code.
  - The URL goes in argv (public). The registration body goes on stdin,
    and the response comes back on a pipe.
  - Behind a small `Https` trait in `src/https.rs`, with a fake for tests.
- **busybox `wget` in the tower VM** (alpine image, open egress) would use
  the existing exec path. Rejected: busybox `wget` hides 4xx bodies, so
  registration errors would be opaque. It would also run user-chosen URLs
  in the VM that holds every secret.

Every call is logged as method + host + path, never a body (AGENTS.md).

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
  - The connect and paste requests carry the client secret and refresh
    token in in-process bodies, like `credential set` (AGENTS.md
    invariant).
  - curl argv holds only public URLs.
  - The consent URL (state, PKCE challenge, client id) is public by
    design. The code verifier stays in the vault.
  - The callback lands on `127.0.0.1` only.
- **Nothing read back.** hangar never calls `reveal=true` and never
  stores the masked values.
- **In-bay OAuth is closed off twice.**
  - The tool's own OAuth is off, or doesn't fire because an
    `Authorization` header is set.
  - Path-scoped routes deny `/register` and `/token` (`403`, deny mode).
  - The vault answers `502` (not `401`) for a not-connected or failed
    credential, so it never invites a client login.
- **Routes still come only from config the user wrote.** `credential
  login` never adds a route. The existing source checks still apply: two
  sources can't route one host, and a `tower.routes` entry can't take an
  app route's name (`apps.rs`, `merge_routes`, `check_routes`).
- **Discovery is checked, not trusted.**
  - Every endpoint must be `https`, and issuer and `resource` must match
    (RFC 8414 §3.3, RFC 9728 §3.3).
  - curl doesn't follow redirects to non-https URLs.
  - The token endpoint is reached by the vault through netguard, so
    metadata can't point the vault at a private address.
- **Registered clients are public.** A client id from registration is not
  a secret (RFC 7591), and a public client (`none`) has no secret to
  leak. A confidential registration's secret is handled like a credential
  value.
- **Rotation and revocation.**
  - The vault keeps a rotated refresh token (`maybeRefreshOAuth`), with
    one refresh per key at a time.
  - `credential rm` deletes everything in the tower.
  - A revoked grant shows up as `refresh failed` in `credential list`.
- **User credentials only.** OAuth credentials are never recorded in
  `credential-keys`, so `up` never deletes them. Names that
  `credentialFiles` or an app manages are refused.

## Test plan (fakes only)

- `src/oauth.rs`, pure functions:
  - well-known URL building for root and path issuers;
  - metadata parsing: Atlassian's real document as a fixture, an OpenID
    document, the PRM `resource` and `authorization_servers`;
  - refusals: issuer mismatch, `http` endpoint, no `S256`;
  - the registration body (public vs `client_secret_post`) and response
    parsing, including a returned secret;
  - the consent URL getting `resource=`.
- `src/https.rs`: the curl argv (flags present, no body in argv), the
  status and body split, and a missing curl named in the error, with a
  fake program the way `keychain.rs` tests do.
- `agent_vault.rs`: each new `Admin` call against `serve_each`, the
  existing scripted TCP server. Checks: path, method, a body with no extra
  fields, and parsing of the masked list.
- `credential.rs`, with `FakeBroker` and a fake `Https`:
  - the full login: discovery, then registration, then connect, then the
    wait sees `last_refreshed_at` move;
  - re-login reuses the stored client and skips registration;
  - refusals: static and managed names refused, the no-registration error
    names the redirect URI;
  - paste mode calls `oauth_tokens`;
  - list states, including `missing`.
- `tests/cli.rs`: `credential login` with the vault down gives the hint,
  `--help` has examples, `credential list --json` has the new fields.
- Logs: a test at `trace` that no body or header value appears (the
  `http.rs` rule).

## Spike: Atlassian end to end (before any code)

Done by hand with curl, on main (`fd9a5ef`, PR #9 included). Each step
records yes or no and keeps any surprise:

1. **Registration.** Register a client: `POST https://mcp.atlassian.com/v1/register`
   with redirect `http://127.0.0.1:14321/v1/oauth/callback` and
   `token_endpoint_auth_method: none`. Does it accept an `http` loopback
   redirect?
2. **Connect.** Call `POST /v1/credentials/oauth/connect` with the owner
   session from `vault/.agent-vault/session.json`, open the URL, consent.
   Then `GET /v1/credentials/oauth/status?key=ATLASSIAN` should say
   `connected`.
   - Is `resource` needed (the exchange sends none)?
   - Which scopes, if any, must be passed?
3. **Injection.** Add the route above, run `up`, then from a bay:
   - `curl -X POST https://mcp.atlassian.com/v1/mcp` with an MCP
     `initialize` gives `200`;
   - `/v1/register` gives `403`.
4. **Refresh.** Note `expires_in` and wait past it, then call again.
   Check that `last_refreshed_at` moved and whether the refresh token
   rotated (`credential list` stays `connected`).
5. **Claude Code.** Add the server with
   `claude mcp add-json --scope user atlassian '{…"Bearer ${ATLASSIAN}"}'`,
   then check `claude mcp list` and a tool call. Unverified: whether
   Claude Code's HTTP MCP transport honors `HTTPS_PROXY` and
   `NODE_EXTRA_CA_CERTS`.
6. **Paperclip.** Do its agents see the user-scope server? If not, does
   its import or gateway path work through the same route? Check the
   reported `delete settings.mcpServers`.
7. **Failure modes.** `credential rm ATLASSIAN` should give the bay `502`
   (missing). Revoke the app at Atlassian: does the next refresh show
   `refresh failed`?

Exit: all seven pass, or the design changes before step 1 of the
implementation.

## Implementation steps (each a green commit, docs included)

1. `feat(credential): show each credential's kind and state`:
   - `Broker::credentials()` replaces `credential_keys()`;
   - list states and `missing`;
   - `credential set` refuses an OAuth credential;
   - cli.md and usage.md.
2. `feat: add an https client over curl`: `src/https.rs`, `HANGAR_CURL` in
   `nix/cli.nix`, and architecture.md's file map.
3. `feat: discover OAuth metadata and register clients`: `src/oauth.rs`,
   pure, unused until step 5.
4. `feat(broker): connect and paste OAuth credentials`: the trait methods,
   the agent-vault `Admin` calls, and the fake.
5. `feat(credential): add 'credential login'`:
   - the command, the browser opener taken out of `vault_ui`, and the
     wait;
   - usage.md "MCP servers" and "OAuth credentials", and
     configuration.md's path-scoped route example;
   - architecture.md "Credentials";
   - an AGENTS.md invariant: refresh tokens and client secrets never
     leave the vault, and hangar never reads credentials back.

## Open questions

- **A `credential logout` verb.** `rm` already does it. Is an alias worth
  the extra surface?
- **Paste mode.** Ship it in step 5, or wait until a provider needs it?
  Atlassian doesn't, if spike step 1 passes.
- **curl as a runtime tool.** Acceptable, or is a TLS crate preferred
  despite AGENTS.md's "fewest dependencies"?
