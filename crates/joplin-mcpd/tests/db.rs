use joplin_mcpd::db::migrations::{
    DERIVED_INDEX_TABLES, PERSISTENT_MCP_TABLES, ensure_applied_version_supported,
    latest_embedded_version,
};

#[test]
#[ignore = "requires explicit database-test command"]
fn migration_contract_separates_persistent_and_derived_tables() {
    assert_eq!(
        PERSISTENT_MCP_TABLES,
        ["mcp_users", "mcp_tokens", "audit_log"]
    );
    assert!(DERIVED_INDEX_TABLES.contains(&"index_state"));
    assert!(DERIVED_INDEX_TABLES.contains(&"notes_index"));
    assert!(!DERIVED_INDEX_TABLES.contains(&"mcp_tokens"));
}

#[test]
#[ignore = "requires explicit database-test command"]
fn migration_version_gate_rejects_newer_databases() {
    let embedded = latest_embedded_version();

    ensure_applied_version_supported(None, embedded).expect("fresh database accepted");
    ensure_applied_version_supported(Some(embedded), embedded).expect("current database accepted");
    ensure_applied_version_supported(Some(embedded + 1), embedded)
        .expect_err("newer database rejected");
}

#[test]
#[ignore = "requires explicit database-test command"]
fn local_compose_provides_disposable_database_service() {
    let compose = include_str!("../../../tests/compose.local.yaml");

    assert!(compose.contains("postgres:"));
    assert!(compose.contains("postgres:17-alpine"));
    assert!(compose.contains("127.0.0.1:55432:5432"));
    assert!(compose.contains("victorialogs:"));
    assert!(compose.contains("127.0.0.1:59428:9428"));
}
