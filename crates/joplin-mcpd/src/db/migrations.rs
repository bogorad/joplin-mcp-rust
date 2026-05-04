pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

pub const PERSISTENT_MCP_TABLES: &[&str] = &["mcp_users", "mcp_tokens", "audit_log"];
pub const DERIVED_INDEX_TABLES: &[&str] = &[
    "index_state",
    "notebooks_index",
    "notes_index",
    "tags_index",
    "note_tags_index",
    "resources_index",
    "deleted_items_index",
];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MigrationVersionError {
    #[error(
        "database migration version {applied_version} is newer than binary version {embedded_version}"
    )]
    DatabaseNewerThanBinary {
        applied_version: i64,
        embedded_version: i64,
    },
}

pub async fn run(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    ensure_database_not_newer_than_binary(pool).await?;
    MIGRATOR.run(pool).await?;
    Ok(())
}

pub fn latest_embedded_version() -> i64 {
    MIGRATOR
        .iter()
        .map(|migration| migration.version)
        .max()
        .unwrap_or_default()
}

pub async fn ensure_database_not_newer_than_binary(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    let applied_version = highest_applied_version(pool).await?;
    ensure_applied_version_supported(applied_version, latest_embedded_version())?;
    Ok(())
}

async fn highest_applied_version(pool: &sqlx::PgPool) -> anyhow::Result<Option<i64>> {
    let migrations_table: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations')::text")
            .fetch_one(pool)
            .await?;

    if migrations_table.is_none() {
        return Ok(None);
    }

    let version = sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations")
        .fetch_one(pool)
        .await?;
    Ok(version)
}

pub fn ensure_applied_version_supported(
    applied_version: Option<i64>,
    embedded_version: i64,
) -> Result<(), MigrationVersionError> {
    if let Some(applied_version) = applied_version
        && applied_version > embedded_version
    {
        return Err(MigrationVersionError::DatabaseNewerThanBinary {
            applied_version,
            embedded_version,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const INITIAL_SQL: &str = include_str!("../../migrations/20260504000100_initial.sql");
    const LAST_CHECKED_SQL: &str =
        include_str!("../../migrations/20260504000101_add_last_checked_at.sql");

    #[test]
    fn embedded_migration_version_is_present() {
        assert_eq!(latest_embedded_version(), 20260504000101);
    }

    #[test]
    fn rejects_newer_applied_database_version() {
        let error =
            ensure_applied_version_supported(Some(20260504000102), latest_embedded_version())
                .expect_err("newer database must be rejected");
        assert_eq!(
            error,
            MigrationVersionError::DatabaseNewerThanBinary {
                applied_version: 20260504000102,
                embedded_version: 20260504000101
            }
        );
    }

    #[test]
    fn allows_empty_or_current_database_version() {
        ensure_applied_version_supported(None, latest_embedded_version())
            .expect("fresh database is valid");
        ensure_applied_version_supported(Some(20260504000100), latest_embedded_version())
            .expect("current database is valid");
        ensure_applied_version_supported(Some(20260504000101), latest_embedded_version())
            .expect("current database is valid");
    }

    #[test]
    fn initial_migration_declares_required_schema_tables_and_indexes() {
        for required in [
            "CREATE SCHEMA IF NOT EXISTS joplin_mcp",
            "CREATE TABLE IF NOT EXISTS joplin_mcp.mcp_users",
            "CREATE TABLE IF NOT EXISTS joplin_mcp.mcp_tokens",
            "CREATE TABLE IF NOT EXISTS joplin_mcp.audit_log",
            "CREATE TABLE IF NOT EXISTS joplin_mcp.index_state",
            "CREATE TABLE IF NOT EXISTS joplin_mcp.notebooks_index",
            "CREATE TABLE IF NOT EXISTS joplin_mcp.notes_index",
            "CREATE TABLE IF NOT EXISTS joplin_mcp.tags_index",
            "CREATE TABLE IF NOT EXISTS joplin_mcp.note_tags_index",
            "CREATE TABLE IF NOT EXISTS joplin_mcp.resources_index",
            "CREATE TABLE IF NOT EXISTS joplin_mcp.deleted_items_index",
            "CREATE INDEX IF NOT EXISTS mcp_tokens_user_id_idx",
            "CREATE INDEX IF NOT EXISTS audit_log_user_time_idx",
            "CREATE INDEX IF NOT EXISTS notebooks_parent_idx",
            "CREATE INDEX IF NOT EXISTS notes_parent_idx",
            "CREATE INDEX IF NOT EXISTS notes_updated_idx",
            "CREATE INDEX IF NOT EXISTS notes_search_idx",
            "CREATE INDEX IF NOT EXISTS note_tags_by_tag_idx",
            "search_vector tsvector GENERATED ALWAYS AS",
            "to_tsvector('simple'",
        ] {
            assert!(
                INITIAL_SQL.contains(required),
                "missing SQL fragment: {required}"
            );
        }
    }

    #[test]
    fn initial_migration_does_not_mutate_canonical_joplin_tables() {
        let forbidden_fragments = [
            "CREATE TABLE IF NOT EXISTS users",
            "CREATE TABLE users",
            "ALTER TABLE users",
            "CREATE INDEX IF NOT EXISTS users",
            "CREATE INDEX users",
            "CREATE TABLE IF NOT EXISTS items",
            "CREATE TABLE items",
            "ALTER TABLE items",
            "CREATE INDEX IF NOT EXISTS items",
            "CREATE INDEX items",
            "CREATE TRIGGER",
        ];

        for forbidden in forbidden_fragments {
            assert!(
                !INITIAL_SQL.contains(forbidden),
                "migration must not mutate Joplin table: {forbidden}"
            );
        }
    }

    #[test]
    fn last_checked_migration_declares_index_poll_state() {
        assert!(LAST_CHECKED_SQL.contains("ALTER TABLE joplin_mcp.index_state"));
        assert!(LAST_CHECKED_SQL.contains("ADD COLUMN IF NOT EXISTS last_checked_at timestamptz"));
    }

    #[test]
    fn backup_classification_includes_audit_log_as_persistent_state() {
        assert_eq!(
            PERSISTENT_MCP_TABLES,
            ["mcp_users", "mcp_tokens", "audit_log"]
        );
        assert_eq!(
            DERIVED_INDEX_TABLES,
            [
                "index_state",
                "notebooks_index",
                "notes_index",
                "tags_index",
                "note_tags_index",
                "resources_index",
                "deleted_items_index"
            ]
        );
        for persistent_table in PERSISTENT_MCP_TABLES {
            assert!(
                !DERIVED_INDEX_TABLES.contains(persistent_table),
                "persistent table must not be classified as derived: {persistent_table}"
            );
        }
        assert!(!DERIVED_INDEX_TABLES.contains(&"audit_log"));
    }
}
