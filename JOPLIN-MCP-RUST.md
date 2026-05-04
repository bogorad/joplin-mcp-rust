# JP-MCP Plan

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

## 3. Actors

There are three actors.

### MCP Client

The local binary called by the LLM harness.

Responsibilities:

- bootstrap login when needed
- store the MCP token under `/run/joplin-mcp-client/`
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
  stores /run/joplin-mcp-client/token
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
- keep Joplin sessions server-side or discard them after login

## 6. Bootstrap Flow

### Programmatic Flow

The main path is programmatic.

```text
1. joplin-mcp-client starts.
2. It checks /run/joplin-mcp-client/token.
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
9. joplin-mcp-client writes the token to /run/joplin-mcp-client/token with
   mode 0600.
10. Normal MCP calls use the MCP token.
```

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
--token-file /run/joplin-mcp-client/token
--url-file /run/joplin-mcp-client/url
--client-label chuck-laptop-codex
--test-id <uuid>
```

Behavior:

- create `/run/joplin-mcp-client/` if missing
- require directory mode 0700
- write token file mode 0600
- never print password
- never print token unless explicitly requested with `--print-token`
- support idempotency by reusing a valid token

### serve

This is the mode called by the LLM harness.

The exact transport depends on the harness:

- if the harness supports remote MCP over SSE/HTTP with auth headers, `serve`
  can be unnecessary
- if the harness expects stdio, `serve` should expose local stdio MCP and proxy
  requests to `joplin-mcpd`

For the first implementation, prefer stdio proxy mode because it works with
more harnesses:

```text
LLM harness <stdio> joplin-mcp-client <HTTP/SSE> joplin-mcpd
```

The client must read the token from `/run/joplin-mcp-client/token` before
opening the server connection.

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
argon2 or hmac    token hashing strategy
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
GET  /sse
POST /message
```

If a newer MCP transport is adopted later, keep it separate from the auth and
index layers.

### Server Config

Static config comes from Nix/SOPS and environment or config files.

Example:

```toml
[server]
listen = "0.0.0.0:8081"
public_base_url = "https://joplin-mcp.lan"
lan_cidrs = ["192.168.0.0/16", "10.0.0.0/8"]

[joplin]
base_url = "https://joplin.lan"

[postgres]
dsn_file = "/run/credentials/joplin-mcpd.service/postgres-dsn"

[tokens]
hmac_key_file = "/run/credentials/joplin-mcpd.service/token-hmac-key"
default_scope = "read"
expires_after = null

[index]
refresh_interval_seconds = 60
full_rebuild_interval_hours = 24
max_parallel_users = 4

[logging]
service_name = "joplin-mcpd"
otlp_logs_endpoint = "http://victorialogs:9428/insert/opentelemetry/v1/logs"
otlp_protocol = "http/protobuf"
```

Do not store per-user Joplin passwords here.

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

On success, Joplin Server returns a session object. The MCP server may discard
the Joplin session after resolving the user ID, because normal reads use direct
Postgres access.

After Joplin auth succeeds, resolve the Joplin user row by email in Postgres.
Use the Joplin user ID as the stable identity key. Email is display data only.

If the email lookup fails after successful Joplin auth, fail the bootstrap and
log a sanitized error. Do not create a user by guessing.

## 10. Token Model

The token returned to the client is an MCP token.

Raw token format:

```text
mcp_<base64url random bytes>
```

Generate at least 32 random bytes before base64url encoding.

Never store the raw token. Store a keyed HMAC or hash.

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
  label text NOT NULL,
  scope text NOT NULL DEFAULT 'read',
  created_at timestamptz NOT NULL DEFAULT now(),
  last_seen_at timestamptz,
  revoked_at timestamptz,
  expires_at timestamptz
);

CREATE INDEX mcp_tokens_user_id_idx
  ON joplin_mcp.mcp_tokens(user_id);
```

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

## 12. Joplin Source Tables

The Joplin Server schema is internal. The implementation must validate the
actual schema at startup and fail early if required columns are missing.

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

Startup schema check:

```sql
SELECT column_name
FROM information_schema.columns
WHERE table_schema = current_schema()
  AND table_name = 'items'
ORDER BY column_name;
```

If content is not in `items.content` because Joplin is configured to use an
external content storage driver, v1 must fail early with a clear error. Add
external storage support only after direct DB indexing works.

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
1. upsert mcp_users
2. ensure index_state exists
3. if no ready index exists, start an async full build
4. return token immediately with index status
```

Normal refresh:

```text
1. find users with active tokens
2. refresh each active user every index.refresh_interval_seconds
3. process changed Joplin items since last_seen_joplin_updated_time
4. upsert derived rows
5. mark stale or deleted rows
6. update index_state
```

For v1, a full rebuild is acceptable:

```text
DELETE FROM joplin_mcp.notes_index WHERE user_id = $1;
DELETE FROM joplin_mcp.notebooks_index WHERE user_id = $1;
DELETE FROM joplin_mcp.tags_index WHERE user_id = $1;
DELETE FROM joplin_mcp.resources_index WHERE user_id = $1;
rebuild all rows for the user;
```

Use a transaction. If the rebuild fails, keep the previous ready index when
possible. Do not leave half-built visible state.

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
2. Find metadata start at the first line matching:
   ^id:\s+[0-9a-f]{32}$
3. The first non-empty line before metadata is title.
4. Body is everything after title and before metadata, trimmed of leading and
   trailing blank lines.
5. Metadata is key/value pairs after metadata start.
6. Trust DB jop_type and jop_encryption_applied more than parsed metadata.
7. If parser fails, log item ID and reason, skip the item, and continue.
```

Rust sketch:

```rust
struct ParsedItem {
    title: String,
    body: String,
    metadata: std::collections::HashMap<String, String>,
}

fn parse_joplin_item(raw: &str) -> anyhow::Result<ParsedItem> {
    let lines: Vec<&str> = raw.lines().collect();
    let id_re = regex::Regex::new(r"^id:\s+[0-9a-f]{32}$")?;

    let metadata_start = lines
        .iter()
        .position(|line| id_re.is_match(line.trim()))
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

## 17. MCP Tools

All tools are read-only.

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
  "limit": 50,
  "offset": 0
}
```

Returns title, ID, notebook, updated time, and short preview.

### `search_notes`

Input:

```json
{
  "query": "text",
  "limit": 20
}
```

Use Postgres full-text search:

```sql
SELECT joplin_id, title, ts_rank(search_vector, plainto_tsquery('simple', $2)) AS rank
FROM joplin_mcp.notes_index
WHERE user_id = $1
  AND search_vector @@ plainto_tsquery('simple', $2)
ORDER BY rank DESC, updated_time DESC
LIMIT $3;
```

Fallback to `ILIKE` only if full-text search gives no results and the query is
short enough.

### `get_note`

Input:

```json
{
  "note_id": "32-char Joplin ID"
}
```

Returns title, notebook, updated time, body, and resource reference count.

### `get_recent_notes`

Input:

```json
{
  "limit": 20
}
```

Returns notes ordered by `updated_time DESC`.

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

Email may be logged only as a hash or redacted display value.

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
skip encrypted item
extract resource references
deduplicate resource references
hash token and reject raw-token lookup
redact secrets in errors
validate Joplin IDs
```

### Database Tests

Use an isolated Postgres database.

Required tests:

```text
migrations apply cleanly
schema validation fails if required Joplin columns are absent
bootstrap upserts mcp_users
token lookup rejects revoked token
full index rebuild is transactional
notes_search_idx returns expected notes
owner-only filtering excludes other users
encrypted item filtering excludes encrypted items
```

### Server Integration Tests

Required tests:

```text
bootstrap success returns token and starts index
bootstrap failure rejects invalid Joplin credentials
token check accepts valid token
token check rejects unknown token
status tool returns read-only and unencrypted-only
search tool returns only current user's notes
get_note rejects another user's note ID
index failure leaves prior ready index in place
```

### Client Integration Tests

Required tests:

```text
bootstrap writes token mode 0600
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
6. query VictoriaLogs by test.id
7. fail if logs are missing
```

## 21. Fail-Early Rules

The server must refuse to start if:

```text
OTLP logging config is invalid
Postgres DSN is missing
Postgres connection fails
required joplin_mcp migrations are missing
Joplin source schema validation fails
token HMAC key is missing
listen address is public while LAN allow-list is disabled
```

The server may start degraded if Joplin Server auth endpoint is temporarily
unreachable, but `/readyz` must report not ready and bootstrap must fail with a
clear error.

The client must fail before serving if:

```text
token file is missing and bootstrap is disabled
token file permissions are broader than 0600
token check fails
server URL is missing
```

## 22. Security Requirements

### Token Storage

Client token path:

```text
/run/joplin-mcp-client/token
```

Permissions:

```text
/run/joplin-mcp-client      0700
/run/joplin-mcp-client/token 0600
```

Server stores only token hashes.

### Password Handling

Joplin passwords:

```text
accepted only during bootstrap/login
kept in memory only long enough to call Joplin Server
never logged
never stored
never written to panic messages
```

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

## 23. NixOS/LXC Deployment Shape

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

## 24. Implementation Modules

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

## 25. First Milestone

Milestone 1 is a local proof.

Deliver:

```text
joplin-mcpd starts
migrations create joplin_mcp schema
schema check validates fixture Joplin tables
bootstrap login against fake Joplin auth succeeds
MCP token is minted and stored hashed
indexer builds notes_index from fixture data
search_notes returns one fixture note
VictoriaLogs receives protobuf OTLP logs
test queries VictoriaLogs by test.id
```

No real Joplin Server required for milestone 1.

## 26. Second Milestone

Milestone 2 connects to the real Joplin Server database.

Deliver:

```text
read real users table
authenticate against real Joplin Server
resolve real joplin_user_id
build index for one user
compare one note against Joplin UI/API manually
verify encrypted items are skipped
verify another user is not visible
verify logs in VictoriaLogs
```

## 27. Third Milestone

Milestone 3 makes it usable from the LLM harness.

Deliver:

```text
joplin-mcp-client bootstrap
joplin-mcp-client serve
LLM harness config example
status tool
list_notebooks
list_notes
search_notes
get_note
logout
```

## 28. Acceptance Criteria

The project is acceptable when:

```text
unencrypted notes for the authenticated user can be searched and read
encrypted notes are skipped and reported as unsupported
another user's notes are not visible
Joplin passwords are never stored
raw MCP tokens are never stored
client token is stored under /run with mode 0600
server indexes are stored in joplin_mcp schema
all write tools are absent
VictoriaLogs receives OTLP/HTTP protobuf logs
tests pass only after querying VictoriaLogs by test.id
startup fails early on bad DB/schema/token/logging config
```

## 29. References

- Joplin Server MCP repo being replaced:
  `https://github.com/Alexander-Zhukov/joplin-server-mcp`
- Joplin Server items spec:
  `https://joplinapp.org/help/dev/spec/server_items/`
- Joplin Server delta sync spec:
  `https://joplinapp.org/help/dev/spec/server_delta_sync/`
- Joplin E2EE spec:
  `https://joplinapp.org/help/dev/spec/e2ee/`
- VictoriaLogs OpenTelemetry ingestion:
  `https://docs.victoriametrics.com/victorialogs/data-ingestion/opentelemetry/`
- VictoriaLogs query API:
  `https://docs.victoriametrics.com/victorialogs/querying/`
- OpenTelemetry OTLP specification:
  `https://opentelemetry.io/docs/specs/otlp/`

