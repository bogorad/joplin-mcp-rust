# Real Joplin Validation

This validation is manual and opt-in. It must not run in normal local or CI test
flows.

Run the live integration target only with:

```bash
JP_MCP_LIVE_JOPLIN=1 cargo test -p joplin-mcpd --test real_joplin -- --ignored
```

If this machine cannot resolve the SOPS `postgres.host` name, provide a
sanitized local override without changing database, user, or password secrets:

```bash
JP_MCP_LIVE_JOPLIN=1 JP_MCP_LIVE_POSTGRES_HOST=<reachable-host-or-ip> just test-real-joplin
```

Non-secret checks before running:

1. Confirm `secrets.yaml` exists and is SOPS-encrypted.
2. Confirm the configured SOPS identity is available to `sops -d secrets.yaml`.
3. Confirm these key names exist without printing values:
   `victorialogs_url`, `joplin.url`, `joplin.username`, `joplin.password`,
   `postgres.host`, `postgres.port`, `postgres.joplin_database`,
   `postgres.joplin_user`, `postgres.joplin_password`,
   `postgres.mcp_database`, `postgres.mcp_user`, and
   `postgres.mcp_password`.
4. Confirm `postgres.joplin_database` and `postgres.mcp_database` are separate.
5. Confirm `postgres.joplin_user` and `postgres.mcp_user` are separate.
6. Confirm `victorialogs_url` resolves to the live VictoriaLogs base URL.

The live test compares a representative indexed note and tag edge against the
canonical Joplin database rows. It also fails if the live source has unencrypted
notes, tags, or note-tag rows but the derived index is empty for that class.
Record only aggregate counts, commands, and timestamps. Do not paste note
bodies, passwords, DSNs, session IDs, tokens, or decrypted secret values into
logs, tickets, snapshots, or docs.
