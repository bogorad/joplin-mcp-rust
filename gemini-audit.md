Here is a comprehensive code audit of the `joplin-mcp-rust` codebase. The audit is broken down by severity, covering architectural flaws, resource leaks, dead config, inconsistencies, and code hygiene.

---

## 🟢 Executive Summary

The codebase is highly disciplined, strictly adheres to its planned architectural boundaries (as defined in `JOPLIN-MCP-RUST.md`), and exhibits excellent use of Rust's type system. The separation of canonical Joplin data from the derived `joplin_mcp` schema is strictly enforced, and observability (VictoriaLogs/OTLP) is integrated well.

However, there are a few **critical implementation flaws** regarding Postgres connection pooling (advisory locks) and memory management (rate limiting), as well as some dead configuration and swallowed metrics.

---

## 🔴 High Severity: Bugs & Stability Risks

### 1. Broken Process-Level Singleton Lock (`db/lifecycle.rs`)

The `SingletonLock` implementation attempts to ensure only one instance of `joplin-mcpd` runs at a time. It is fundamentally broken due to how Postgres session-level advisory locks interact with connection pools.

```rust
pub async fn acquire(pool: &sqlx::PgPool) -> anyhow::Result<Self> {
    let acquired = sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1)")
        .bind(SINGLETON_LOCK_KEY)
        .fetch_one(pool)
        .await?;
// ...
```

- **The Problem:** `fetch_one(pool)` checks out a random connection, executes the lock function, and immediately returns the connection to the pool. The lock is tied to that specific database connection's PID.
  - Other queries using that specific connection will hold the lock.
  - Other instances of the app connecting to Postgres will be blocked _only as long as that specific connection is kept alive by the pool_. If the pool idles out and closes the connection, the lock is silently released, allowing a second instance to start.
  - Furthermore, the `Drop` implementation spawns a detached Tokio task to unlock it, which will likely grab a _different_ connection from the pool, failing to release the original lock.
- **Remediation:** To use session-level advisory locks, you must acquire a dedicated connection (`let mut conn = pool.acquire().await?`), execute the lock, and **hold onto that connection** for the lifetime of the application.

### 2. Unbounded Memory Leak in Rate Limiter (`http.rs`)

The `BootstrapRateLimiter` uses `BTreeMap<String, WindowCounter>` to track requests per IP and per email.

```rust
struct BootstrapRateLimitState {
    per_ip: BTreeMap<String, WindowCounter>,
    per_email: BTreeMap<String, WindowCounter>,
}
```

- **The Problem:** There is no eviction mechanism for old keys. Every unique IP address or email address that attempts a login will permanently add an entry to these maps. Over time, or during a brute-force / DoS attack, this will cause an Out-Of-Memory (OOM) crash.
- **Remediation:** Replace `BTreeMap` with an LRU cache (e.g., using the `moka` or `lru` crates), or implement a periodic pruning task that removes entries where `now.duration_since(window_start) >= window`.

---

## 🟠 Medium Severity: Dead Config & Logic Flaws

### 1. Dead Configuration: `statement_timeout_seconds` (`config.rs`, `main.rs`)

The `PostgresConfig` defines `statement_timeout_seconds`, and the plan mandates its use to prevent hanging queries.

- **The Problem:** The configuration is loaded, validated, and never applied to the Postgres connection. `PgPoolOptions::new()` handles `acquire_timeout` and `max_connections`, but it does not natively map a statement timeout.
- **Remediation:** You must explicitly pass it to the connection options before building the pool:
  ```rust
  let options = PgConnectOptions::from_str(dsn.trim())?
      .options([("statement_timeout", format!("{}", config.postgres.statement_timeout_seconds * 1000))]);
  let pool = PgPoolOptions::new()...connect_with(options).await?;
  ```

### 2. Discarded Full Rebuild Metrics (`indexer/refresh.rs`)

When the incremental lookback cap is exceeded, `incremental_refresh_user` escalates to a full rebuild.

```rust
if let Err(error) = full_rebuild_user(mcp_pool, source, user.mcp_user_id, &user.joplin_user_id).await {
    // ... handles error
}
// ... returns dummy incremental outcome
return Ok(IncrementalRefreshOutcome {
    lock_acquired: true,
    full_rebuild_required: true,
    changed_items: 0, // <--- Data is lost
// ...
```

- **The Problem:** `full_rebuild_user` returns a rich `FullRebuildOutcome` containing the exact counts of indexed notes, skipped encrypted files, etc. Because `incremental_refresh_user` ignores the `Ok(outcome)`, these statistics are completely swallowed and never logged or sent to metrics.
- **Remediation:** Map the `FullRebuildOutcome` into the `IncrementalRefreshOutcome` so the worker cycle can aggregate and log the actual work performed during the escalation.

### 3. Spawning Detached Tasks in `Drop` (`db/lifecycle.rs`)

- **The Problem:**
  ```rust
  impl Drop for SingletonLock {
      fn drop(&mut self) {
          tokio::spawn(async move { /* unlock */ });
      }
  }
  ```
  Spawning a Tokio task inside `Drop` can panic if the Tokio runtime is in the process of shutting down (which is exactly when `Drop` is called during graceful shutdown).
- **Remediation:** Since Postgres automatically releases session locks when the connection drops, if you fix the lock issue (High #1) by holding a dedicated `sqlx::pool::PoolConnection`, simply dropping the connection will safely and automatically release the lock.

---

## 🟡 Low Severity: Dead Code & Inconsistencies

### 1. Unused Struct Fields in Refresh Outcome (`indexer/refresh.rs`)

`IncrementalRefreshOutcome` tracks `skipped_encrypted`, `skipped_malformed`, and `skipped_wrong_owner`.

- **The Problem:** These fields are carefully calculated in `build_incremental_rows`, passed up to the outcome, but the caller (`run_index_refresh_cycle` in `worker.rs`) entirely ignores them. They are never recorded in `WorkerCycleStats` or emitted to VictoriaLogs.
- **Remediation:** Add these to `WorkerCycleStats` and emit them in `stats.record_metrics()` and `stats.log_finished()`.

### 2. File Permission Validation Edge Case (`joplin-mcp-client/src/token_file.rs`)

```rust
let dir_mode = fs::metadata(dir)?.permissions().mode() & 0o777;
if dir_mode != 0o700 { bail!("..."); }
```

- **The Problem:** Using exact equality (`!= 0o700`) is excessively strict. If a user's `XDG_RUNTIME_DIR` (e.g., `/run/user/1000`) has permissions of `0o700` but includes a sticky bit, SetUID, or SetGID bit (which are above `0o777` mask), masking it with `0o777` handles it, but if they have `0o711` on the parent dir it fails. It's safer to ensure _group and other_ have zero permissions: `(dir_mode & 0o077) == 0`.
- _Note: As implemented, it strictly follows the design doc. Mentioning purely as a UX edge-case._

### 3. Keyset Pagination Edge Case (`mcp/tools.rs`)

In `fetch_search_note_rows`, the keyset pagination fallback checks:

```sql
OR (0::real = $9 AND COALESCE(notes.updated_time, 0) < $10)
OR (0::real = $9 AND COALESCE(notes.updated_time, 0) = $10 AND notes.joplin_id < $11)
```

- **Observation:** Because you are sorting `DESC` for `updated_time` and `DESC` for `joplin_id`, using `<` for the cursor pagination is perfectly correct. Excellent attention to detail here, as this is a common trap.

### 4. Over-fetching in Memory (`indexer/refresh.rs` / `source.rs`)

- **Observation:** `changed_items_since` loads all changed items at once into a `Vec<JoplinItem>` using `fetch_all`. This includes the raw markdown `content` (`bytea`). If a user has a massive sync of heavily-laden notes, this will spike RAM.
- **Remediation:** For v1, this is acceptable. For v2, consider using `fetch` (streaming) to process and insert rows in batches rather than holding the entire delta payload in memory.

---

## 🟢 Commendations (What went exceptionally well)

1. **Strict Plan Adherence:** The codebase enforces the exact non-goals of the spec. `source.rs` strictly forces `WHERE owner_id = $1` and `encrypted` flags are properly filtered out. It correctly ignores share tables.
2. **Robust Content Truncation:** `truncate_text_at_chars` correctly handles UTF-8 boundaries using `char_indices()`. It prevents slicing strings mid-character which would result in Rust panics.
3. **Signed Cursors:** The implementation of opaque, signed cursors (`encode_cursor`) prevents users from maliciously altering pagination state, tying the cursor securely to the `filter_hash`.
4. **Structured Auditing:** Redaction logic in `audit.rs` and `logging.rs` is thorough, ensuring bearer tokens, passwords, and note bodies are scrubbed from OpenTelemetry outputs and DB audit logs before emission.
