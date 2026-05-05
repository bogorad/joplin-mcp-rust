## 🔴 High Severity: Data Consistency

### 1. Hard-Deleted Note-Tag Edges Leak (Hallucination Risk)

In `crates/joplin-mcpd/src/indexer/refresh.rs`, the `reconcile_hard_deletes` function is responsible for catching items that were permanently deleted from the Joplin database (since they won't appear in the `changed_items_since` delta feed).

- **The Problem:** `reconcile_hard_deletes` compares `indexed_item_refs` against `active_item_refs` fetched from the source. However, in `crates/joplin-mcpd/src/indexer/source.rs`, the `ACTIVE_ITEM_REFS_QUERY` explicitly filters `jop_type IN (1, 2, 5, 9)`. It intentionally omits `6` (NoteTag).
- **The Impact:** If a user removes a tag from a note in Joplin, the `note_tags` row is hard-deleted. Because it is hard-deleted, it doesn't appear in the incremental delta. Because type `6` is excluded from reconciliation, the indexer never realizes it was deleted. The edge remains in `joplin_mcp.note_tags_index` permanently (until a 24-hour full rebuild occurs). The LLM will hallucinate that notes have tags they no longer have.
- **Remediation:** Add `6` to the `IN` clause of `ACTIVE_ITEM_REFS_QUERY`, and ensure `indexed_item_refs` also selects `item_type = 6` from `joplin_mcp.note_tags_index`.

---

## 🟠 Medium Severity: Security & Logic Flaws

### 1. IP Spoofing in Rate Limiter via `X-Forwarded-For`

In `crates/joplin-mcpd/src/lifecycle.rs`, the `extract_client_ip` function parses the `forwarded_header` (e.g., `X-Forwarded-For`) when the request comes from a trusted proxy:

```rust
fn parse_forwarded_ip(value: &HeaderValue) -> Option<IpAddr> {
    let value = value.to_str().ok()?;
    let first = value.split(',').next()?.trim();
    first.parse().ok()
}
```

- **The Problem:** It takes the _first_ (left-most) IP address in the list. Standard HTTP proxies _append_ the connecting IP to the existing header. If a malicious client sends `X-Forwarded-For: 1.2.3.4`, the trusted proxy appends the real IP, resulting in `1.2.3.4, 9.9.9.9`. The code grabs `1.2.3.4`.
- **The Impact:** An attacker can completely bypass the IP-based bootstrap rate limiter by randomizing the `X-Forwarded-For` header on every brute-force attempt.
- **Remediation:** When behind a trusted proxy, you should extract the _last_ untrusted IP. If you only have one layer of trusted proxies, this is simply the right-most IP in the list: `value.split(',').last()?.trim()`.

### 2. Same-Millisecond Data Loss in `get_changes_since`

In `crates/joplin-mcpd/src/mcp/tools.rs`, the `CHANGES_SINCE_QUERY` uses a strict greater-than filter for the base watermark:

```sql
WHERE notes.updated_time > $2
```

- **The Problem:** If an LLM syncs up to timestamp `T`, and later asks for changes `since: T`, it will permanently miss any _other_ items that were also updated at exactly millisecond `T` but weren't returned in the previous sync (e.g., due to pagination limits, or being committed to the DB just after the read).
- **Observation:** You correctly identified and fixed this exact "same-millisecond watermark" bug in the _indexer_ (`jmr-h6a.1`) by using `>= $2` for the base query and relying on the keyset cursor (`OR (updated_time = $3 AND id > $4)`) to prevent duplicates.
- **Remediation:** Apply the same logic to `CHANGES_SINCE_QUERY`. Change `> $2` to `>= $2`. If the client provides a cursor, the keyset pagination will naturally skip the duplicates. If they don't provide a cursor, returning items at exactly `T` is safer than dropping them.

---

## 🟡 Low Severity: Dead Code & Minor Risks

### 1. Dead Code: Token `last_seen_at` is Never Updated

In `crates/joplin-mcpd/src/auth/tokens.rs`, there is a well-tested function `touch_last_seen_if_stale` designed to update a token's `last_seen_at` timestamp at most once every 5 minutes.

- **The Problem:** This function is never called anywhere in the codebase. `McpAuth::authenticate` calls `authenticate_bearer`, which validates the token but never touches the `last_seen_at` column.
- **Remediation:** Spawn a detached Tokio task inside `McpAuth::authenticate` (or `mcp_post`) to call `touch_last_seen_if_stale` upon successful authentication, allowing operators to audit inactive tokens.

### 2. Unbounded Memory Allocation in Stdio Proxy

In `crates/joplin-mcp-client/src/proxy.rs`, the client reads JSON-RPC frames from the LLM harness over standard input:

```rust
let Some(content_length) = content_length else { ... };
let mut body = vec![0_u8; content_length];
```

- **The Problem:** It allocates a `Vec` directly based on the `Content-Length` header provided by the local process. If a buggy or malicious local harness sends `Content-Length: 4000000000`, the client will attempt to allocate 4GB of RAM and likely panic/OOM.
- **Remediation:** Impose a reasonable maximum frame size (e.g., `100 * 1024 * 1024` for 100MB) and return an error if `content_length` exceeds it before allocating.

---
