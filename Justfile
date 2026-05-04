set shell := ["bash", "-uc"]

fmt:
    cargo fmt --check

check:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

test-unit:
    cargo test --workspace --lib

test-db:
    cargo test -p joplin-mcpd --test db -- --ignored

test-server:
    cargo test -p joplin-mcpd --test server -- --ignored

test-client:
    cargo test -p joplin-mcp-client

test-e2e:
    cargo test -p joplin-mcpd --test e2e -- --ignored

test-all-local:
    cargo fmt --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo test --workspace

test-real-joplin:
    test "${JP_MCP_LIVE_JOPLIN:-}" = "1"
    cargo test -p joplin-mcpd --test real_joplin -- --ignored
