# Joplin MCP Rust

## For Humans

Joplin MCP Rust gives LLM tools read-only access to a user's unencrypted Joplin
Server notes without giving the model direct access to the Joplin database,
Joplin credentials, or broad filesystem state.

The system has two binaries:

- `joplin-mcpd` is the server. It reads canonical Joplin Server data, builds
  derived per-user indexes in a separate MCP database, authenticates MCP
  requests, and exposes read-only MCP tools over Streamable HTTP.
- `joplin-mcp-client` is the local harness helper. It bootstraps a user session,
  stores the returned MCP token in a narrow runtime token file, and proxies MCP
  stdio traffic to the remote HTTP server.

The server reads from two separate Postgres databases and roles:

- The Joplin database is read-only input. It contains canonical Joplin Server
  tables such as `users` and `items`.
- The MCP database is owned by this project. It contains `joplin_mcp` tables for
  MCP users, hashed MCP tokens, audit rows, index state, notes, notebooks, tags,
  note-tag edges, resources, and deletion evidence.

The derived index is the model-facing data boundary. MCP tools read only from
`joplin_mcp` index tables. They do not query canonical Joplin tables directly.
The indexer keeps owner-only visibility, skips encrypted items, skips unsupported
shared-recipient visibility, parses live Joplin DB content, and prunes stale
note-tag edges whose note or tag is not active.

The security model is intentionally conservative:

- Version 1 is read-only. There are no write tools.
- Joplin passwords are accepted only during bootstrap/login and are not stored.
- Raw MCP tokens are returned once, then stored only as hashes.
- TLS is required for LAN traffic. Plain HTTP is for localhost test mode only.
- Origin and Referer checks protect browser-reachable routes.
- Logs and audit metadata must not contain note bodies, passwords, DSNs, raw
  tokens, token hashes, full auth headers, or decrypted secrets.

Common verification commands:

```bash
nix develop
just test-all-local
cargo build --locked --workspace
nix flake check
nix build .#joplin-mcpd
nix build .#joplin-mcp-client
```

The real-Joplin gate is opt-in because it decrypts test secrets and touches a
real Joplin Server:

```bash
JP_MCP_LIVE_JOPLIN=1 just test-real-joplin
```

If the configured Postgres hostname is not reachable from the test machine:

```bash
JP_MCP_LIVE_JOPLIN=1 JP_MCP_LIVE_POSTGRES_HOST=<reachable-host-or-ip> just test-real-joplin
```

More detail lives in:

- `docs/contracts.md` for boundaries and non-goals.
- `docs/acceptance.md` for the full acceptance map.
- `docs/real-joplin-validation.md` for live validation.
- `docs/backup-scope.md` for persistent-state backup boundaries.
- `docs/observability.md` for logging and metrics.

## For LLMs

Wire the server first. Wire the client only where a local stdio MCP harness needs
to talk to the remote server.

NixOS module shape:

```nix
{
  inputs.joplin-mcp-rust.url = "github:bogorad/joplin-mcp-rust";

  outputs = { self, nixpkgs, joplin-mcp-rust, ... }: {
    nixosConfigurations.host = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        joplin-mcp-rust.nixosModules.joplin-mcp
        {
          services.joplin-mcp = {
            enable = true;
            package = joplin-mcp-rust.packages.x86_64-linux.joplin-mcpd;

            credentials = {
              postgresJoplinDsn = "/run/secrets/postgres-joplin-dsn";
              postgresMcpDsn = "/run/secrets/postgres-mcp-dsn";
              tokenHmacKey = "/run/secrets/token-hmac-key";
            };

            settings = {
              server = {
                listen = "127.0.0.1:8081";
                public_base_url = "https://joplin-mcp.lan";
                tls_mode = "required";
                allowed_origins = [ "https://joplin-mcp.lan" ];
              };
            };
          };
        }
      ];
    };
  };
}
```

The included module creates `systemd.services.joplin-mcpd`, runs it as the
`joplin-mcp` system user and group, sets `StateDirectory=joplin-mcp`,
`RuntimeDirectory=joplin-mcp`, `UMask=0077`, and loads these systemd
credentials:

- `postgres-joplin-dsn`
- `postgres-mcp-dsn`
- `token-hmac-key`

Plain systemd unit shape:

```ini
[Unit]
Description=Joplin MCP server
After=network-online.target
Wants=network-online.target

[Service]
User=joplin-mcp
Group=joplin-mcp
StateDirectory=joplin-mcp
RuntimeDirectory=joplin-mcp
UMask=0077
LoadCredential=postgres-joplin-dsn:/run/secrets/postgres-joplin-dsn
LoadCredential=postgres-mcp-dsn:/run/secrets/postgres-mcp-dsn
LoadCredential=token-hmac-key:/run/secrets/token-hmac-key
Environment=JP_MCPD_CONFIG=/etc/joplin-mcpd.toml
ExecStart=/usr/local/bin/joplin-mcpd --config /etc/joplin-mcpd.toml
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

Matching `joplin-mcpd.toml` shape:

```toml
[postgres]
joplin_dsn_file = "/run/credentials/joplin-mcpd.service/postgres-joplin-dsn"
mcp_dsn_file = "/run/credentials/joplin-mcpd.service/postgres-mcp-dsn"
runtime_max_connections = 12
indexer_max_connections = 4
acquire_timeout_seconds = 5
statement_timeout_seconds = 15

[server]
listen = "127.0.0.1:8081"
public_base_url = "https://joplin-mcp.lan"
tls_mode = "required"
allowed_origins = ["https://joplin-mcp.lan"]
allow_insecure_localhost = false
lan_cidrs = []
trusted_proxies = []

[tokens]
active_hmac_key_id = "primary"

[[tokens.hmac_keys]]
id = "primary"
file = "/run/credentials/joplin-mcpd.service/token-hmac-key"
```

Put TLS at the reverse proxy when `joplin-mcpd` listens on localhost. The proxy
must preserve the public HTTPS origin configured in `public_base_url` and
`allowed_origins`.

Client bootstrap shape:

```bash
joplin-mcp-client bootstrap \
  --server-url https://joplin-mcp.lan \
  --email <joplin-login-email>
```

Client stdio proxy shape for an MCP harness:

```bash
joplin-mcp-client serve --server-url https://joplin-mcp.lan
```

Operational constraints for agents:

- Do not collapse the Joplin and MCP Postgres databases or users.
- Do not place Joplin passwords, MCP tokens, DSNs, or HMAC key bytes in Nix
  store paths. Use systemd credentials, SOPS, agenix, or another runtime secret
  mechanism.
- Do not expose `joplin-mcpd` over plain HTTP except on localhost test paths.
- Do not add write access to canonical Joplin tables.
- Do not route MCP tools around the derived `joplin_mcp` index tables.
