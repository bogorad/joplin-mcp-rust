# JP-MCP Backup Scope

Back up only persistent MCP state from the MCP Postgres database.

## Persistent MCP State

These tables are not rebuildable from Joplin and must be included in MCP state backups:

```text
joplin_mcp.mcp_users
joplin_mcp.mcp_tokens
joplin_mcp.audit_log
```

`mcp_tokens` stores token metadata and keyed token hashes. Raw MCP tokens are never stored, so a backup must not add a raw token export path.

## Derived Index State

These tables are derived from Joplin data and may be excluded from backups because the indexer can rebuild them:

```text
joplin_mcp.index_state
joplin_mcp.notebooks_index
joplin_mcp.notes_index
joplin_mcp.tags_index
joplin_mcp.note_tags_index
joplin_mcp.resources_index
joplin_mcp.deleted_items_index
```

After a restore that excludes derived indexes, run the normal index rebuild path before serving index-dependent MCP tools.

## Backup Command Shape

Use a table-scoped `pg_dump` against `postgres.mcp_database` with the MCP database role:

```sh
pg_dump "$MCP_DATABASE_URL" \
  --schema=joplin_mcp \
  --table=joplin_mcp.mcp_users \
  --table=joplin_mcp.mcp_tokens \
  --table=joplin_mcp.audit_log \
  --format=custom \
  --file=joplin-mcp-persistent-state.dump
```

Do not include `secrets.yaml`, decrypted secret files, DSNs, passwords, raw MCP tokens, Joplin passwords, full auth headers, note bodies, or token hashes in backup paths, backup manifests, command logs, or operator notes.

If `audit_log` retention is shortened later, create the long-term security archive from VictoriaLogs or another external log store before deleting old audit rows.
