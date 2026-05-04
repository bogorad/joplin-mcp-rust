use anyhow::{Context, ensure};
use chrono::{DateTime, Utc};
use joplin_mcpd::{
    config::IndexConfig,
    db::{
        lifecycle::SingletonLock,
        migrations::{
            self, DERIVED_INDEX_TABLES, PERSISTENT_MCP_TABLES, ensure_applied_version_supported,
            latest_embedded_version,
        },
    },
    indexer::{source::JoplinDbSource, worker::run_index_refresh_cycle},
};
use sqlx::{
    FromRow, PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::time::Duration;
use uuid::Uuid;

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

#[tokio::test]
#[ignore = "requires disposable local Postgres from tests/compose.local.yaml"]
async fn singleton_lock_holds_connection_until_lock_object_drops() -> anyhow::Result<()> {
    let dbs = TestDatabases::create("singleton").await?;
    let (joplin_pool, mcp_pool) = dbs.connect().await?;

    let first = SingletonLock::acquire(&mcp_pool).await?;
    let second = SingletonLock::acquire(&mcp_pool)
        .await
        .expect_err("second lock acquisition should fail while first lock is alive");
    ensure!(
        second
            .to_string()
            .contains("another joplin-mcpd instance holds the singleton lock"),
        "unexpected second acquisition error: {second:#}"
    );

    drop(first);

    let reacquired = SingletonLock::acquire(&mcp_pool).await?;
    drop(reacquired);

    joplin_pool.close().await;
    mcp_pool.close().await;
    dbs.drop().await
}

#[tokio::test]
#[ignore = "requires disposable local Postgres from tests/compose.local.yaml"]
async fn repeated_worker_refresh_updates_changed_rows_without_dangling_tag_edges()
-> anyhow::Result<()> {
    let dbs = TestDatabases::create("changed").await?;
    let (joplin_pool, mcp_pool) = dbs.connect().await?;
    prepare_joplin_source(&joplin_pool).await?;
    migrations::run(&mcp_pool).await?;

    let user_id = insert_mcp_user(&mcp_pool, "joplin-user-a").await?;
    insert_joplin_item(
        &joplin_pool,
        SourceItem::note("joplin-user-a", "note-a", "Initial body", 10),
    )
    .await?;
    insert_joplin_item(
        &joplin_pool,
        SourceItem::tag("joplin-user-a", "tag-a", "Tag A", 11),
    )
    .await?;
    insert_joplin_item(
        &joplin_pool,
        SourceItem::note_tag("joplin-user-a", "edge-a", "note-a", "tag-a", 12),
    )
    .await?;
    insert_joplin_item(
        &joplin_pool,
        SourceItem::note_tag(
            "joplin-user-a",
            "dangling-edge",
            "note-a",
            "missing-tag",
            13,
        ),
    )
    .await?;

    let source = JoplinDbSource::new(joplin_pool.clone());
    let config = immediate_refresh_config();
    run_index_refresh_cycle(&mcp_pool, &source, &config).await;
    let initial_state = index_state(&mcp_pool, user_id).await?;
    ensure_eq(
        initial_state.last_seen_joplin_updated_time,
        Some(13),
        "initial watermark",
    )?;
    assert_note_body(&mcp_pool, user_id, "note-a", "Initial body").await?;
    assert_no_dangling_tag_edges(&mcp_pool, user_id).await?;

    force_user_due(&mcp_pool, user_id).await?;
    update_joplin_note_body(&joplin_pool, "joplin-user-a", "note-a", "Changed body", 20).await?;
    run_index_refresh_cycle(&mcp_pool, &source, &config).await;

    let changed_state = index_state(&mcp_pool, user_id).await?;
    ensure_eq(
        changed_state.last_seen_joplin_updated_time,
        Some(20),
        "changed watermark",
    )?;
    ensure!(
        changed_state.last_incremental_at > initial_state.last_incremental_at,
        "incremental timestamp did not advance after changed row"
    );
    assert_note_body(&mcp_pool, user_id, "note-a", "Changed body").await?;
    assert_no_dangling_tag_edges(&mcp_pool, user_id).await?;

    joplin_pool.close().await;
    mcp_pool.close().await;
    dbs.drop().await
}

#[tokio::test]
#[ignore = "requires disposable local Postgres from tests/compose.local.yaml"]
async fn repeated_worker_refresh_marks_no_change_checked_without_rewriting_rows()
-> anyhow::Result<()> {
    let dbs = TestDatabases::create("nochange").await?;
    let (joplin_pool, mcp_pool) = dbs.connect().await?;
    prepare_joplin_source(&joplin_pool).await?;
    migrations::run(&mcp_pool).await?;

    let user_id = insert_mcp_user(&mcp_pool, "joplin-user-a").await?;
    insert_joplin_item(
        &joplin_pool,
        SourceItem::note("joplin-user-a", "note-a", "Stable body", 10),
    )
    .await?;

    let source = JoplinDbSource::new(joplin_pool.clone());
    let config = immediate_refresh_config();
    run_index_refresh_cycle(&mcp_pool, &source, &config).await;
    let initial_state = index_state(&mcp_pool, user_id).await?;
    let initial_indexed_at = note_indexed_at(&mcp_pool, user_id, "note-a").await?;

    let old_checked_at = force_user_due(&mcp_pool, user_id).await?;
    run_index_refresh_cycle(&mcp_pool, &source, &config).await;

    let checked_state = index_state(&mcp_pool, user_id).await?;
    let checked_indexed_at = note_indexed_at(&mcp_pool, user_id, "note-a").await?;
    ensure!(
        checked_state.last_checked_at > old_checked_at,
        "last_checked_at did not advance on no-change cycle"
    );
    ensure_eq(
        checked_state.last_seen_joplin_updated_time,
        initial_state.last_seen_joplin_updated_time,
        "no-change watermark",
    )?;
    ensure_eq(
        checked_state.last_incremental_at,
        initial_state.last_incremental_at,
        "no-change incremental timestamp",
    )?;
    ensure_eq(
        checked_indexed_at,
        initial_indexed_at,
        "no-change row indexed_at",
    )?;

    joplin_pool.close().await;
    mcp_pool.close().await;
    dbs.drop().await
}

#[tokio::test]
#[ignore = "requires disposable local Postgres from tests/compose.local.yaml"]
async fn repeated_worker_refresh_selectively_updates_only_changed_users() -> anyhow::Result<()> {
    let dbs = TestDatabases::create("multi").await?;
    let (joplin_pool, mcp_pool) = dbs.connect().await?;
    prepare_joplin_source(&joplin_pool).await?;
    migrations::run(&mcp_pool).await?;

    let user_a = insert_mcp_user(&mcp_pool, "joplin-user-a").await?;
    let user_b = insert_mcp_user(&mcp_pool, "joplin-user-b").await?;
    insert_joplin_item(
        &joplin_pool,
        SourceItem::note("joplin-user-a", "note-a", "Alice initial", 10),
    )
    .await?;
    insert_joplin_item(
        &joplin_pool,
        SourceItem::note("joplin-user-b", "note-b", "Bob stable", 10),
    )
    .await?;

    let source = JoplinDbSource::new(joplin_pool.clone());
    let config = IndexConfig {
        max_parallel_users: 2,
        ..immediate_refresh_config()
    };
    run_index_refresh_cycle(&mcp_pool, &source, &config).await;
    let initial_a = index_state(&mcp_pool, user_a).await?;
    let initial_b = index_state(&mcp_pool, user_b).await?;
    let indexed_b = note_indexed_at(&mcp_pool, user_b, "note-b").await?;

    force_user_due(&mcp_pool, user_a).await?;
    force_user_due(&mcp_pool, user_b).await?;
    update_joplin_note_body(&joplin_pool, "joplin-user-a", "note-a", "Alice changed", 30).await?;
    run_index_refresh_cycle(&mcp_pool, &source, &config).await;

    let changed_a = index_state(&mcp_pool, user_a).await?;
    let checked_b = index_state(&mcp_pool, user_b).await?;
    ensure_eq(
        changed_a.last_seen_joplin_updated_time,
        Some(30),
        "user A watermark",
    )?;
    ensure_eq(
        checked_b.last_seen_joplin_updated_time,
        initial_b.last_seen_joplin_updated_time,
        "user B watermark",
    )?;
    ensure!(
        changed_a.last_incremental_at > initial_a.last_incremental_at,
        "changed user's incremental timestamp did not advance"
    );
    ensure_eq(
        checked_b.last_incremental_at,
        initial_b.last_incremental_at,
        "unchanged user's incremental timestamp",
    )?;
    ensure_eq(
        note_indexed_at(&mcp_pool, user_b, "note-b").await?,
        indexed_b,
        "unchanged user's note indexed_at",
    )?;
    assert_note_body(&mcp_pool, user_a, "note-a", "Alice changed").await?;
    assert_note_body(&mcp_pool, user_b, "note-b", "Bob stable").await?;

    joplin_pool.close().await;
    mcp_pool.close().await;
    dbs.drop().await
}

#[derive(Debug, FromRow)]
struct IndexStateRow {
    last_checked_at: DateTime<Utc>,
    last_incremental_at: Option<DateTime<Utc>>,
    last_seen_joplin_updated_time: Option<i64>,
}

struct TestDatabases {
    admin_options: PgConnectOptions,
    joplin_db: String,
    mcp_db: String,
}

impl TestDatabases {
    async fn create(label: &str) -> anyhow::Result<Self> {
        let suffix = Uuid::new_v4().simple().to_string();
        let joplin_db = format!("jmr_{label}_joplin_{suffix}");
        let mcp_db = format!("jmr_{label}_mcp_{suffix}");
        let admin_options = local_postgres_options("postgres");
        let admin = connect_pool(admin_options.clone()).await?;

        create_database(&admin, &joplin_db).await?;
        create_database(&admin, &mcp_db).await?;
        admin.close().await;

        Ok(Self {
            admin_options,
            joplin_db,
            mcp_db,
        })
    }

    async fn connect(&self) -> anyhow::Result<(PgPool, PgPool)> {
        let joplin_pool = connect_pool(local_postgres_options(&self.joplin_db)).await?;
        let mcp_pool = connect_pool(local_postgres_options(&self.mcp_db)).await?;
        Ok((joplin_pool, mcp_pool))
    }

    async fn drop(self) -> anyhow::Result<()> {
        let admin = connect_pool(self.admin_options).await?;
        drop_database(&admin, &self.joplin_db).await?;
        drop_database(&admin, &self.mcp_db).await?;
        admin.close().await;
        Ok(())
    }
}

fn local_postgres_options(database: &str) -> PgConnectOptions {
    PgConnectOptions::new()
        .host("127.0.0.1")
        .port(55432)
        .username("postgres")
        .password("local-postgres")
        .database(database)
}

fn immediate_refresh_config() -> IndexConfig {
    IndexConfig {
        max_parallel_users: 1,
        refresh_interval_seconds: 0,
        ..IndexConfig::default()
    }
}

async fn connect_pool(options: PgConnectOptions) -> anyhow::Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options)
        .await
        .context("connect to disposable local Postgres")
}

async fn create_database(admin: &PgPool, name: &str) -> anyhow::Result<()> {
    sqlx::query(&format!(r#"CREATE DATABASE "{name}""#))
        .execute(admin)
        .await
        .with_context(|| format!("create disposable database {name}"))?;
    Ok(())
}

async fn drop_database(admin: &PgPool, name: &str) -> anyhow::Result<()> {
    sqlx::query(&format!(r#"DROP DATABASE IF EXISTS "{name}" WITH (FORCE)"#))
        .execute(admin)
        .await
        .with_context(|| format!("drop disposable database {name}"))?;
    Ok(())
}

async fn prepare_joplin_source(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE items (
            id text PRIMARY KEY,
            owner_id text NOT NULL,
            content bytea NOT NULL,
            name text NOT NULL,
            mime_type text NOT NULL,
            updated_time bigint NOT NULL,
            created_time bigint NOT NULL,
            jop_id text NOT NULL,
            jop_parent_id text NOT NULL,
            jop_type int NOT NULL,
            jop_encryption_applied int NOT NULL DEFAULT 0
        )
        "#,
    )
    .execute(pool)
    .await
    .context("create disposable Joplin items table")?;
    Ok(())
}

async fn insert_mcp_user(pool: &PgPool, joplin_user_id: &str) -> anyhow::Result<Uuid> {
    let user_id = Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO joplin_mcp.mcp_users (id, joplin_user_id, joplin_email, last_login_at)
        VALUES ($1, $2, $3, now())
        "#,
    )
    .bind(user_id)
    .bind(joplin_user_id)
    .bind(format!("{joplin_user_id}@example.test"))
    .execute(pool)
    .await
    .context("insert disposable MCP user")?;

    sqlx::query(
        r#"
        INSERT INTO joplin_mcp.mcp_tokens (id, user_id, token_hash, hmac_key_id, label, scope)
        VALUES ($1, $2, $3, 'db-test', 'db-test', 'read')
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(Uuid::new_v4().as_bytes().to_vec())
    .execute(pool)
    .await
    .context("insert disposable active MCP token")?;

    Ok(user_id)
}

#[derive(Clone)]
struct SourceItem {
    owner_id: String,
    joplin_id: String,
    item_id: String,
    name: String,
    content: Vec<u8>,
    updated_time: i64,
    item_type: i32,
}

impl SourceItem {
    fn note(owner_id: &str, joplin_id: &str, body: &str, updated_time: i64) -> Self {
        Self {
            owner_id: owner_id.to_string(),
            joplin_id: joplin_id.to_string(),
            item_id: format!("server-{owner_id}-{joplin_id}"),
            name: format!("{joplin_id} title"),
            content: body.as_bytes().to_vec(),
            updated_time,
            item_type: 1,
        }
    }

    fn tag(owner_id: &str, joplin_id: &str, title: &str, updated_time: i64) -> Self {
        Self {
            owner_id: owner_id.to_string(),
            joplin_id: joplin_id.to_string(),
            item_id: format!("server-{owner_id}-{joplin_id}"),
            name: title.to_string(),
            content: Vec::new(),
            updated_time,
            item_type: 5,
        }
    }

    fn note_tag(
        owner_id: &str,
        joplin_id: &str,
        note_id: &str,
        tag_id: &str,
        updated_time: i64,
    ) -> Self {
        Self {
            owner_id: owner_id.to_string(),
            joplin_id: joplin_id.to_string(),
            item_id: format!("server-{owner_id}-{joplin_id}"),
            name: String::new(),
            content: format!("note_id: {note_id}\ntag_id: {tag_id}").into_bytes(),
            updated_time,
            item_type: 6,
        }
    }
}

async fn insert_joplin_item(pool: &PgPool, item: SourceItem) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO items (
            id, owner_id, content, name, mime_type, updated_time, created_time,
            jop_id, jop_parent_id, jop_type, jop_encryption_applied
        )
        VALUES ($1, $2, $3, $4, 'text/plain', $5, 1, $6, '', $7, 0)
        "#,
    )
    .bind(item.item_id)
    .bind(item.owner_id)
    .bind(item.content)
    .bind(item.name)
    .bind(item.updated_time)
    .bind(item.joplin_id)
    .bind(item.item_type)
    .execute(pool)
    .await
    .context("insert disposable Joplin item")?;
    Ok(())
}

async fn update_joplin_note_body(
    pool: &PgPool,
    owner_id: &str,
    joplin_id: &str,
    body: &str,
    updated_time: i64,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        UPDATE items
        SET content = $3, updated_time = $4
        WHERE owner_id = $1
          AND jop_id = $2
          AND jop_type = 1
        "#,
    )
    .bind(owner_id)
    .bind(joplin_id)
    .bind(body.as_bytes())
    .bind(updated_time)
    .execute(pool)
    .await
    .context("update disposable Joplin note")?;
    Ok(())
}

async fn force_user_due(pool: &PgPool, user_id: Uuid) -> anyhow::Result<DateTime<Utc>> {
    let old_checked_at = Utc::now() - chrono::Duration::minutes(5);
    sqlx::query("UPDATE joplin_mcp.index_state SET last_checked_at = $2 WHERE user_id = $1")
        .bind(user_id)
        .bind(old_checked_at)
        .execute(pool)
        .await
        .context("force disposable user refresh due")?;
    Ok(old_checked_at)
}

async fn index_state(pool: &PgPool, user_id: Uuid) -> anyhow::Result<IndexStateRow> {
    sqlx::query_as::<_, IndexStateRow>(
        r#"
        SELECT last_checked_at, last_incremental_at, last_seen_joplin_updated_time
        FROM joplin_mcp.index_state
        WHERE user_id = $1
        "#,
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .context("read disposable index state")
}

async fn note_indexed_at(
    pool: &PgPool,
    user_id: Uuid,
    joplin_id: &str,
) -> anyhow::Result<DateTime<Utc>> {
    sqlx::query_scalar(
        "SELECT indexed_at FROM joplin_mcp.notes_index WHERE user_id = $1 AND joplin_id = $2",
    )
    .bind(user_id)
    .bind(joplin_id)
    .fetch_one(pool)
    .await
    .context("read disposable note indexed_at")
}

async fn assert_note_body(
    pool: &PgPool,
    user_id: Uuid,
    joplin_id: &str,
    expected: &str,
) -> anyhow::Result<()> {
    let body: String = sqlx::query_scalar(
        "SELECT body_text FROM joplin_mcp.notes_index WHERE user_id = $1 AND joplin_id = $2",
    )
    .bind(user_id)
    .bind(joplin_id)
    .fetch_one(pool)
    .await
    .context("read disposable note body")?;
    ensure_eq(body, expected.to_string(), "indexed note body")
}

async fn assert_no_dangling_tag_edges(pool: &PgPool, user_id: Uuid) -> anyhow::Result<()> {
    let dangling: i64 = sqlx::query_scalar(
        r#"
        SELECT count(*)
        FROM joplin_mcp.note_tags_index edge
        LEFT JOIN joplin_mcp.notes_index notes
          ON notes.user_id = edge.user_id
         AND notes.joplin_id = edge.note_joplin_id
        LEFT JOIN joplin_mcp.tags_index tags
          ON tags.user_id = edge.user_id
         AND tags.joplin_id = edge.tag_joplin_id
        WHERE edge.user_id = $1
          AND (notes.joplin_id IS NULL OR tags.joplin_id IS NULL)
        "#,
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .context("count dangling disposable tag edges")?;
    ensure_eq(dangling, 0, "dangling tag edges")
}

fn ensure_eq<T>(actual: T, expected: T, label: &str) -> anyhow::Result<()>
where
    T: std::fmt::Debug + PartialEq,
{
    ensure!(
        actual == expected,
        "{label}: expected {expected:?}, got {actual:?}"
    );
    Ok(())
}
