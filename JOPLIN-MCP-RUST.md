# JP-MCP Plan

Version: 0.2
Status: Draft
Last updated: 2026-05-04
Target MCP spec: 2025-06-18

Normative language uses RFC 2119 meanings: MUST, MUST NOT, SHOULD, SHOULD NOT,
and MAY.

## 1. Purpose

Build a LAN-only Rust MCP system that lets an LLM harness read a user's
unencrypted Joplin Server notes through a local MCP client and a central MCP
server.

The goal is to avoid running one Docker container per user, avoid storing
per-user Joplin credentials in static Nix config, and avoid depending on an
upstream Python MCP image. Joplin Server remains the identity authority. The MCP
server owns its own tokens and its own read indexes.

## 2. Hard Limitations

### Unencrypted Only

This project intentionally supports only unencrypted Joplin data.

Do not implement Joplin E2EE decryption. Do not ask users for Joplin E2EE
passwords. Do not store E2EE keys. Do not attempt to parse encrypted payloads.

If an item has `jop_encryption_applied != 0`, skip it and log a sanitized
counter. Tool responses must say encrypted items are unsupported when relevant.

Reason: Joplin account authentication proves the user can sync data. It does not
give plaintext access to encrypted notes. Adding decryption would turn this into
a Joplin client implementation and would require key management, master key
handling, resource decryption, writes, and trust decisions that are outside this
project.

### Read-Only First

Version 1 is read-only.

Do not write directly to Joplin Server Postgres tables. Do not create, update,
delete, tag, move, or share notes through direct SQL.

Reason: Joplin Server is a sync system. Direct writes risk bypassing sync
events, item serialization, resource storage, delete semantics, share semantics,
and client expectations.

### Owner Items Only

Version 1 indexes items owned by the authenticated Joplin user only.

Shared notebooks are explicitly unsupported until the Joplin visibility model is
implemented and tested. For now, do not infer access from shares.

### LAN Only

The MCP server must be reachable only from the LAN or a narrower allow-list. Do
not expose it publicly. Do not put it behind a public reverse proxy route.

LAN-only is a reachability limit, not the whole security model. The service MUST
still use TLS for non-localhost traffic, validate request origins for browser
reachable endpoints, and require authentication on every MCP request.

### Non-Goals

Version 1 does not implement:

- Joplin E2EE decryption
- write tools
- shared notebook access
- resource binary download
- multi-Joplin-server federation
- direct mutation of Joplin tables

### Glossary

MCP token:

The credential minted by `joplin-mcpd` and presented to MCP endpoints.

Joplin session token:

The Joplin Server credential returned by Joplin authentication. It is not passed
to the LLM harness and must be invalidated after identity resolution when
Joplin exposes a supported invalidation endpoint.

MCP user ID:

The UUID in `joplin_mcp.mcp_users.id`.

Joplin user ID:

The stable user ID from Joplin Server.

Joplin item ID:

The server-side `items.id` row identifier.

Joplin content ID:

The client-side `items.jop_id` value embedded in synced item content.

## 3. Actors

There are three actors.

### MCP Client

The local binary called by the LLM harness.

Responsibilities:

- bootstrap login when needed
- store the MCP token under `${XDG_RUNTIME_DIR}/joplin-mcp-client/`
- read the token for MCP calls
- proxy or call the central MCP server
- pass observability fields such as `test.id`

The client is not the Joplin Server. It does not own indexes. It does not store
Joplin passwords unless the operator configures a local password source.

### MCP Server

The central LAN service.

Responsibilities:

- accept bootstrap credentials from the MCP client or web login page
- authenticate those credentials against Joplin Server
- map the Joplin user to a local MCP user
- mint an MCP-specific access token
- validate MCP tokens on every MCP request
- build and refresh per-user indexes in Postgres
- serve read-only MCP tools
- emit centralized logs via OTLP/HTTP protobuf to VictoriaLogs

The MCP server must not store Joplin passwords.

### Joplin Server

The existing Joplin Server instance.

Responsibilities:

- authenticate email/password during bootstrap or web login
- remain the source of Joplin user identity
- remain the source of truth for Joplin items in Postgres

## 4. Big Picture

The system is a hybrid.

```text
LLM harness
  calls local joplin-mcp-client

joplin-mcp-client
  bootstraps with Joplin credentials when no token exists
  stores ${XDG_RUNTIME_DIR}/joplin-mcp-client/token
  calls central joplin-mcpd with MCP token

joplin-mcpd
  verifies bootstrap credentials with Joplin Server
  mints MCP tokens
  reads Joplin Postgres
  writes derived indexes in joplin_mcp schema
  serves MCP read tools
  logs to VictoriaLogs via OTLP/HTTP protobuf

Joplin Server
  authenticates users
  owns the canonical Joplin data

Postgres
  stores Joplin tables
  stores joplin_mcp derived indexes and token metadata
```

## 5. Authentication Model

Use separate MCP tokens.

A Joplin session token and an MCP token are different credentials. Joplin
credentials prove identity once. The MCP token authorizes MCP access afterward.

Reasons to use separate MCP tokens:

- avoid handing a real Joplin API session token to the LLM harness
- scope tokens to MCP read-only access
- revoke MCP access without changing the Joplin account
- record client labels and last use
- bind future tokens to capability sets
- invalidate temporary Joplin sessions after login and user lookup

## 6. Bootstrap Flow

### Programmatic Flow

The main path is programmatic.

```text
1. joplin-mcp-client starts.
2. It checks ${XDG_RUNTIME_DIR}/joplin-mcp-client/token.
3. If the token exists, it asks joplin-mcpd whether it is still valid.
4. If valid, it uses the token.
5. If missing or invalid, it obtains Joplin email/password from a configured
   source:
   - prompt
   - stdin
   - rbw command
   - local SOPS-managed secret
   - another local command
6. It sends email/password to joplin-mcpd.
7. joplin-mcpd calls Joplin Server auth.
8. If auth succeeds, joplin-mcpd upserts the MCP user, starts index creation
   if needed, mints an MCP token, and returns it once.
9. joplin-mcp-client writes the token to
   ${XDG_RUNTIME_DIR}/joplin-mcp-client/token with mode 0600.
10. Normal MCP calls use the MCP token.
```

### Testing Secret Source

For integration and end-to-end tests only, the harness may read real Joplin test
credentials from repo-local SOPS data. Production code must not depend on this
path.

Required test secret keys:

```text
sops -d secrets.yaml | yq '.joplin|keys'

- url
- username
- password
```

Test code may read `.joplin.url`, `.joplin.username`, and `.joplin.password`
from `secrets.yaml` after decrypting with SOPS. Do not print these values. Do
not write them into logs, snapshots, panic messages, or generated config files.
If `secrets.yaml` is absent or the keys are missing, real-Joplin tests must skip
unless the test target explicitly requires live credentials.

### Web Login Flow

The web path is an additional human-friendly path.

```text
1. User opens https://joplin-mcp.lan/login.
2. User enters Joplin email/password and client label.
3. joplin-mcpd authenticates against Joplin Server.
4. joplin-mcpd returns a one-time visible MCP token or client config snippet.
5. User places the token where the local MCP client can read it.
```

The web login path uses the same server-side token creation code as the
programmatic path.

## 7. Client Requirements

Build a local Rust binary named `joplin-mcp-client`.

It must support at least these commands:

```text
joplin-mcp-client bootstrap
joplin-mcp-client serve
joplin-mcp-client status
joplin-mcp-client logout
```

### bootstrap

Options:

```text
--server-url https://joplin-mcp.lan
--email user@example.com
--password-stdin
--password-command "rbw get joplin-password"
--prompt-password
--token-file ${XDG_RUNTIME_DIR}/joplin-mcp-client/token
--url-file ${XDG_RUNTIME_DIR}/joplin-mcp-client/url
--client-label chuck-laptop-codex
--test-id <uuid>
--server-fingerprint sha256:<base64>
```

Behavior:

- default to `${XDG_RUNTIME_DIR}/joplin-mcp-client/`
- create the token directory if missing
- require directory mode 0700
- write token file mode 0600
- never print password
- never print token unless explicitly requested with `--print-token`
- support idempotency by reusing a valid token
- fall back to `/run/joplin-mcp-client/` only for a configured system service
  user with a pre-created runtime directory

### serve

This is the mode called by the LLM harness.

The primary remote transport is MCP Streamable HTTP, pinned to the target MCP
spec version at the top of this document.

If the harness supports remote MCP over Streamable HTTP with Authorization
headers, `serve` is unnecessary. Configure the harness to call `joplin-mcpd`
directly.

If the harness expects stdio, `serve` exposes local stdio MCP and proxies
requests to `joplin-mcpd`:

```text
LLM harness <stdio> joplin-mcp-client <Streamable HTTP> joplin-mcpd
```

The client must read the token from
`${XDG_RUNTIME_DIR}/joplin-mcp-client/token` before opening the server
connection.

Legacy HTTP+SSE support is optional compatibility only. Do not make `/sse` and
`/message` the default v1 transport.

### status

Checks:

- token file exists
- token is accepted by the MCP server
- server reports index status
- server reports read-only mode

### logout

Calls token revocation on `joplin-mcpd`, then removes the local token file.

## 8. Server Requirements

Build a Rust service named `joplin-mcpd`.

Recommended crates:

```text
axum              HTTP server
tokio             async runtime
sqlx              Postgres access and migrations
reqwest           Joplin Server auth calls
serde             JSON
uuid              IDs
rand              token generation
hmac              token hashing strategy
subtle            constant-time byte comparison when needed
tracing           structured logs
opentelemetry     OTLP logs
```

Use a Rust MCP library if it has working server support for the chosen
transport. If the available library does not support the required transport
cleanly, isolate MCP protocol handling behind a `transport` module so it can be
replaced later.

### HTTP Endpoints

Required non-MCP endpoints:

```text
GET  /healthz
GET  /readyz
POST /api/bootstrap/login
POST /api/token/check
POST /api/token/revoke
GET  /api/index/status
GET  /login
POST /login
```

MCP transport endpoints:

```text
POST /mcp
GET  /mcp
DELETE /mcp
```

The `/mcp` endpoint uses MCP Streamable HTTP. It MUST require `Authorization:
Bearer <mcp-token>`, validate the `Origin` header, and honor the
`MCP-Protocol-Version` header.

Version 1 uses stateless Streamable HTTP:

```text
POST /mcp    handles JSON-RPC requests, notifications, and responses
GET /mcp     returns 405 Method Not Allowed
DELETE /mcp  returns 405 Method Not Allowed
```

The server does not issue `Mcp-Session-Id` in v1. Clients must not require MCP
session affinity from this server. If stateful MCP sessions are added later,
they must be implemented as an explicit transport-layer feature.

`POST /mcp` returns `application/json` for JSON-RPC requests. For JSON-RPC
notifications and responses, return `202 Accepted` with no body. Do not return
`text/event-stream` in v1. If a client does not accept `application/json`, return
`406 Not Acceptable`.

Requests after initialization must send `MCP-Protocol-Version: 2025-06-18`.
Requests with an unsupported version return `400 Bad Request`. If a request omits
the header outside initialization, treat it as unsupported rather than silently
upgrading it.

Version 1 uses a custom `mcp_<base64url>` Bearer token issued by
`/api/bootstrap/login`. It does not implement MCP OAuth 2.1 discovery or dynamic
client registration. A future version may add OAuth if a harness requires it.

Origin policy:

```text
1. /mcp and /api/* reject any present Origin not in server.allowed_origins.
2. /mcp and /api/* allow absent Origin only for non-browser JSON requests with
   Authorization: Bearer.
3. GET /login may have no Origin because normal browser navigation omits it.
4. POST /login rejects any present Origin or Referer not in allowed_origins.
```

If legacy HTTP+SSE is added for an older harness, keep it in a separate
transport module and require an explicit compatibility flag.

### Server Config

Static config comes from Nix/SOPS and environment or config files.

Example:

```toml
[server]
listen = "0.0.0.0:8081"
public_base_url = "https://joplin-mcp.lan"
lan_cidrs = ["192.168.0.0/16", "10.0.0.0/8"]
tls_mode = "required"
allowed_origins = ["https://joplin-mcp.lan"]
allow_insecure_localhost = false
trusted_proxies = []
forwarded_header = "x-forwarded-for"
request_timeout_seconds = 30
slow_request_log_threshold_ms = 2000
shutdown_grace_seconds = 30

[joplin]
base_url = "https://joplin.lan"

[postgres]
dsn_file = "/run/credentials/joplin-mcpd.service/postgres-dsn"
runtime_max_connections = 12
indexer_max_connections = 4
acquire_timeout_seconds = 5
statement_timeout_seconds = 15

[tokens]
hmac_keys = [
  { id = "2026-05", file = "/run/credentials/joplin-mcpd.service/token-hmac-key" }
]
active_hmac_key_id = "2026-05"
default_scope = "read"
expires_after_days = 90
allow_non_expiring_tokens = false

[bootstrap_rate_limit]
per_ip_per_minute = 5
per_email_per_hour = 20

[mcp]
protocol_version = "2025-06-18"
max_response_bytes = 65536
default_body_truncate_chars = 8000
tool_timeout_seconds = 20

[index]
source = "joplin_db"
refresh_interval_seconds = 60
full_rebuild_interval_hours = 24
max_parallel_users = 4
text_search_config = "simple"
incremental_lookback_max_seconds = 86400

[logging]
service_name = "joplin-mcpd"
otlp_logs_endpoint = "http://victorialogs:9428/insert/opentelemetry/v1/logs"
otlp_protocol = "http/protobuf"
email_display = "redacted"
```

Do not store per-user Joplin passwords here.

Reverse-proxy IP policy:

```text
1. If server.trusted_proxies is empty, ignore Forwarded and X-Forwarded-For.
2. Use the socket peer address for rate limits and audit remote_ip.
3. If trusted_proxies is non-empty, accept the configured forwarded_header only
   when the socket peer is inside one trusted CIDR.
4. Reject malformed forwarded IP values instead of guessing.
```

Version 1 is single-instance only. Running two `joplin-mcpd` processes against
the same `joplin_mcp` schema is unsupported. At startup, the server should take a
process-wide Postgres advisory lock and fail if another instance already holds
it.

Request timeout policy:

```text
HTTP request timeout: server.request_timeout_seconds
MCP tool timeout:     mcp.tool_timeout_seconds
Postgres statement:   postgres.statement_timeout_seconds
Slow request log:     server.slow_request_log_threshold_ms
```

On shutdown, stop accepting new `/mcp` requests, let in-flight requests finish
within `server.shutdown_grace_seconds`, stop indexer workers after their current
batch, flush logs, close DB pools, then exit.

## 9. Joplin Authentication

The MCP server authenticates bootstrap credentials by calling Joplin Server.

Expected request shape, based on the current Python implementation being
replaced:

```http
POST /api/sessions
Content-Type: application/json

{
  "email": "user@example.com",
  "password": "secret"
}
```

On success, Joplin Server returns a session object. The MCP server must use it
only long enough to resolve the user ID, then explicitly invalidate that Joplin
session through the supported Joplin logout/session deletion endpoint. Do not
store the Joplin session token.

Expected response shape:

```json
{
  "id": "joplin-session-id",
  "user_id": "joplin-user-id"
}
```

If the deployed Joplin Server version has no supported session invalidation
endpoint, document that evidence in the implementation and rely on Joplin's
server-side session expiry. Do not silently leave reusable Joplin sessions behind
because the MCP server uses direct Postgres access for normal reads.

After Joplin auth succeeds, use the `user_id` from the Joplin session response as
the stable identity key. Resolve the current Joplin user row by that ID in
Postgres to refresh display data such as email. Do not key identity on email.

Bootstrap upsert rule:

```sql
INSERT INTO joplin_mcp.mcp_users (id, joplin_user_id, joplin_email, last_login_at)
VALUES ($1, $2, $3, now())
ON CONFLICT (joplin_user_id)
DO UPDATE SET
  joplin_email = EXCLUDED.joplin_email,
  last_login_at = now();
```

If the Joplin auth response omits `user_id`, or if the returned user ID cannot be
resolved in Postgres, fail the bootstrap and log a sanitized error. Do not fall
back to matching by email.

Bootstrap failure responses must be uniform. Do not reveal whether the email was
unknown, the password was wrong, or Joplin auth was temporarily unavailable in
the client-visible error. Differentiate only in structured logs and the audit
trail.

`POST /api/bootstrap/login` and `POST /login` MUST enforce the configured rate
limits before calling Joplin Server. Log and audit rate-limit outcomes without
recording the password.

Operators MAY disable programmatic credential entry and require the web login
flow only. This is useful when the local CLI trust boundary is weak.

## 10. Token Model

The token returned to the client is an MCP token.

Raw token format:

```text
mcp_<base64url random bytes>
```

Generate at least 32 random bytes before base64url encoding.

Never store the raw token. Store a keyed HMAC.

Recommended:

```rust
fn token_hash(hmac_key: &[u8], raw_token: &str) -> [u8; 32] {
    // HMAC-SHA256(raw_token)
    // Pseudocode only. Use a reviewed crate.
}
```

Every authenticated request must:

```text
1. read Authorization: Bearer <token>
2. reject missing or malformed tokens
3. hash token
4. look up active token row
5. reject revoked tokens
6. reject expired tokens if expires_at is set
7. load mcp_users row
8. attach user context to request
```

Do not accept tokens in URLs by default. If URL tokens are needed for a specific
client, put that behind an explicit config flag and disable request path logging.

Default token expiry is 90 days. Non-expiring tokens require an explicit config
flag and an explicit CLI option.

HMAC key rotation is part of v1. Store `hmac_key_id` with each token row. The
server may accept inactive keys during a configured rotation window, but new
tokens must use `active_hmac_key_id`.

HMAC key files contain exactly 32 raw bytes. They are binary credential files,
not hex, base64, or newline-terminated text. Startup must reject a missing key,
a key shorter or longer than 32 bytes, and an `active_hmac_key_id` that does not
exist in `tokens.hmac_keys`.

Bearer tokens over TLS do not need a nonce or replay window in v1. If plaintext
localhost transport is allowed for testing, it must be bound to `127.0.0.1` and
must not be enabled on LAN interfaces.

Database lookup by token hash avoids direct token byte comparison in the hot
path. If any in-memory comparison of token material is introduced, use a
constant-time comparison.

## 11. Postgres Layout

Create a separate schema.

```sql
CREATE SCHEMA IF NOT EXISTS joplin_mcp;
```

Do not add columns to Joplin tables. Do not add triggers to Joplin tables in v1.
Do not add indexes to Joplin tables unless measurement proves a need.

### MCP Users

```sql
CREATE TABLE joplin_mcp.mcp_users (
  id uuid PRIMARY KEY,
  joplin_user_id text NOT NULL UNIQUE,
  joplin_email text NOT NULL,
  display_name text,
  created_at timestamptz NOT NULL DEFAULT now(),
  last_login_at timestamptz,
  disabled_at timestamptz
);
```

### MCP Tokens

```sql
CREATE TABLE joplin_mcp.mcp_tokens (
  id uuid PRIMARY KEY,
  user_id uuid NOT NULL REFERENCES joplin_mcp.mcp_users(id) ON DELETE CASCADE,
  token_hash bytea NOT NULL UNIQUE,
  hmac_key_id text NOT NULL,
  label text NOT NULL,
  scope text NOT NULL DEFAULT 'read',
  created_at timestamptz NOT NULL DEFAULT now(),
  created_from_ip inet,
  last_seen_at timestamptz,
  revoked_at timestamptz,
  revoked_by uuid,
  revoke_reason text,
  revoked_from_ip inet,
  expires_at timestamptz
);

CREATE INDEX mcp_tokens_user_id_idx
  ON joplin_mcp.mcp_tokens(user_id);
```

### Audit Log

```sql
CREATE TABLE joplin_mcp.audit_log (
  id uuid PRIMARY KEY,
  user_id uuid REFERENCES joplin_mcp.mcp_users(id) ON DELETE SET NULL,
  event_type text NOT NULL,
  outcome text NOT NULL,
  client_label text,
  remote_ip inet,
  metadata jsonb NOT NULL DEFAULT '{}',
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX audit_log_user_time_idx
  ON joplin_mcp.audit_log(user_id, created_at DESC);
```

Audit events include bootstrap success/failure, token mint, token revoke, rate
limit rejection, schema validation failure, and indexer state transitions to
`failed`.

`mcp_users`, `mcp_tokens`, and `audit_log` are persistent MCP state. They are not
rebuildable from Joplin data. Index tables below are derived state and may be
rebuilt from Joplin.

### Index State

```sql
CREATE TABLE joplin_mcp.index_state (
  user_id uuid PRIMARY KEY REFERENCES joplin_mcp.mcp_users(id) ON DELETE CASCADE,
  status text NOT NULL,
  last_full_rebuild_at timestamptz,
  last_incremental_at timestamptz,
  last_seen_joplin_updated_time bigint,
  last_error text,
  updated_at timestamptz NOT NULL DEFAULT now()
);
```

Allowed `status` values:

```text
empty
building
ready
failed
stale
```

Status transitions:

```text
empty    -> building when the first full build starts
building -> ready when a full build commits
building -> failed when no previous ready index can be served
ready    -> stale when last_incremental_at is older than 2 * refresh_interval
stale    -> ready when refresh or rebuild catches up
ready    -> failed only for unrecoverable source/schema errors
```

Tools may serve a `stale` index, but responses must include `index_status:
"stale"` metadata. `index_not_ready` is reserved for users with no previous
ready index.

### Notebooks Index

```sql
CREATE TABLE joplin_mcp.notebooks_index (
  user_id uuid NOT NULL REFERENCES joplin_mcp.mcp_users(id) ON DELETE CASCADE,
  joplin_item_id text NOT NULL,
  joplin_id text NOT NULL,
  parent_joplin_id text,
  title text NOT NULL,
  created_time bigint,
  updated_time bigint,
  deleted_time bigint,
  indexed_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (user_id, joplin_id)
);

CREATE INDEX notebooks_parent_idx
  ON joplin_mcp.notebooks_index(user_id, parent_joplin_id);
```

### Notes Index

```sql
CREATE TABLE joplin_mcp.notes_index (
  user_id uuid NOT NULL REFERENCES joplin_mcp.mcp_users(id) ON DELETE CASCADE,
  joplin_item_id text NOT NULL,
  joplin_id text NOT NULL,
  parent_joplin_id text,
  title text NOT NULL,
  body_text text NOT NULL,
  is_todo boolean NOT NULL DEFAULT false,
  created_time bigint,
  updated_time bigint,
  deleted_time bigint,
  resource_refs text[] NOT NULL DEFAULT '{}',
  search_vector tsvector GENERATED ALWAYS AS (
    setweight(to_tsvector('simple', coalesce(title, '')), 'A') ||
    setweight(to_tsvector('simple', coalesce(body_text, '')), 'B')
  ) STORED,
  indexed_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (user_id, joplin_id)
);

CREATE INDEX notes_parent_idx
  ON joplin_mcp.notes_index(user_id, parent_joplin_id);

CREATE INDEX notes_updated_idx
  ON joplin_mcp.notes_index(user_id, updated_time DESC);

CREATE INDEX notes_search_idx
  ON joplin_mcp.notes_index USING gin(search_vector);
```

### Tags Index

```sql
CREATE TABLE joplin_mcp.tags_index (
  user_id uuid NOT NULL REFERENCES joplin_mcp.mcp_users(id) ON DELETE CASCADE,
  joplin_item_id text NOT NULL,
  joplin_id text NOT NULL,
  title text NOT NULL,
  updated_time bigint,
  indexed_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (user_id, joplin_id)
);
```

### Note Tags Index

```sql
CREATE TABLE joplin_mcp.note_tags_index (
  user_id uuid NOT NULL REFERENCES joplin_mcp.mcp_users(id) ON DELETE CASCADE,
  note_joplin_id text NOT NULL,
  tag_joplin_id text NOT NULL,
  updated_time bigint,
  indexed_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (user_id, note_joplin_id, tag_joplin_id)
);

CREATE INDEX note_tags_by_tag_idx
  ON joplin_mcp.note_tags_index(user_id, tag_joplin_id);
```

### Resources Index

```sql
CREATE TABLE joplin_mcp.resources_index (
  user_id uuid NOT NULL REFERENCES joplin_mcp.mcp_users(id) ON DELETE CASCADE,
  joplin_item_id text NOT NULL,
  joplin_id text NOT NULL,
  title text NOT NULL,
  mime text,
  size_bytes bigint,
  file_extension text,
  updated_time bigint,
  indexed_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (user_id, joplin_id)
);
```

### Deleted Items Index

```sql
CREATE TABLE joplin_mcp.deleted_items_index (
  user_id uuid NOT NULL REFERENCES joplin_mcp.mcp_users(id) ON DELETE CASCADE,
  joplin_id text NOT NULL,
  item_type int,
  source text NOT NULL,
  deleted_time bigint,
  tombstoned_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (user_id, joplin_id)
);
```

This table records deletion evidence for IDs that should not appear in active
tool results. It is not a content source.

## 12. Joplin Source Tables

The Joplin Server schema is internal. The implementation must validate the
actual schema at startup and fail early if required columns are missing.

Version 1 uses direct Postgres reads because the Joplin Server database is
already local to the deployment. Keep that decision isolated behind a source
trait:

```rust
trait JoplinSource {
    async fn changed_items_since(&self, user_id: &str, since: Option<i64>) -> Result<Vec<JoplinItem>>;
    async fn item_by_id(&self, user_id: &str, item_id: &str) -> Result<Option<JoplinItem>>;
}
```

The first implementation is `JoplinDbSource`. A future `JoplinApiSource` may use
Joplin's delta sync API, but it is not required for v1.

Expected source tables and fields:

```text
users.id
users.email

items.id                  server-side item ID
items.owner_id            owning Joplin user ID
items.content             raw serialized item content, unless external storage is used
items.name                item path or file name
items.mime_type
items.updated_time
items.created_time
items.jop_id              client-side Joplin item ID
items.jop_parent_id
items.jop_type
items.jop_encryption_applied
```

Joplin's own server item spec states that Joplin stores content in the `items`
table and extracts useful Joplin fields with `jop_` names, including the
client-side `items.jop_id` and server-side `items.id`.

Startup schema check must validate names and types:

```sql
SELECT column_name, data_type
FROM information_schema.columns
WHERE table_schema = current_schema()
  AND table_name = 'items'
ORDER BY column_name;
```

If content is not in `items.content` because Joplin is configured to use an
external content storage driver, v1 must fail early with a clear error. Add
external storage support only after direct DB indexing works.

External-storage startup check:

```text
1. sample up to 100 non-encrypted, non-resource items with jop_encryption_applied = 0
2. ignore empty servers with no matching rows
3. if every sampled row has NULL or empty content, fail startup
4. error message must say: items.content is empty across sampled rows;
   external Joplin content storage is unsupported in joplin-mcpd v1
```

Use `sqlx migrate` as the migration mechanism for the `joplin_mcp` schema.
Migrations are embedded in the binary. Down migrations are not supported in v1.
Use sqlx's `_sqlx_migrations` table as the schema-version source of truth. Do
not add a separate `schema_version` table in v1. The server must refuse to start
if the highest applied migration version is newer than the binary supports.

## 13. Joplin Item Types

Use Joplin type IDs:

```text
1  note
2  folder/notebook
5  tag
6  note_tag
9  resource
13 revision
```

Skip revisions in v1.

## 14. Indexer Design

The indexer is owned by the MCP server.

On successful bootstrap:

```text
1. upsert mcp_users by Joplin user_id from the session response
2. ensure index_state exists
3. if no ready index exists, start an async full build
4. return token immediately with index status
```

Normal refresh:

```text
1. find users with active tokens
2. refresh each active user every index.refresh_interval_seconds
3. if the refresh gap exceeds incremental_lookback_max_seconds, schedule a full rebuild
4. process changed Joplin items since last_seen_joplin_updated_time
5. upsert derived rows
6. purge or tombstone deleted rows
7. update index_state
```

For v1, a full rebuild is acceptable:

```text
DELETE FROM joplin_mcp.notes_index WHERE user_id = $1;
DELETE FROM joplin_mcp.notebooks_index WHERE user_id = $1;
DELETE FROM joplin_mcp.tags_index WHERE user_id = $1;
DELETE FROM joplin_mcp.note_tags_index WHERE user_id = $1;
DELETE FROM joplin_mcp.resources_index WHERE user_id = $1;
DELETE FROM joplin_mcp.deleted_items_index WHERE user_id = $1;
rebuild all rows for the user;
```

Use a transaction. If the rebuild fails, keep the previous ready index when
possible. Do not leave half-built visible state.

Concurrency rules:

```text
1. take a per-user advisory lock before refresh or rebuild
2. skip or reschedule if another worker owns that user
3. run background indexing through postgres.indexer_max_connections only
4. keep foreground MCP/auth/status reads on postgres.runtime_max_connections
5. cap rebuild work by row count, max_parallel_users, and indexer DB capacity
6. never borrow runtime pool connections for full rebuild work
7. update last_seen_joplin_updated_time only after all changed rows commit
```

Deleted items:

If `deleted_time` is non-zero, exclude the item from normal tool responses.
Remove it from active index tables and upsert `deleted_items_index` so status
and debug output can explain why the item is absent.

Before implementing incremental refresh against a real Joplin database, verify
Joplin's deletion source of truth for rows removed from `items`. If Joplin
exposes tombstones, `deleted_items`, or an equivalent deletion feed, include that
source in schema validation and refresh logic. Incremental refresh must purge
derived rows for hard-deleted item IDs, not only rows that remain present with
non-zero `deleted_time`.

If no supported deletion source exists, incremental refresh must be paired with
periodic reconciliation that removes derived rows whose source item no longer
exists.

Reconciliation must also upsert `deleted_items_index` for removed IDs when the
item type can be determined from the prior active index row.

## 15. Content Parser

Joplin serialized item content is text for notes, folders, tags, and metadata
items. Parse it conservatively.

Expected shape:

```text
Title line

Markdown body

id: 0123456789abcdef0123456789abcdef
parent_id: ...
created_time: ...
updated_time: ...
is_todo: 0
type_: 1
```

Parsing rules:

```text
1. Split content into lines.
2. Find candidate metadata start by scanning from the bottom for the last line matching:
   ^id:\s+[0-9a-f]{32}$
3. Accept the candidate only if every following non-empty line is a key/value
   metadata line in the same contiguous footer block.
4. Require the footer to include id, type_, created_time, and updated_time.
5. Reject the candidate if any required footer key is absent or duplicated.
6. The first non-empty line before metadata is title.
7. Body is everything after title and before metadata, trimmed of leading and
   trailing blank lines.
8. Metadata is key/value pairs after metadata start.
9. Trust DB jop_type and jop_encryption_applied more than parsed metadata.
10. If parser fails, log item ID and reason, skip the item, and continue.
```

Rust sketch:

```rust
static ID_RE: once_cell::sync::Lazy<regex::Regex> =
    once_cell::sync::Lazy::new(|| regex::Regex::new(r"^id:\s+[0-9a-f]{32}$").expect("valid regex"));

struct ParsedItem {
    title: String,
    body: String,
    metadata: std::collections::HashMap<String, String>,
}

fn parse_joplin_item(raw: &str) -> anyhow::Result<ParsedItem> {
    let lines: Vec<&str> = raw.lines().collect();

    let metadata_start = lines
        .iter()
        .enumerate()
        .rev()
        .find_map(|(idx, line)| {
            if ID_RE.is_match(line.trim()) && is_valid_metadata_footer(&lines[idx..]) {
                Some(idx)
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow::anyhow!("missing metadata id line"))?;

    let title_pos = lines[..metadata_start]
        .iter()
        .position(|line| !line.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing title"))?;

    let title = lines[title_pos].trim().to_string();
    let body = lines[(title_pos + 1)..metadata_start]
        .join("\n")
        .trim()
        .to_string();

    let mut metadata = std::collections::HashMap::new();
    for line in &lines[metadata_start..] {
        if let Some((k, v)) = line.split_once(':') {
            metadata.insert(k.trim().to_string(), v.trim().to_string());
        }
    }

    Ok(ParsedItem { title, body, metadata })
}
```

`is_valid_metadata_footer` must require a contiguous key/value footer with one
`id`, one `type_`, one `created_time`, and one `updated_time`.

Add parser tests for metadata-looking text inside markdown code blocks,
`source_url:` values with colons, empty metadata values, malformed metadata,
missing required footer keys, duplicated required footer keys, and pasted body
text that contains a valid-looking `id:` line.

## 16. Resource References

Extract resource references from note bodies.

Patterns:

```text
(:/<32 hex chars>)
src=":/<32 hex chars>"
```

Store deduplicated resource IDs in `notes_index.resource_refs`.

Do not inline base64 resources in v1. Large resources can destroy MCP context
size and observability. Provide resource metadata first.

`resource_refs text[]` is acceptable for v1. It makes "which notes reference
this resource?" slower than a join table. If resource reverse lookup becomes a
tool requirement, add `note_resources_index` instead of overloading the array.

## 17. MCP Tools

All tools are read-only.

Every tool advertised by `tools/list` must include an MCP JSON Schema
`inputSchema`. Limits in schemas are part of the security model, not just
documentation.

Uniform response rules:

```text
1. never return another user's data
2. default body text is truncated to mcp.default_body_truncate_chars
3. total serialized response must not exceed mcp.max_response_bytes
4. include truncated=true and a continuation cursor when content is shortened
5. use structured MCP errors for validation, auth, and not-found failures
6. never return silent empty results while the initial index is still building
```

Index-dependent tools must check `index_state` before reading derived tables. If
no ready index exists for the user, return a structured MCP error with code
`index_not_ready`, the current `index_status`, and a bounded
`retry_after_seconds`. If a previous ready index exists while refresh is running,
serve the previous ready index and include `index_status` metadata.

Cursor contract:

```text
1. cursors are opaque to clients and versioned internally
2. list/search cursors must use keyset pagination, not offset pagination
3. cursor payloads include tool name, filter hash, sort keys, and last key values
4. invalid or mismatched cursors return a structured validation error
```

Stable sort keys:

```text
list_notes:        updated_time DESC, joplin_id DESC
get_recent_notes:  updated_time DESC, joplin_id DESC
get_changes_since: updated_time ASC, joplin_id ASC
search_notes:      rank DESC, updated_time DESC, joplin_id DESC
get_notes_by_tag:  updated_time DESC, joplin_id DESC
list_notebooks:    title ASC, joplin_id ASC if pagination is added
list_tags:         title ASC, joplin_id ASC if pagination is added
```

`body_cursor` for `get_note` is separate from list pagination. It must encode the
note ID, the indexed note version, and the next body offset so a cursor from an
older note body cannot resume against a newer body.

### `status`

Returns:

```json
{
  "user": "user@example.com",
  "mode": "read-only",
  "index_status": "ready",
  "last_indexed_at": "2026-05-04T10:00:00Z",
  "unencrypted_only": true,
  "shared_notebooks": "unsupported"
}
```

### `list_notebooks`

Input:

```json
{}
```

Returns notebook titles, IDs, parents, and note counts.

### `list_notes`

Input:

```json
{
  "notebook_id": "optional 32-char Joplin ID",
  "tag_ids": ["optional 32-char Joplin tag IDs"],
  "updated_after": "optional unix ms timestamp",
  "updated_before": "optional unix ms timestamp",
  "is_todo": "optional boolean",
  "limit": 50,
  "cursor": "optional opaque cursor"
}
```

Returns title, ID, notebook, updated time, and short preview.

### `search_notes`

Input:

```json
{
  "query": "text",
  "notebook_id": "optional 32-char Joplin ID",
  "tag_ids": ["optional 32-char Joplin tag IDs"],
  "updated_after": "optional unix ms timestamp",
  "updated_before": "optional unix ms timestamp",
  "is_todo": "optional boolean",
  "limit": 20,
  "cursor": "optional opaque cursor",
  "limit_body_chars": 8000
}
```

Use Postgres full-text search:

```sql
SELECT joplin_id, title, ts_rank(search_vector, plainto_tsquery('simple', $2)) AS rank
FROM joplin_mcp.notes_index
WHERE user_id = $1
  AND search_vector @@ plainto_tsquery('simple', $2)
ORDER BY rank DESC, updated_time DESC, joplin_id DESC
LIMIT $3;
```

When a search cursor is supplied, the query builder must add the matching keyset
predicate for `rank`, `updated_time`, and `joplin_id`.

Fallback to `ILIKE` only if full-text search gives no results and the query is
short enough.

The SQL above shows the default `simple` text search config. If
`index.text_search_config` is changed, migrations and query builders must use
the configured value consistently.

Example `inputSchema`:

```json
{
  "type": "object",
  "properties": {
    "query": {"type": "string", "minLength": 1, "maxLength": 1024},
    "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20},
    "cursor": {"type": "string"}
  },
  "required": ["query"],
  "additionalProperties": false
}
```

### `get_note`

Input:

```json
{
  "note_id": "32-char Joplin ID",
  "body_cursor": "optional opaque body cursor",
  "max_body_chars": 8000
}
```

Returns title, notebook, updated time, body, and resource reference count.

If the body is truncated, return:

```json
{
  "truncated": true,
  "next_cursor": "opaque body cursor"
}
```

### `get_note_excerpt`

Input:

```json
{
  "note_id": "32-char Joplin ID",
  "max_chars": 2000
}
```

Returns title, notebook, updated time, and a bounded excerpt.

### `get_recent_notes`

Input:

```json
{
  "limit": 20,
  "cursor": "optional opaque cursor"
}
```

Returns notes ordered by `updated_time DESC`.

### `list_tags`

Input:

```json
{}
```

Returns tags with note counts for the authenticated user.

### `get_notes_by_tag`

Input:

```json
{
  "tag_id": "32-char Joplin tag ID",
  "limit": 50,
  "cursor": "optional opaque cursor"
}
```

Returns notes associated with one tag.

### `get_notebook_tree`

Input:

```json
{}
```

Returns notebooks as a hierarchy with note counts.

### `get_changes_since`

Input:

```json
{
  "since": "unix ms timestamp",
  "limit": 100,
  "cursor": "optional opaque cursor"
}
```

Returns notes changed since the supplied timestamp. This is the primary tool for
LLM sessions that need to resume work without rescanning all notes.

### `get_note_resources`

Input:

```json
{
  "note_id": "32-char Joplin ID"
}
```

Returns resource IDs and metadata for resources referenced by the note.

Do not download binary data in v1.

## 18. Observability Requirements

Centralized logging is a hard requirement.

The server and client must emit structured logs through OpenTelemetry logs to
VictoriaLogs.

### VictoriaLogs OTLP Format

Use OTLP/HTTP with binary protobuf payloads.

VictoriaLogs official docs specify the log ingestion endpoint:

```text
http://victorialogs:9428/insert/opentelemetry/v1/logs
```

OpenTelemetry OTLP/HTTP binary protobuf uses:

```text
Content-Type: application/x-protobuf
Body: protobuf-encoded ExportLogsServiceRequest
Protocol setting: http/protobuf
```

Required config:

```text
OTEL_EXPORTER_OTLP_LOGS_ENDPOINT=http://victorialogs:9428/insert/opentelemetry/v1/logs
OTEL_EXPORTER_OTLP_LOGS_PROTOCOL=http/protobuf
```

Do not use JSON OTLP for this project.

VictoriaLogs treats OpenTelemetry resource attributes as log stream fields by
default. Set the `VL-Stream-Fields` HTTP header to keep stream cardinality under
control:

```text
VL-Stream-Fields: service.name,service.instance.id,deployment.environment
```

Do not include user IDs, emails, token IDs, request IDs, or test IDs in stream
fields. Those belong as regular log fields.

### Required Log Fields

Every server log event must include these when available:

```text
service.name
service.version
service.instance.id
deployment.environment
request.id
test.id
client.label
mcp.user_id
joplin.user_id
token.id
index.status
operation
outcome
error.kind
```

Never log:

```text
Joplin password
raw MCP token
token hash
note body
resource binary content
full Authorization header
```

Email is logged as a redacted display value, for example `c***@example.com`.
Do not log raw email addresses.

### Metrics and Traces

Logs are required. Metrics and traces are also part of the v1 design.

Required metrics:

```text
index_lag_seconds by user
index_refresh_duration_seconds
mcp_tool_duration_seconds by tool
mcp_tool_errors_total by tool and error.kind
bootstrap_login_total by outcome
postgres_pool_wait_seconds by pool
```

Required trace spans:

```text
bootstrap login -> joplin auth -> user upsert -> token mint
index refresh -> source query -> row upserts -> state update
MCP tool call -> DB query -> response serialization
```

### Alerts

At minimum, define alert conditions for:

```text
index status failed for any user
bootstrap errors above threshold
VictoriaLogs ingestion failure
Postgres runtime or indexer pool saturation
MCP tool error rate above threshold
```

### Audit Trail

Security-relevant events must be written to `joplin_mcp.audit_log` in addition
to normal logs. Audit retention is independent of VictoriaLogs retention.

### Rust Logging Sketch

Pseudocode:

```rust
fn init_logging(config: &Config) -> anyhow::Result<LogGuard> {
    // Configure an OpenTelemetry logs exporter using OTLP/HTTP protobuf.
    // Endpoint:
    //   /insert/opentelemetry/v1/logs
    // Header:
    //   Content-Type: application/x-protobuf
    //   VL-Stream-Fields: service.name,service.instance.id,deployment.environment
    //
    // Attach tracing subscriber and opentelemetry log appender.
    // Return a guard so shutdown can flush.
    Ok(LogGuard {})
}
```

The implementation must flush logs on shutdown.

### Fail-Early Logging

At startup:

```text
1. initialize logging
2. emit startup log
3. validate config
4. validate DB connection
5. validate Joplin auth endpoint reachability
6. validate Joplin schema
7. validate migrations
8. start serving
```

If logging cannot initialize, the service must fail before listening.

Integration tests must also verify that expected logs are queryable from
VictoriaLogs. This catches silent exporter misconfiguration.

## 19. Test Observability

Every test must have a test ID.

Generate a UUID at test start:

```text
test.id = jp-mcp-test-<uuid>
```

The test harness must pass it through:

```text
X-Test-Id: jp-mcp-test-...
```

The client must forward it to the server.

The server must attach it to every log emitted while handling that request,
including:

```text
bootstrap login
token check
index build start
index build result
MCP tool call
MCP tool result
error paths
```

Tests must query VictoriaLogs before passing:

```text
POST /select/logsql/query
query={service.name="joplin-mcpd" test.id="jp-mcp-test-..."}
```

VictoriaLogs docs list `/select/logsql/query` as the HTTP query endpoint.

Because log ingestion is asynchronous, tests must poll the query until expected
events appear or the timeout expires. Use bounded backoff starting at 250 ms,
cap each sleep at 2 seconds, and fail after 30 seconds by default.

An integration test that cannot find its expected log events must fail, even if
the functional response was correct.

## 20. Test Requirements

### Unit Tests

Required unit tests:

```text
parse note item
parse empty body
parse notebook item
parse malformed item
reject footer missing required metadata keys
reject duplicated required footer keys
do not split body on pasted valid-looking id line
skip encrypted item
extract resource references
deduplicate resource references
hash token and reject raw-token lookup
reject HMAC key files that are not exactly 32 raw bytes
redact secrets in errors
validate Joplin IDs
property-test parse_joplin_item malformed content
fuzz resource reference extraction
```

### Database Tests

Use an isolated Postgres database.

Required tests:

```text
migrations apply cleanly
newer _sqlx_migrations version blocks startup
schema validation fails if required Joplin columns are absent
bootstrap upserts mcp_users
bootstrap with changed Joplin email updates mcp_users.joplin_email
token lookup rejects revoked token
full index rebuild is transactional
incremental lookback cap triggers full rebuild
notes_search_idx returns expected notes
owner-only filtering excludes other users
encrypted item filtering excludes encrypted items
note_tags_index supports tag filtering
deleted_time items are excluded from normal reads
deleted_items_index records tombstones
hard-deleted item IDs purge derived index rows
stale index status transitions back to ready after refresh
runtime DB pool still serves status while indexer pool is saturated
keyset cursors preserve stable order across pages
schema validation fails on wrong column type
```

### Server Integration Tests

Required tests:

```text
bootstrap success returns token and starts index
bootstrap uses Joplin session user_id instead of email as identity key
bootstrap invalidates the temporary Joplin session after user lookup
bootstrap failure rejects invalid Joplin credentials
token check accepts valid token
token check rejects unknown token
POST /mcp returns application/json for request responses
GET /mcp returns 405
DELETE /mcp returns 405
unsupported MCP-Protocol-Version returns 400
unexpected Origin is rejected
status tool returns read-only and unencrypted-only
index-dependent tools return index_not_ready during initial build
stale index results include index_status metadata
search tool returns only current user's notes
get_note rejects another user's note ID
index failure leaves prior ready index in place
two refresh jobs for one user do not corrupt index state
second server instance against same database fails startup
external content storage detection fails startup
request and tool timeouts are enforced
shutdown drains in-flight requests and flushes logs
rate limiting rejects bootstrap brute force
large note response is truncated within response budget
```

### Client Integration Tests

Required tests:

```text
bootstrap writes token mode 0600
bootstrap uses XDG_RUNTIME_DIR by default
bootstrap reuses valid token
bootstrap refreshes invalid token
serve fails early when token missing
logout revokes server token and removes local token
test.id is forwarded to server logs
```

### End-to-End Test

An end-to-end test must start:

```text
Postgres
fake or real Joplin auth endpoint
joplin-mcpd
joplin-mcp-client
VictoriaLogs
```

It must:

```text
1. insert fixture Joplin users/items
2. bootstrap as user A
3. search for a known note
4. verify user B's note is not visible
5. verify encrypted note is not visible
6. page through search results with a keyset cursor
7. poll VictoriaLogs by test.id
8. fail if logs are missing after the timeout
```

When the end-to-end test runs against a real Joplin Server instead of the fake
auth endpoint, it must load test-only `url`, `username`, and `password` from
`secrets.yaml` via SOPS and must not emit those values.

## 21. Fail-Early Rules

The server must refuse to start if:

```text
OTLP logging config is invalid
Postgres DSN is missing
Postgres connection fails
required joplin_mcp migrations are missing
_sqlx_migrations records a newer version than the binary supports
Joplin source schema validation fails
external content storage is detected
token HMAC key is missing
token HMAC key is not exactly 32 raw bytes
token HMAC key id in config is missing from key list
another joplin-mcpd instance holds the singleton lock
TLS is disabled for a non-localhost listener
Origin validation is disabled for browser-reachable endpoints
listen address is public while LAN allow-list is disabled
```

The server may start degraded if Joplin Server auth endpoint is temporarily
unreachable, but `/readyz` must report not ready and bootstrap must fail with a
clear error.

The client must fail before serving if:

```text
token file is missing and bootstrap is disabled
token file permissions are broader than 0600
token directory permissions are broader than 0700
token check fails
server URL is missing
TLS fingerprint check fails when configured
```

## 22. Security Requirements

### Token Storage

Client token path:

```text
${XDG_RUNTIME_DIR}/joplin-mcp-client/token
```

Permissions:

```text
${XDG_RUNTIME_DIR}/joplin-mcp-client       0700
${XDG_RUNTIME_DIR}/joplin-mcp-client/token 0600
```

Fallback to `/run/joplin-mcp-client/token` only for a system service account
with an explicitly managed runtime directory.

Server stores only token hashes.

### Transport Security

The server MUST serve TLS for all LAN traffic. Self-signed certificates are
acceptable only when the client pins the certificate fingerprint with
`--server-fingerprint`.

Plaintext HTTP is allowed only when:

```text
1. allow_insecure_localhost = true
2. the server listens on 127.0.0.1 or ::1
3. the client config is local-test only
```

Browser-reachable endpoints MUST validate `Origin` and reject unexpected
origins.

### Password Handling

Joplin passwords:

```text
accepted only during bootstrap/login
kept in memory only long enough to call Joplin Server
never logged
never stored
never written to panic messages
```

Bootstrap login MUST be rate-limited per IP and per email. Client-visible errors
must stay uniform across unknown-email, bad-password, rate-limited, and temporary
Joplin failures.

### SQL Access

The MCP server Postgres role should have:

```text
read access to required Joplin tables
read/write access to joplin_mcp schema
no schema ownership over Joplin tables
no superuser
```

Future hardening can split roles:

```text
joplin_mcp_indexer   read Joplin tables, write joplin_mcp indexes
joplin_mcp_runtime   read joplin_mcp indexes, read tokens
```

### Logs

Logs are operational data, not a note export mechanism.

No note bodies in logs. No search query bodies if they may contain private text.
For search, log query length and a query hash, not the query itself.

## 23. Backup Requirements

The `joplin_mcp` schema contains both persistent MCP state and derived indexes.

Back up persistent tables:

```text
joplin_mcp.mcp_users
joplin_mcp.mcp_tokens
joplin_mcp.audit_log
```

Derived index tables may be excluded from backups because they rebuild from
Joplin:

```text
joplin_mcp.index_state
joplin_mcp.notebooks_index
joplin_mcp.notes_index
joplin_mcp.tags_index
joplin_mcp.note_tags_index
joplin_mcp.resources_index
joplin_mcp.deleted_items_index
```

If `audit_log` retention is shortened later, the long-term security archive must
come from VictoriaLogs or another external log store before old rows are removed.

## 24. NixOS/LXC Deployment Shape

The project should fit the existing flake-based repo patterns.

Expected host/container:

```text
lxc/joplin-mcp.nix or modules/services/joplin-mcp.nix
```

Service user:

```text
users.users.joplin-mcp = {
  isSystemUser = true;
  group = "joplin-mcp";
};
```

Systemd shape:

```nix
systemd.services.joplin-mcpd = {
  serviceConfig = {
    User = "joplin-mcp";
    Group = "joplin-mcp";
    DynamicUser = false;
    StateDirectory = "joplin-mcp";
    RuntimeDirectory = "joplin-mcp";
    UMask = "0077";
    LoadCredential = [
      "postgres-dsn:/run/secrets/joplin-mcp/postgres-dsn"
      "token-hmac-key:/run/secrets/joplin-mcp/token-hmac-key"
    ];
  };
};
```

Do not put secrets into Nix store strings.

## 25. Implementation Modules

Suggested Rust module layout:

```text
crates/
  joplin-mcpd/
    src/
      main.rs
      config.rs
      logging.rs
      http.rs
      auth/
        mod.rs
        joplin.rs
        tokens.rs
      db/
        mod.rs
        migrations.rs
        schema_check.rs
      indexer/
        mod.rs
        parser.rs
        rebuild.rs
        refresh.rs
      mcp/
        mod.rs
        transport.rs
        tools.rs
      observability/
        mod.rs
        request_context.rs
  joplin-mcp-client/
    src/
      main.rs
      bootstrap.rs
      token_file.rs
      proxy.rs
      config.rs
```

Keep boundaries strict:

```text
auth does not parse notes
indexer does not validate HTTP tokens
mcp tools read index tables only
logging never receives raw secrets
client never connects to Postgres
```

## 26. First Milestone

Milestone 1 is a local proof.

Deliver:

```text
joplin-mcpd starts
migrations create joplin_mcp schema
schema check validates fixture Joplin tables
bootstrap login against fake Joplin auth succeeds
MCP token is minted and stored hashed
HMAC key file format is validated
indexer builds notes_index from fixture data
search_notes returns one fixture note
list_tags returns one fixture tag
VictoriaLogs receives protobuf OTLP logs
test polls VictoriaLogs by test.id
```

No real Joplin Server required for milestone 1.

## 27. Second Milestone

Milestone 2 connects to the real Joplin Server database.

Deliver:

```text
read real users table
authenticate against real Joplin Server
load real-Joplin test credentials from SOPS secrets.yaml without logging them
resolve real joplin_user_id from Joplin session response
verify changed Joplin email refreshes mcp_users
verify temporary Joplin session is invalidated after bootstrap
build index for one user
compare one note against Joplin UI/API manually
verify encrypted items are skipped
verify another user is not visible
verify soft-deleted notes are not returned
verify hard-deleted items or tombstones purge derived rows
verify tag filters match Joplin UI/API manually
verify logs in VictoriaLogs
```

## 28. Third Milestone

Milestone 3 makes it usable from the LLM harness.

Deliver:

```text
joplin-mcp-client bootstrap
joplin-mcp-client serve
LLM harness config example
status tool
list_notebooks
list_tags
get_notebook_tree
list_notes
search_notes
get_note
get_note_excerpt
get_changes_since
logout
```

## 29. Acceptance Criteria

The project is acceptable when:

```text
unencrypted notes for the authenticated user can be searched and read
encrypted notes are skipped and reported as unsupported
another user's notes are not visible
Joplin passwords are never stored
raw MCP tokens are never stored
client token is stored under XDG_RUNTIME_DIR with mode 0600
MCP Streamable HTTP is the primary remote transport
MCP Streamable HTTP v1 is stateless: POST /mcp only, GET/DELETE return 405
custom Bearer auth is documented as a v1 OAuth 2.1 non-goal
TLS is required for LAN traffic
Origin policy is explicit for browser and non-browser requests
reverse-proxy IP attribution is explicit and safe by default
server indexes are stored in joplin_mcp schema
sqlx _sqlx_migrations is the schema-version source of truth
external Joplin content storage fails early
tag filters work through note_tags_index
deleted_items_index records deletion evidence
stale index status has defined transitions
initial index build returns index_not_ready instead of silent empty results
foreground MCP/status requests do not depend on indexer pool availability
large note bodies are truncated with continuation metadata
paginated tools use keyset cursors with stable sort keys
single-instance deployment is enforced
server/tool/statement timeouts and shutdown drain are defined
persistent MCP users/tokens are backed up separately from derived indexes
all write tools are absent
VictoriaLogs receives OTLP/HTTP protobuf logs
metrics expose index lag and tool latency
tests pass only after polling VictoriaLogs by test.id
startup fails early on bad DB/schema/token/logging config
```

## 30. References

- Joplin Server MCP repo being replaced:
  `https://github.com/Alexander-Zhukov/joplin-server-mcp`
- Joplin Server items spec:
  `https://joplinapp.org/help/dev/spec/server_items/`
- Joplin Server delta sync spec:
  `https://joplinapp.org/help/dev/spec/server_delta_sync/`
- Joplin E2EE spec:
  `https://joplinapp.org/help/dev/spec/e2ee/`
- MCP Streamable HTTP transport spec:
  `https://modelcontextprotocol.io/specification/2025-06-18/basic/transports`
- MCP authorization and security specs:
  `https://modelcontextprotocol.io/specification/2025-06-18/basic/authorization`
  `https://modelcontextprotocol.io/specification/2025-06-18/basic/security_best_practices`
- MCP tools spec:
  `https://modelcontextprotocol.io/specification/2025-06-18/server/tools`
- VictoriaLogs OpenTelemetry ingestion:
  `https://docs.victoriametrics.com/victorialogs/data-ingestion/opentelemetry/`
- VictoriaLogs query API:
  `https://docs.victoriametrics.com/victorialogs/querying/`
- OpenTelemetry OTLP specification:
  `https://opentelemetry.io/docs/specs/otlp/`

## 31. Change Log

### 0.2 - 2026-05-04

- Adopted MCP Streamable HTTP as the primary remote transport.
- Added TLS, Origin validation, XDG runtime token storage, finite token expiry,
  HMAC key rotation, bootstrap rate limits, audit logging, tag indexing, tool
  JSON Schema requirements, response-size budgets, metrics, traces, and deleted
  item behavior.
- Clarified stateless MCP transport behavior, Joplin user_id bootstrap identity,
  sqlx schema-version gating, HMAC key format, external-storage detection,
  stale/tombstone index behavior, single-instance operation, timeouts, shutdown
  drain, reverse-proxy IP handling, and backup boundaries.
