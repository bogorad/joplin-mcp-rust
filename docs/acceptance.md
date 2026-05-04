# JP-MCP v1 Acceptance Gate

This gate is for `jmr-gyb.5.7`. Do not run or close it until `jmr-gyb.5.5`
is closed. The default local gate must not decrypt `secrets.yaml` and must not
require a real Joplin Server.

## Command Gate

Run these commands from the repository root inside `nix develop`.

| Gate | Command | Evidence to capture |
| --- | --- | --- |
| Rust formatting | `cargo fmt --check` | Command exits 0 with no diff required. |
| Rust lint | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | Command exits 0 with no warnings. |
| Rust build | `cargo build --workspace` | Workspace builds all crates. |
| Rust tests | `cargo test --workspace` | Local non-secret tests pass. |
| Nix checks | `nix flake check` | Flake checks pass, including formatter/package/module checks. |
| Server package | `nix build .#joplin-mcpd` | Build exits 0 and creates a result for `joplin-mcpd`. |
| Client package | `nix build .#joplin-mcp-client` | Build exits 0 and creates a result for `joplin-mcp-client`. |
| Local acceptance | `just test-all-local` | Runs local non-secret checks without decrypting `secrets.yaml`. |
| Live Joplin acceptance | `JP_MCP_LIVE_JOPLIN=1 just test-real-joplin` | Opt-in only. Uses SOPS test secrets, including `victorialogs_url`, without printing values. |

`just test-all-local` is the default acceptance command. It must not run
`sops -d secrets.yaml`, must not print decrypted secret values, and must not
require the real Joplin Server.

`just test-real-joplin` is the only required command that may decrypt
`secrets.yaml`. It must fail before running unless `JP_MCP_LIVE_JOPLIN=1` is
set.

If local DNS cannot resolve the SOPS `postgres.host` name, use
`JP_MCP_LIVE_POSTGRES_HOST=<reachable-host-or-ip>` with `just test-real-joplin`.
The override changes only the network address used for the live test; database
names, users, and passwords still come from SOPS.

## Service And Environment Evidence

| Criterion | Command or file | Evidence to capture |
| --- | --- | --- |
| Stable local service runner exists | `tests/compose.local.yaml` | File defines disposable `postgres`, `fake-joplin-auth`, and `victorialogs` services. |
| Local Postgres is disposable | `tests/compose.local.yaml` | Service `postgres` uses fixture credentials and binds only to `127.0.0.1:55432`. |
| Local fake Joplin auth exists | `tests/compose.local.yaml` | Service `fake-joplin-auth` is present and binds only to `127.0.0.1:58080`. |
| Local VictoriaLogs exists | `tests/compose.local.yaml` | Service `victorialogs` exposes container port `9428` on `127.0.0.1:59428`. |
| Live VictoriaLogs endpoint is configured | `JP_MCP_LIVE_JOPLIN=1 just test-real-joplin` | Live SOPS `victorialogs_url` exists and builds valid VictoriaLogs query and OTLP endpoints. |
| Live OTLP insert endpoint is fixed | `rg -n "/insert/opentelemetry/v1/logs" flake.nix crates JOPLIN-MCP-RUST.md` | Live logs are sent to `http://victorialogs.lan:9428/insert/opentelemetry/v1/logs`. |
| Live query endpoint is fixed | `rg -n "/select/logsql/query" crates JOPLIN-MCP-RUST.md` | Live tests query `http://victorialogs.lan:9428/select/logsql/query`. |
| Joplin and MCP databases are separate | `JP_MCP_LIVE_JOPLIN=1 just test-real-joplin` | Test gate proves `postgres.joplin_database != postgres.mcp_database` before live tests run. |
| Joplin database role is read-only | `rg -n "joplin_database|joplin_user" JOPLIN-MCP-RUST.md docs crates` | Evidence shows canonical Joplin reads use `postgres.joplin_database` with `postgres.joplin_user`. |
| MCP database role is separate | `rg -n "mcp_database|mcp_user" JOPLIN-MCP-RUST.md docs crates` | Evidence shows `joplin_mcp` schema, migrations, tokens, audit log, and indexes use `postgres.mcp_database` with `postgres.mcp_user`. |

Do not capture decrypted secret values, DSNs, passwords, raw MCP tokens, token
hashes, note bodies, full auth headers, or generated configs containing them.

## Milestone Checklist

| Milestone | Criterion | Verification |
| --- | --- | --- |
| 1 local proof | `joplin-mcpd` starts | `just test-all-local`; evidence from server or e2e test output. |
| 1 local proof | migrations create `joplin_mcp` schema | `cargo test --workspace`; migration tests or local DB tests pass. |
| 1 local proof | schema check validates fixture Joplin tables | `cargo test --workspace`; schema check fixture test passes. |
| 1 local proof | bootstrap login against fake Joplin auth succeeds | `just test-all-local`; local e2e uses `tests/compose.local.yaml`. |
| 1 local proof | MCP token is minted and stored hashed | `cargo test --workspace`; token tests reject raw-token lookup/storage. |
| 1 local proof | HMAC key file format is validated | `cargo test --workspace`; bad key config test fails early. |
| 1 local proof | indexer builds `notes_index` from fixture data | `cargo test --workspace`; index fixture test passes. |
| 1 local proof | `search_notes` returns one fixture note | `just test-all-local`; MCP tool or e2e assertion passes. |
| 1 local proof | `list_tags` returns one fixture tag | `just test-all-local`; MCP tool or e2e assertion passes. |
| 1 local proof | VictoriaLogs receives protobuf OTLP logs | `just test-all-local`; tests poll local VictoriaLogs by `test.id`. |
| 1 local proof | test polls VictoriaLogs by `test.id` | `just test-all-local`; missing-log case fails and expected-log case passes. |
| 2 real Joplin | read real users table | `JP_MCP_LIVE_JOPLIN=1 just test-real-joplin`; live evidence is opt-in. |
| 2 real Joplin | authenticate against real Joplin Server | `JP_MCP_LIVE_JOPLIN=1 just test-real-joplin`; no credentials printed. |
| 2 real Joplin | load real test credentials without logging them | `JP_MCP_LIVE_JOPLIN=1 just test-real-joplin`; captured output contains no secret values, DSNs, tokens, or generated configs. |
| 2 real Joplin | use separate Joplin and MCP databases | `JP_MCP_LIVE_JOPLIN=1 just test-real-joplin`; gate fails if `postgres.joplin_database == postgres.mcp_database`. |
| 2 real Joplin | resolve real `joplin_user_id` | `JP_MCP_LIVE_JOPLIN=1 just test-real-joplin`; live identity assertion passes. |
| 2 real Joplin | changed Joplin email refreshes `mcp_users` | `JP_MCP_LIVE_JOPLIN=1 just test-real-joplin`; live or documented manual evidence. |
| 2 real Joplin | temporary Joplin session is invalidated after bootstrap | `JP_MCP_LIVE_JOPLIN=1 just test-real-joplin`; live assertion passes. |
| 2 real Joplin | build index for one user | `JP_MCP_LIVE_JOPLIN=1 just test-real-joplin`; live index status reaches ready. |
| 2 real Joplin | compare one note against canonical Joplin data | `JP_MCP_LIVE_JOPLIN=1 just test-real-joplin`; gate compares one indexed note to the source Joplin DB row without printing body text. |
| 2 real Joplin | encrypted items are skipped | `JP_MCP_LIVE_JOPLIN=1 just test-real-joplin`; live assertion or documented fixture evidence. |
| 2 real Joplin | another user's note is not visible | `JP_MCP_LIVE_JOPLIN=1 just test-real-joplin`; live assertion passes. |
| 2 real Joplin | soft-deleted notes are not returned | `JP_MCP_LIVE_JOPLIN=1 just test-real-joplin`; live assertion passes. |
| 2 real Joplin | hard-deleted items or tombstones purge derived rows | `JP_MCP_LIVE_JOPLIN=1 just test-real-joplin`; live assertion or documented manual evidence. |
| 2 real Joplin | tag filters match canonical Joplin data | `JP_MCP_LIVE_JOPLIN=1 just test-real-joplin`; gate requires nonzero live tag edges when source edges exist, prunes dangling stale source edges, and compares one indexed edge/tag to source rows. |
| 2 real Joplin | logs appear in VictoriaLogs | `JP_MCP_LIVE_JOPLIN=1 just test-real-joplin`; query `http://victorialogs.lan:9428/select/logsql/query` by `test.id`. |
| 3 harness | `joplin-mcp-client bootstrap` | `just test-client` and `just test-all-local`; token file behavior verified. |
| 3 harness | `joplin-mcp-client serve` | `just test-client` and `just test-all-local`; stdio proxy starts only with valid token. |
| 3 harness | LLM harness config example | Manual doc review until an automated doc test exists. |
| 3 harness | status tool | `just test-all-local`; client/server status assertion passes. |
| 3 harness | `list_notebooks` | `just test-all-local`; MCP tool assertion passes. |
| 3 harness | `list_tags` | `just test-all-local`; MCP tool assertion passes. |
| 3 harness | `get_notebook_tree` | `just test-all-local`; MCP tool assertion passes. |
| 3 harness | `list_notes` | `just test-all-local`; MCP tool assertion passes. |
| 3 harness | `search_notes` | `just test-all-local`; MCP tool assertion passes. |
| 3 harness | `get_note` | `just test-all-local`; MCP tool assertion passes. |
| 3 harness | `get_note_excerpt` | `just test-all-local`; MCP tool assertion passes. |
| 3 harness | `get_changes_since` | `just test-all-local`; MCP tool assertion passes. |
| 3 harness | `logout` | `just test-client` and `just test-all-local`; revoke and local token removal assertions pass. |

## Acceptance Criteria Checklist

| Criterion | Verification |
| --- | --- |
| Unencrypted notes for the authenticated user can be searched and read | `just test-all-local`; live proof with `JP_MCP_LIVE_JOPLIN=1 just test-real-joplin`. |
| Encrypted notes are skipped and reported as unsupported | `cargo test --workspace`; live proof in `test-real-joplin` when fixtures exist. |
| Another user's notes are not visible | `cargo test --workspace`; live proof in `test-real-joplin`. |
| Joplin passwords are never stored | `cargo test --workspace`; review captured logs for no password values during live test. |
| Raw MCP tokens are never stored | `cargo test --workspace`; token storage tests pass. |
| Client token is stored under `XDG_RUNTIME_DIR` with mode `0600` | `just test-client`. |
| MCP Streamable HTTP is the primary remote transport | `cargo test --workspace`; transport tests pass. |
| MCP Streamable HTTP v1 is stateless: `POST /mcp` only, `GET`/`DELETE` return `405` | `cargo test --workspace`; HTTP transport tests pass. |
| Custom Bearer auth is documented as a v1 OAuth 2.1 non-goal | Manual doc review of `docs/contracts.md` or plan text. |
| TLS is required for LAN traffic | Manual Nix/deployment config review until automated. |
| Origin policy is explicit for browser and non-browser requests | `cargo test --workspace`; Origin/Referer tests pass. |
| Reverse-proxy IP attribution is explicit and safe by default | `cargo test --workspace`; config or HTTP tests pass. |
| Server indexes are stored in `joplin_mcp` schema | `cargo test --workspace`; migration/schema tests pass. |
| `sqlx _sqlx_migrations` is the schema-version source of truth | `cargo test --workspace`; migration tests pass. |
| External Joplin content storage fails early | `cargo test --workspace`; schema/source validation tests pass. |
| Tag filters work through `note_tags_index` | `cargo test --workspace`; MCP/search tests pass. |
| `deleted_items_index` records deletion evidence | `cargo test --workspace`; deletion reconciliation tests pass. |
| Stale index status has defined transitions | `cargo test --workspace`; index lifecycle tests pass. |
| Initial index build returns `index_not_ready` instead of silent empty results | `cargo test --workspace`; MCP tool status tests pass. |
| Foreground MCP/status requests do not depend on indexer pool availability | `cargo test --workspace`; pool isolation tests pass. |
| Large note bodies are truncated with continuation metadata | `cargo test --workspace`; note response budget tests pass. |
| Paginated tools use keyset cursors with stable sort keys | `cargo test --workspace`; pagination tests pass. |
| Single-instance deployment is enforced | `nix flake check`; module and runtime config tests pass. |
| Server/tool/statement timeouts and shutdown drain are defined | `cargo test --workspace`; lifecycle tests pass. |
| Persistent MCP users/tokens are backed up separately from derived indexes | Review `docs/backup-scope.md`; backup classification tests pass under `cargo test --workspace`. |
| All write tools are absent | `cargo test --workspace`; MCP registry tests pass. |
| VictoriaLogs receives OTLP/HTTP protobuf logs | `just test-all-local`; live proof with `JP_MCP_LIVE_JOPLIN=1 just test-real-joplin`. |
| Metrics expose index lag and tool latency | `cargo test --workspace`; metrics tests pass. |
| Tests pass only after polling VictoriaLogs by `test.id` | `just test-all-local`; missing-log test fails as designed and expected-log polling passes. |
| Startup fails early on bad DB/schema/token/logging config | `cargo test --workspace`; config, schema, token, and logging failure tests pass. |

## Manual Criteria

These criteria remain manual unless `jmr-gyb.5.5` adds automated coverage:

- Compare one live note against the Joplin UI/API.
- Verify live tag filters match the Joplin UI/API.
- Review the LLM harness config example.
- Review custom Bearer auth as a documented OAuth 2.1 non-goal.
- Review TLS deployment requirements for LAN traffic.

Manual evidence must avoid secret values, DSNs, passwords, raw MCP tokens, token
hashes, note bodies, full auth headers, and generated configs containing them.
