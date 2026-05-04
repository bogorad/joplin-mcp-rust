# Observability

This document records the v1 metric, trace, and alert contract. Labels must stay bounded. Do not use note IDs, raw queries, emails, token values, token hashes, passwords, DSNs, request IDs, or test IDs as metric labels.

## Metrics

- `index_lag_seconds`: histogram by `user.hash`.
- `index_refresh_duration_seconds`: histogram by `outcome`.
- `mcp_tool_duration_seconds`: histogram by `tool`.
- `mcp_tool_errors_total`: counter by `tool` and `error.kind`.
- `bootstrap_login_total`: counter by `outcome`.
- `postgres_pool_wait_seconds`: histogram by `pool`.

## Trace Spans

- Bootstrap login: `bootstrap.login`, `bootstrap.joplin_auth`, `bootstrap.user_resolve`, `bootstrap.user_upsert`, `bootstrap.token_mint`.
- Index refresh: `index.refresh`, `index.source_query`, `index.row_upserts`, `index.state_update`, with `index.full_rebuild` when a rebuild is required.
- MCP tool call: `mcp.tool_call`.

## Minimum Alerts

- `index_status_failed`: page when any user's index enters failed status.
- `bootstrap_errors_above_threshold`: page on sustained failed or rate-limited bootstrap attempts.
- `victorialogs_ingestion_failure`: page when OTLP log ingestion or test log polling fails.
- `postgres_pool_saturation`: page on sustained runtime or indexer pool wait.
- `mcp_tool_error_rate_above_threshold`: page on sustained MCP tool errors by bounded tool and error kind.
