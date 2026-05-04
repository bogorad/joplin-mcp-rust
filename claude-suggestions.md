# Suggestions for JOPLIN-MCP-RUST.md

A constructive critique of the JP-MCP plan with proposed enhancements. Section
numbers below refer to the original plan unless stated otherwise.

## 1. Summary

The plan is well-scoped and operationally minded. The hard limits (read-only,
owner-only, unencrypted-only, LAN-only) are sound. Observability as a hard
requirement is excellent.

The main weaknesses fall into four buckets:

1. **Direct Postgres reads on Joplin tables couple the MCP server to a
   third-party schema** that the Joplin team treats as internal.
2. **Authentication/security guidance is incomplete** in transport, key
   rotation, rate limiting, multi-user host paths, and audit trail.
3. **The MCP tool surface is too thin** for an LLM to do useful work without
   round-tripping. Filters, tags, and date ranges are missing.
4. **Doc hygiene gaps** — no glossary, no spec version, no change log, mixed
   normative levels (must/should/may), no JSON Schemas for tool inputs.

The rest of this document expands on each.

## 2. What's Working Well

Worth preserving as-is:

- Clear actor split (client / server / Joplin Server) with explicit
  responsibilities.
- Separate MCP token, hashed at rest, with revocation. Good token UX.
- Fail-early startup checks (DB, schema, auth endpoint, logging).
- Index in a dedicated `joplin_mcp` schema. No mutation of Joplin tables.
- Structured logging through OTLP, with log-shape rules and a redaction list.
- Test ID propagation (`X-Test-Id`) through to log queries.
- Milestone plan that ends with a measurable acceptance bar.

## 3. High-Impact Issues

### 3.1 Direct Postgres access vs. Joplin HTTP API

Section 12 acknowledges Joplin's schema is internal and adds a startup column
check. That helps, but the contract is still implicit. Joplin has changed
internal storage shapes before (notably the move to extracted `jop_*` columns)
and could change again. Each change becomes a silent indexer break until the
schema check fails.

The Joplin Server has an HTTP API with a delta sync endpoint that is part of a
documented spec (referenced in section 29). It is the contract Joplin clients
already depend on.

**Proposal:** add an `index.source` config knob with two backends:

```toml
[index]
source = "joplin_db"   # current plan
# source = "joplin_api"  # future: HTTP delta sync
```

Even if v1 ships only `joplin_db`, isolating the source behind a trait
(`trait JoplinSource`) lets you swap later without rewriting the indexer. This
also makes testing easier: a fake `JoplinSource` is cheaper than a fixture
Postgres.

### 3.2 Indexing strategy will not scale past a few users

Section 14 describes a poll loop:

- "find users with active tokens"
- "refresh each active user every `index.refresh_interval_seconds`"
- `max_parallel_users = 4`

For 4 users at 60s intervals, this is fine. For 50 users it is not, and there
is no explicit ceiling. Worse, full rebuilds (`DELETE … rebuild all rows`) are
listed as acceptable for v1, which means a single user with 100k notes blocks
a worker for the full rebuild duration.

**Proposals:**

1. Drive refresh from Joplin's `updated_time` cursor, not from a wall-clock
   poll. Section 11 already stores `last_seen_joplin_updated_time`. Use it.
2. Replace the per-user poll with a single global cursor query that fans out
   changed items to per-user upserts.
3. Cap rebuild concurrency by row volume, not by user count.
4. Document a target: e.g., "p99 incremental refresh under 5s for users with
   <10k notes" so the design has a measurable scaling bar.
5. Clarify whether the indexer is single-process or designed for HA. Two
   indexer instances racing on the same user will corrupt state.

### 3.3 Transport choice is ambiguous and possibly outdated

Section 8 lists `GET /sse` and `POST /message`. That is the legacy MCP HTTP+SSE
transport. Streamable HTTP (single endpoint, bidirectional) replaced it as the
recommended transport in 2025. The plan should pick one explicitly and pin a
spec version. Otherwise the implementer will pick the first thing the chosen
Rust crate supports, with no documented decision.

Also: the stdio→HTTP proxy in section 7 (`serve` mode) exists because some
harnesses only speak stdio. Many modern harnesses (Claude Desktop, OpenCode,
Cursor, etc.) speak HTTP MCP directly with a configured Bearer token. Document
when the proxy is actually needed; if rarely, demote it to a fallback and
make HTTP MCP the primary path.

### 3.4 Bootstrap UX has security and ergonomics issues

Section 6 sends Joplin email/password from the local CLI to the MCP server.
This works but has problems:

- The user types real Joplin credentials into a CLI binary. A trojan binary
  named `joplin-mcp-client` can exfiltrate them.
- There is no rate-limit on `POST /api/bootstrap/login` documented. Brute
  force is undefended.
- Login response shape should not differ between "no such email" and "bad
  password" — section 9 says "fail the bootstrap and log a sanitized error"
  but does not require uniform error responses.

**Proposals:**

1. Require uniform error response on bootstrap failure. Differentiate only in
   logs.
2. Add bootstrap-login rate limit (e.g., 5 attempts per IP per minute, 20
   per email per hour) with explicit config.
3. Add a web-flow-only mode where programmatic credential entry is disabled
   by config. Operators who want defense-in-depth can require humans to log
   in via the browser and copy a token.
4. Document a credential-bootstrap audit trail (who, when, from where, with
   what label).

### 3.5 No transport encryption guidance

"LAN only" is not a security control. A switch-port mirror, a compromised
device on the same VLAN, or a rogue WiFi AP defeats it.

**Proposal:** add a section 22 subsection on TLS:

- The server must serve over TLS. Self-signed with cert pinning is acceptable
  for LAN deployments; document `--server-fingerprint` on the client.
- Plaintext HTTP must be allowed only behind an explicit
  `--insecure-no-tls` flag, refused unless the server listens on `127.0.0.1`.
- Document interaction with the assumed reverse proxy (Caddy, nginx, etc.).

### 3.6 Token model gaps

Section 10 is good as far as it goes. Missing pieces:

- **Default expiry is `null` (forever).** This is the wrong default. Pick a
  reasonable default like 90 days, with explicit non-expiry behind a flag.
- **No HMAC key rotation strategy.** If `token_hmac_key_file` ever leaks,
  every stored hash is compromised. Plan for keyed-hash rotation: store
  `hmac_key_id` per token row; accept old keys during a rotation window;
  reject after.
- **No constant-time comparison.** DB lookup avoids the timing-attack issue
  for hash comparison naturally, but any in-memory equality on token bytes
  must use `subtle::ConstantTimeEq`. State this explicitly.
- **Token format example missing.** "at least 32 random bytes before
  base64url encoding" is correct but readers benefit from an example
  (`mcp_aBc...XyZ` with the actual length).
- **No nonce / replay window.** For a Bearer token over TLS this is fine,
  but state it explicitly so reviewers do not flag it later.
- **Token revocation lacks audit fields.** Add `revoked_by`, `revoke_reason`,
  `revoked_from_ip` columns.

### 3.7 Multi-user host path is shared

`/run/joplin-mcp-client/token` is a single global path. On a multi-user host,
two users on the same machine share or clobber each other's token.

**Proposal:** default to `${XDG_RUNTIME_DIR}/joplin-mcp-client/token`
(typically `/run/user/<uid>/joplin-mcp-client/token`). Fall back to
`/run/joplin-mcp-client/token` only when invoked by a system service with a
fixed user. Update section 22 accordingly.

## 4. Data Model and Indexer

### 4.1 Body duplication is a storage cost

`notes_index.body_text` stores the entire note body. For a user with 50 MB of
notes, the MCP server doubles their storage footprint inside Postgres (Joplin
already stores it in `items.content`). This is fine for v1 but should be
flagged as a known trade-off, with a future option to keep only `tsvector`
and snippets and resolve full body on demand from Joplin.

### 4.2 Full-text search uses `'simple'` config

Section 11 uses `to_tsvector('simple', ...)`. This means no stemming, no stop
words, no case folding beyond default. For English notes this matters: a
search for "running" will not match "ran". Add a config knob:

```toml
[index]
text_search_config = "simple"   # or "english", or per-user
```

For multilingual users, consider `pg_trgm` GIN indexes on title/body in
addition to `tsvector` for substring-style matches.

### 4.3 Note↔tag relationship is missing

Section 13 lists Joplin type 6 (`note_tag`) but section 11 has no
`note_tags_index` table. Without it, `get_notes_by_tag` and tag filtering in
search are impossible.

**Proposal:** add

```sql
CREATE TABLE joplin_mcp.note_tags_index (
  user_id uuid NOT NULL REFERENCES joplin_mcp.mcp_users(id) ON DELETE CASCADE,
  note_joplin_id text NOT NULL,
  tag_joplin_id text NOT NULL,
  PRIMARY KEY (user_id, note_joplin_id, tag_joplin_id)
);

CREATE INDEX note_tags_by_tag_idx
  ON joplin_mcp.note_tags_index(user_id, tag_joplin_id);
```

### 4.4 Resource references stored as `text[]`

`resource_refs text[]` is a quick win but blocks "which notes reference this
resource?" queries. Acceptable for v1 if downgraded to a soft requirement,
but document the limitation. Alternative: `note_resources_index` join table.

### 4.5 Soft-delete behavior is unspecified

Joplin uses `deleted_time` for soft deletes. The schema includes the column
but section 14 does not say whether deleted items are filtered, returned with
a flag, or pruned. Pick one and document it. The MCP tool surface should not
silently include deleted notes in search results.

### 4.6 Schema validation is too narrow

Section 12's startup check selects `column_name` only. It does not check
column types. If Joplin changes `items.content` from `text` to `bytea` (or
vice versa), the indexer will fail at runtime, not at startup.

**Proposal:** check `(column_name, data_type)` pairs against an explicit
expected set, and fail startup if any required column has an unexpected type.

### 4.7 Migration framework not specified

"migrations apply cleanly" is mentioned in section 20 but the framework is
not. State that `sqlx migrate` is the canonical mechanism, that migrations
are embedded in the binary at compile time, and that down-migrations are not
supported in v1.

### 4.8 No connection pool sizing guidance

Add to section 8 config:

```toml
[postgres]
max_connections = 16
acquire_timeout_seconds = 5
```

## 5. MCP Tool Surface

### 5.1 The toolset is too thin for real LLM use

An LLM doing useful note work needs filters and aggregations. Current tools
force the LLM to fetch everything and filter client-side, which wastes
context.

**Add or extend:**

- `search_notes` — add `notebook_id`, `tag_ids`, `updated_after`,
  `updated_before`, `is_todo`, `limit_body_chars` filters.
- `list_tags` — flat list of tags with note counts.
- `get_notes_by_tag` — by tag ID.
- `get_notebook_tree` — hierarchy, not flat list.
- `get_changes_since` — `{since: timestamp}` returns notes changed since.
  This is the killer feature for an LLM that runs across sessions.
- `get_note_excerpt` — title plus first N chars, for cheap browsing.

### 5.2 No JSON Schemas for tool inputs

Section 17 shows input shapes as JSON snippets. MCP tools advertise JSON
Schema in `tools/list`. The plan should include the actual schemas, even as
sketches:

```json
{
  "name": "search_notes",
  "inputSchema": {
    "type": "object",
    "properties": {
      "query": {"type": "string", "minLength": 1, "maxLength": 1024},
      "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20}
    },
    "required": ["query"]
  }
}
```

This also forces decisions about field validation that the current text
glosses over (e.g., max query length).

### 5.3 No response size budget

`get_note` returns the full body. A 200 KB note will blow most LLM context
windows for marginal value. Add:

```toml
[mcp]
max_response_bytes = 65536
default_body_truncate_chars = 8000
```

When a body is truncated, return a flag and a way to fetch the remainder.

### 5.4 Pagination is offset-based

`limit + offset` does not survive concurrent index updates. Use cursor-based
pagination for `list_notes`, `search_notes`, `get_recent_notes`. The cursor
can be the last seen `(updated_time, joplin_id)` tuple.

### 5.5 No uniform error shape

The plan does not specify what an MCP tool returns on error: structured MCP
error vs. JSON `{"error": "..."}` payload. Pick one and document.

## 6. Observability

### 6.1 Logs only, no metrics or traces

The plan covers OTLP logs in detail. It says nothing about metrics or traces.
For a service with database calls, indexer jobs, and external auth, you want
all three.

**Proposal:**

- Metrics: index lag (seconds since last refresh per user), tool latency
  (histogram by tool name), bootstrap login outcomes (counter), MCP request
  rate.
- Traces: spans for `bootstrap → joplin auth → user upsert`,
  `indexer refresh → query Joplin → upsert N rows`, `tool call → DB query →
  serialize`.

VictoriaMetrics + VictoriaLogs + VictoriaTraces is a coherent stack and
already in the operator's environment per the references.

### 6.2 Email handling rule is ambiguous

Section 18 says email "may be logged only as a hash or redacted display
value." Hashing emails is not a useful operational pattern (you cannot
reverse-search by email). Pick one:

- Redacted: `c***@example.com` (operator can recognize accounts).
- Hashed with a per-deployment salt (privacy-first; ops uses email→hash
  lookup tool).

State which and why.

### 6.3 No alerting hooks

The plan should name the alerts that matter:

- `index.status = failed` for any user.
- Repeated `outcome = error` on bootstrap (5 in 5min).
- VictoriaLogs ingestion failure.
- DB connection pool saturation.

Even if you don't ship Prometheus rules, list the conditions.

### 6.4 Audit log is missing

Section 18 covers operational logs. It does not cover an audit trail. Add
a `joplin_mcp.audit_log` table for security-relevant events (bootstrap
success/failure, token mint, token revoke, schema validation failure). This
is operational, not just diagnostic — you want it queryable independent of
log retention policy.

## 7. Testing

The test list in section 20 is solid but missing:

- **Property-based tests** for `parse_joplin_item`. The parser handles
  user-controlled content (note bodies). Use `proptest` for fuzzing-style
  coverage of malformed input.
- **Fuzz test** for resource reference extraction. The regex patterns can
  match aggressively in markdown code blocks.
- **Concurrency test** for indexer: two refresh jobs targeting the same
  user must not corrupt state. Test with `tokio::test` and explicit
  scheduling.
- **Multi-tenant isolation test**: verify that `user_id` predicates are on
  every query. A linter rule or `sqlx::query!` macro discipline helps, but
  a runtime test catches regressions.
- **Token brute-force test**: verify rate limiting actually fires.
- **Large-note test**: 10 MB note body — does the indexer OOM, truncate,
  or skip?
- **`testcontainers` for Postgres**: cleaner than spinning up a fixture DB
  manually.

## 8. Operational Concerns

### 8.1 The `joplin_mcp` schema is rebuildable state

State this explicitly in section 23 or a new operations section. If yes,
backup of `joplin_mcp` is unnecessary; loss recovery = re-bootstrap each
user. This shapes the disaster recovery story.

If no — for example, because token state would be lost — document that and
include a backup recommendation.

### 8.2 No multi-tenant resource limits

A single user with a runaway query (huge `query` body, large limit) can
slow other users. Add per-user rate limits or query-cost caps.

### 8.3 No upgrade story for breaking changes

What happens at v2 if the token format or schema changes? Document:

- Token compatibility window (old tokens accepted for 30 days post-deploy).
- Schema version pin in `joplin_mcp.schema_version` row.
- Refusal to start if `schema_version` is newer than the binary expects.

### 8.4 `/readyz` is binary

Add per-user index status to a separate `/api/index/status?user_id=...`
endpoint, gated by token. Operators want to see "user X has been stuck
building for 30 minutes" without log diving.

## 9. Code-Level Notes

### 9.1 Regex compilation in `parse_joplin_item`

Section 15 builds the regex inside the function. Move to a `OnceLock` or
`once_cell::sync::Lazy`:

```rust
static ID_RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
    regex::Regex::new(r"^id:\s+[0-9a-f]{32}$").expect("valid regex")
});
```

### 9.2 `anyhow::Result` in library code

`anyhow` is fine in `main.rs`. In library modules (`auth`, `indexer`, `mcp`),
prefer `thiserror` enums so callers can match on error kinds and so logging
can attach `error.kind` reliably. Section 18's required field `error.kind`
is awkward to populate from opaque `anyhow::Error`.

### 9.3 Token comparison

Even with DB lookup avoiding timing attacks, document that any in-memory
byte comparison of token material uses `subtle::ConstantTimeEq`. Make this
explicit so the implementer doesn't reach for `==`.

### 9.4 Joplin metadata parsing edge cases

`split_once(':')` in section 15 will mis-parse values that contain colons
(URLs, timestamps in some formats). Use `splitn(2, ':')` and trim. Add a
test for `updated_time: 2026-05-04T10:00:00Z` — that splits correctly with
`split_once`, but a value like `body_url: https://example.com/x` would
misparse if any line happens to land in the metadata block.

Also: Joplin metadata blocks include lines like `markup_language: 1` and
`source_url:` (empty). Test both.

### 9.5 Note body containing fake metadata

Markdown code blocks can contain text matching `^id:\s+[0-9a-f]{32}$`. The
regex requires exactly 32 hex chars, which makes false positives unlikely
but not impossible. Add a parser test for this case. A more defensive parser
would scan from the bottom of the file upward (metadata is always at the
end of a Joplin item).

## 10. Documentation Hygiene

### 10.1 Add a glossary

The doc uses "MCP token", "Joplin token", "session token", "bootstrap
credential", "Joplin user ID", "MCP user ID", "joplin_id", "joplin_item_id"
without a consolidated definition. Add a glossary at section 3.5.

### 10.2 Use RFC 2119 normative levels

The doc mixes "must", "should", "must not", "do not" without a stated
hierarchy. Add a one-line note ("MUST/SHOULD/MAY follow RFC 2119") at the
top of section 1, or downgrade strict-sounding language where it isn't
strict.

### 10.3 No spec version or change log

Add to top of file:

```text
Version: 0.1
Last updated: 2026-05-04
Status: Draft
```

And a `## Change Log` section at the end.

### 10.4 References should pin commits/versions

Section 29 links to Joplin spec pages. Joplin updates these. Capture either
a Wayback URL or a "fetched on 2026-05-04" annotation.

### 10.5 Acceptance criteria are partially measurable

Section 28 mixes verifiable claims ("VictoriaLogs receives OTLP/HTTP
protobuf logs") with claims that need a code review ("raw MCP tokens are
never stored"). For the latter, define how it's verified: e.g., "grep for
`raw_token` writes in DB code returns no hits", or "audit checklist signed
off by two reviewers".

### 10.6 Non-goals section

Add an explicit non-goals section after section 2:

- E2EE decryption — never.
- Writes — deferred to v2 (or never; clarify).
- Shared notebooks — deferred to v2.
- Resource binary download — deferred.
- Multi-Joplin-server federation — never.

## 11. Open Questions for the Author

These need a decision before implementation:

1. **MCP transport version:** legacy SSE or Streamable HTTP? Pin the spec
   version.
2. **Postgres source vs. Joplin HTTP API:** is the direct-DB approach a
   permanent decision or a v1 expedient?
3. **Indexer: single-process or HA-capable?** Affects locking design.
4. **Default token expiry:** never, 30d, 90d, configurable?
5. **Default text search config:** `simple`, `english`, or per-user?
6. **Soft-deleted notes:** filter, flag, or include?
7. **Email logging:** redacted display or salted hash?
8. **Are users running this on multi-user hosts?** Affects token path
   design.
9. **Is `/run/joplin-mcp-client/` written by `joplin-mcp-client` itself
   (requires elevation) or pre-created by systemd?**
10. **Does the LLM harness have a credential-injection trust boundary the
    MCP server needs to enforce?** (Prompt-injection from note bodies into
    tool-using LLMs is a known class.)

## 12. Suggested Reordering

The current section order is roughly: scope → actors → flow → auth → DB →
parser → tools → observability → tests → ops. That works.

A small improvement: move section 22 (Security Requirements) up to right
after section 5 (Authentication Model), since several earlier sections
depend on security decisions (token storage, password handling). The current
ordering forces the reader to flip back and forth.

## 13. Priority Summary

If only a few changes are made, prioritize in this order:

1. **Add TLS guidance** (section 3.5 above). Without it, "LAN only" is
   weaker than the doc implies.
2. **Set a non-null default token expiry** (section 3.6).
3. **Tighten schema validation to include column types** (section 4.6).
4. **Add `note_tags_index` and at least `list_tags` / `get_notes_by_tag`
   tools** (sections 4.3, 5.1).
5. **Decide and document MCP transport version** (section 3.3).
6. **Add metrics and traces, not just logs** (section 6.1).
7. **Add a glossary, version, and non-goals section** (sections 10.1, 10.3,
   10.6). Cheapest, highest legibility win.
8. **Move client token to `${XDG_RUNTIME_DIR}`** (section 3.7).

Everything else is incremental polish.
