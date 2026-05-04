# JP-MCP v1 Contracts

This document mirrors the project limits in `JOPLIN-MCP-RUST.md`.

## Scope Boundaries

- Only unencrypted Joplin data is supported. Encrypted items are skipped.
- Version 1 is read-only. No write tools or direct Joplin table mutations are allowed.
- Owner-owned items are indexed for the owner, including owner-owned items in shared folders.
- Recipient-only shared notebook visibility is unsupported in version 1.
- The service is LAN-only and still requires authentication for every MCP request.
- TLS is required for LAN traffic. Plain HTTP is allowed only for localhost test mode.
- MCP tokens and Joplin session tokens are separate credentials.
- Joplin passwords are accepted only for bootstrap/login and are never stored.
- Raw MCP tokens are returned once and are never stored.

## Module Boundaries

- `auth` authenticates users and validates MCP tokens. It does not parse notes.
- `indexer` parses Joplin items and writes derived index state. It does not validate HTTP tokens.
- `mcp` reads derived index tables only. It does not read canonical Joplin tables.
- `logging` receives redacted operational fields only. It never receives raw secrets.
- `joplin-mcp-client` never connects to Postgres.

## Shared Error Codes

- `validation`
- `auth`
- `not_found`
- `index_not_ready`
- `rate_limited`
- `unsupported`
- `internal`

## Contributor Checklist

- Do not implement Joplin E2EE decryption.
- Do not add write tools.
- Do not infer recipient visibility from shared notebooks.
- Do not log note bodies, passwords, raw tokens, token hashes, DSNs, or full auth headers.
- Do not collapse Joplin and MCP Postgres databases or roles.
- Do not bypass `joplin_mcp` migrations for MCP-owned schema changes.
