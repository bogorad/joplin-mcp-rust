use anyhow::{Context, ensure};
use joplin_mcpd::{
    auth::joplin::{
        HttpJoplinAuthenticator, JoplinAuthenticator, resolve_joplin_user_by_id,
        upsert_mcp_user_by_joplin_user,
    },
    db::{migrations, schema_check::validate_joplin_source_schema},
    indexer::{
        JoplinItemType,
        rebuild::full_rebuild_user,
        source::{JoplinDbSource, JoplinSource},
    },
    observability::victorialogs::VictoriaLogsHarness,
};
use sqlx::{
    FromRow,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

const LIVE_JOPLIN_ENV: &str = "JP_MCP_LIVE_JOPLIN";
const LIVE_POSTGRES_HOST_OVERRIDE_ENV: &str = "JP_MCP_LIVE_POSTGRES_HOST";
const SECRETS_FILE: &str = "secrets.yaml";
const REQUIRED_SECRET_KEYS: &[&str] = &[
    ".victorialogs_url",
    ".joplin.url",
    ".joplin.username",
    ".joplin.password",
    ".postgres.host",
    ".postgres.port",
    ".postgres.joplin_database",
    ".postgres.joplin_user",
    ".postgres.joplin_password",
    ".postgres.mcp_database",
    ".postgres.mcp_user",
    ".postgres.mcp_password",
];

#[tokio::test]
#[ignore = "real Joplin integration is opt-in: JP_MCP_LIVE_JOPLIN=1 cargo test -p joplin-mcpd --test real_joplin -- --ignored"]
async fn validates_real_joplin_indexer_contract() -> anyhow::Result<()> {
    if env::var(LIVE_JOPLIN_ENV).as_deref() != Ok("1") {
        eprintln!("skipping real Joplin integration test; JP_MCP_LIVE_JOPLIN is not 1");
        return Ok(());
    }

    let secrets = LiveSecrets::load(&repo_root().join(SECRETS_FILE))?;
    let victorialogs = secrets.victorialogs_harness()?;
    secrets.validate_split_postgres_contract()?;

    let joplin_pool = connect_pool("joplin", secrets.joplin_db_options()).await?;
    let mcp_pool = connect_pool("mcp", secrets.mcp_db_options()).await?;

    validate_joplin_source_schema(&joplin_pool)
        .await
        .context("validate real Joplin source schema")?;
    migrations::run(&mcp_pool)
        .await
        .context("run MCP migrations for real Joplin validation")?;
    let login_email = resolve_live_login_email(&joplin_pool, &secrets.joplin_username).await?;

    let authenticator = HttpJoplinAuthenticator::new(
        url::Url::parse(&secrets.joplin_url).context("parse Joplin URL secret")?,
    );
    let session = authenticator
        .authenticate(&login_email, &secrets.joplin_password)
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "authenticate against real Joplin: {}",
                error.sanitized_detail()
            )
        })?;
    let joplin_user = resolve_joplin_user_by_id(&joplin_pool, &session.user_id)
        .await
        .context("resolve authenticated Joplin user from real database")?;
    let mcp_user = upsert_mcp_user_by_joplin_user(&mcp_pool, &joplin_user)
        .await
        .context("upsert MCP user for real Joplin validation")?;
    authenticator
        .invalidate_session(&session.id)
        .await
        .context("invalidate temporary Joplin session")?;

    let source = JoplinDbSource::new(joplin_pool.clone());
    let items = source
        .changed_items_since(&joplin_user.id, None)
        .await
        .context("read real Joplin items for authenticated owner")?;
    ensure!(
        items.iter().all(|item| item.owner_id == joplin_user.id),
        "Joplin source returned an item outside the authenticated owner"
    );
    let source_notes = items
        .iter()
        .filter(|item| {
            item.item_type == JoplinItemType::Note && !item.encrypted && !item.content.is_empty()
        })
        .count();
    let source_tags = items
        .iter()
        .filter(|item| item.item_type == JoplinItemType::Tag && !item.encrypted)
        .count();
    let source_note_tags = items
        .iter()
        .filter(|item| item.item_type == JoplinItemType::NoteTag && !item.encrypted)
        .count();
    ensure!(
        source_notes > 0,
        "real Joplin source has no unencrypted note content to validate"
    );

    let outcome = full_rebuild_user(&mcp_pool, &source, mcp_user.id, &joplin_user.id)
        .await
        .context("full rebuild real Joplin index")?;

    let indexed_notes: i64 =
        sqlx::query_scalar("SELECT count(*) FROM joplin_mcp.notes_index WHERE user_id = $1")
            .bind(mcp_user.id)
            .fetch_one(&mcp_pool)
            .await
            .context("count indexed real Joplin notes")?;
    ensure!(
        indexed_notes as usize == outcome.indexed_notes,
        "indexed note count differs from rebuild outcome"
    );
    ensure!(
        indexed_notes > 0,
        "live Joplin note content indexed zero rows"
    );

    let indexed_tags: i64 =
        sqlx::query_scalar("SELECT count(*) FROM joplin_mcp.tags_index WHERE user_id = $1")
            .bind(mcp_user.id)
            .fetch_one(&mcp_pool)
            .await
            .context("count indexed real Joplin tags")?;
    ensure!(
        indexed_tags as usize == outcome.indexed_tags,
        "indexed tag count differs from rebuild outcome"
    );
    if source_tags > 0 {
        ensure!(indexed_tags > 0, "live Joplin tags indexed zero rows");
    }

    let indexed_note_tags: i64 =
        sqlx::query_scalar("SELECT count(*) FROM joplin_mcp.note_tags_index WHERE user_id = $1")
            .bind(mcp_user.id)
            .fetch_one(&mcp_pool)
            .await
            .context("count indexed real Joplin tag edges")?;
    ensure!(
        indexed_note_tags as usize == outcome.indexed_note_tags,
        "indexed tag edge count differs from rebuild outcome"
    );
    if source_note_tags > 0 {
        ensure!(
            indexed_note_tags > 0,
            "live Joplin tag edges indexed zero rows"
        );
    }

    let encrypted_source_notes = items
        .iter()
        .filter(|item| item.item_type == JoplinItemType::Note && item.encrypted)
        .count();
    ensure!(
        outcome.skipped_encrypted >= encrypted_source_notes,
        "encrypted source notes were not reported as skipped"
    );

    assert_no_other_owner_notes(&mcp_pool, &source, mcp_user.id, &joplin_user.id).await?;
    assert_tag_edges_reference_indexed_rows(&mcp_pool, mcp_user.id).await?;
    assert_representative_note_matches_source(&mcp_pool, &source, mcp_user.id, &joplin_user.id)
        .await?;
    if indexed_note_tags > 0 {
        assert_representative_tag_edge_matches_source(&mcp_pool, mcp_user.id, &items).await?;
    }
    let _ = victorialogs
        .query_url()
        .context("build live VictoriaLogs query URL from SOPS victorialogs_url")?;
    let _ = victorialogs
        .otlp_logs_url()
        .context("build live VictoriaLogs OTLP URL from SOPS victorialogs_url")?;

    joplin_pool.close().await;
    mcp_pool.close().await;
    Ok(())
}

async fn resolve_live_login_email(pool: &sqlx::PgPool, username: &str) -> anyhow::Result<String> {
    let email_match: Option<String> = sqlx::query_scalar(
        r#"
        SELECT email
        FROM users
        WHERE email = $1
        LIMIT 1
        "#,
    )
    .bind(username)
    .fetch_optional(pool)
    .await
    .context("resolve SOPS joplin.username as email")?;
    if let Some(email) = email_match {
        return Ok(email);
    }

    let full_name_matches: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT email
        FROM users
        WHERE full_name = $1
        ORDER BY id
        LIMIT 2
        "#,
    )
    .bind(username)
    .fetch_all(pool)
    .await
    .context("resolve SOPS joplin.username as full_name")?;
    match full_name_matches.as_slice() {
        [email] => Ok(email.clone()),
        [] => anyhow::bail!("SOPS joplin.username did not match a Joplin email or full_name"),
        _ => anyhow::bail!("SOPS joplin.username matched more than one Joplin full_name"),
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate is inside workspace")
        .to_path_buf()
}

struct LiveSecrets {
    joplin_url: String,
    joplin_username: String,
    joplin_password: String,
    postgres_host: String,
    postgres_port: u16,
    postgres_joplin_database: String,
    postgres_joplin_user: String,
    postgres_joplin_password: String,
    postgres_mcp_database: String,
    postgres_mcp_user: String,
    postgres_mcp_password: String,
    victorialogs_url: String,
}

#[derive(Debug, FromRow)]
struct IndexedNote {
    joplin_item_id: String,
    joplin_id: String,
    title: String,
    body_text: String,
}

#[derive(Debug, FromRow)]
struct IndexedTagEdge {
    note_joplin_id: String,
    tag_joplin_id: String,
    tag_title: String,
}

impl LiveSecrets {
    fn load(path: &Path) -> anyhow::Result<Self> {
        let decrypted = decrypt_sops_file(path)?;
        for key in REQUIRED_SECRET_KEYS {
            let value = yq_value(&decrypted, key)?;
            ensure!(!value.trim().is_empty(), "required SOPS key is empty");
        }

        let postgres_port = yq_value(&decrypted, ".postgres.port")?
            .parse::<u16>()
            .context("parse postgres.port secret")?;

        Ok(Self {
            joplin_url: yq_value(&decrypted, ".joplin.url")?,
            joplin_username: yq_value(&decrypted, ".joplin.username")?,
            joplin_password: yq_value(&decrypted, ".joplin.password")?,
            postgres_host: live_postgres_host(&decrypted)?,
            postgres_port,
            postgres_joplin_database: yq_value(&decrypted, ".postgres.joplin_database")?,
            postgres_joplin_user: yq_value(&decrypted, ".postgres.joplin_user")?,
            postgres_joplin_password: yq_value(&decrypted, ".postgres.joplin_password")?,
            postgres_mcp_database: yq_value(&decrypted, ".postgres.mcp_database")?,
            postgres_mcp_user: yq_value(&decrypted, ".postgres.mcp_user")?,
            postgres_mcp_password: yq_value(&decrypted, ".postgres.mcp_password")?,
            victorialogs_url: yq_value(&decrypted, ".victorialogs_url")?,
        })
    }

    fn victorialogs_harness(&self) -> anyhow::Result<VictoriaLogsHarness> {
        VictoriaLogsHarness::new(&self.victorialogs_url, Duration::from_secs(5))
            .context("validate SOPS victorialogs_url")
    }

    fn validate_split_postgres_contract(&self) -> anyhow::Result<()> {
        ensure!(
            self.postgres_joplin_database != self.postgres_mcp_database,
            "postgres.joplin_database and postgres.mcp_database must differ"
        );
        ensure!(
            self.postgres_joplin_user != self.postgres_mcp_user,
            "postgres.joplin_user and postgres.mcp_user must differ"
        );
        Ok(())
    }

    fn joplin_db_options(&self) -> PgConnectOptions {
        PgConnectOptions::new()
            .host(&self.postgres_host)
            .port(self.postgres_port)
            .database(&self.postgres_joplin_database)
            .username(&self.postgres_joplin_user)
            .password(&self.postgres_joplin_password)
    }

    fn mcp_db_options(&self) -> PgConnectOptions {
        PgConnectOptions::new()
            .host(&self.postgres_host)
            .port(self.postgres_port)
            .database(&self.postgres_mcp_database)
            .username(&self.postgres_mcp_user)
            .password(&self.postgres_mcp_password)
    }
}

fn live_postgres_host(decrypted: &[u8]) -> anyhow::Result<String> {
    match env::var(LIVE_POSTGRES_HOST_OVERRIDE_ENV) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        _ => yq_value(decrypted, ".postgres.host"),
    }
}

fn decrypt_sops_file(path: &Path) -> anyhow::Result<Vec<u8>> {
    ensure!(path.exists(), "SOPS secrets file is missing");
    let output = Command::new("sops")
        .arg("-d")
        .arg(path)
        .output()
        .context("run sops decrypt")?;
    ensure!(output.status.success(), "SOPS decrypt failed");
    Ok(output.stdout)
}

fn yq_value(input: &[u8], expression: &str) -> anyhow::Result<String> {
    let mut child = Command::new("yq")
        .arg("-e")
        .arg("-r")
        .arg(expression)
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("run yq for SOPS key validation")?;

    {
        let stdin = child.stdin.as_mut().context("open yq stdin")?;
        use std::io::Write;
        stdin.write_all(input).context("send SOPS data to yq")?;
    }

    let output = child
        .wait_with_output()
        .context("read yq output for SOPS key validation")?;
    ensure!(output.status.success(), "required SOPS key is missing");
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_string())
        .context("decode SOPS key value")
}

async fn connect_pool(label: &str, options: PgConnectOptions) -> anyhow::Result<sqlx::PgPool> {
    PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(options)
        .await
        .map_err(|_| anyhow::anyhow!("connect to configured {label} Postgres database failed"))
}

async fn assert_no_other_owner_notes(
    mcp_pool: &sqlx::PgPool,
    source: &JoplinDbSource,
    mcp_user_id: uuid::Uuid,
    joplin_user_id: &str,
) -> anyhow::Result<()> {
    let indexed_item_ids: Vec<String> = sqlx::query_scalar(
        "SELECT joplin_item_id FROM joplin_mcp.notes_index WHERE user_id = $1 LIMIT 100",
    )
    .bind(mcp_user_id)
    .fetch_all(mcp_pool)
    .await
    .context("load indexed note source IDs")?;

    for item_id in indexed_item_ids {
        let item = source
            .item_by_id(joplin_user_id, &item_id)
            .await
            .context("load indexed note source row")?
            .context("indexed source row is missing")?;
        ensure!(
            item.owner_id == joplin_user_id,
            "indexed notes include another owner"
        );
    }
    Ok(())
}

async fn assert_tag_edges_reference_indexed_rows(
    mcp_pool: &sqlx::PgPool,
    mcp_user_id: uuid::Uuid,
) -> anyhow::Result<()> {
    let dangling = sqlx::query(
        r#"
        SELECT edge.note_joplin_id, edge.tag_joplin_id
        FROM joplin_mcp.note_tags_index edge
        LEFT JOIN joplin_mcp.notes_index notes
          ON notes.user_id = edge.user_id
         AND notes.joplin_id = edge.note_joplin_id
        LEFT JOIN joplin_mcp.tags_index tags
          ON tags.user_id = edge.user_id
         AND tags.joplin_id = edge.tag_joplin_id
        WHERE edge.user_id = $1
          AND (notes.joplin_id IS NULL OR tags.joplin_id IS NULL)
        LIMIT 1
        "#,
    )
    .bind(mcp_user_id)
    .fetch_all(mcp_pool)
    .await
    .context("check real Joplin tag edges")?;
    ensure!(
        dangling.is_empty(),
        "tag filter edge references missing index rows"
    );

    Ok(())
}

async fn assert_representative_note_matches_source(
    mcp_pool: &sqlx::PgPool,
    source: &JoplinDbSource,
    mcp_user_id: uuid::Uuid,
    joplin_user_id: &str,
) -> anyhow::Result<()> {
    let indexed_note = sqlx::query_as::<_, IndexedNote>(
        r#"
        SELECT joplin_item_id, joplin_id, title, body_text
        FROM joplin_mcp.notes_index
        WHERE user_id = $1
        ORDER BY updated_time DESC, joplin_item_id ASC
        LIMIT 1
        "#,
    )
    .bind(mcp_user_id)
    .fetch_optional(mcp_pool)
    .await
    .context("load representative indexed note")?
    .context("no representative indexed note exists")?;
    let source_item = source
        .item_by_id(joplin_user_id, &indexed_note.joplin_item_id)
        .await
        .context("load representative source note")?
        .context("representative source note is missing")?;
    let source_body =
        std::str::from_utf8(&source_item.content).context("representative source note is UTF-8")?;

    ensure!(
        source_item.jop_id == indexed_note.joplin_id,
        "representative indexed note has wrong Joplin ID"
    );
    ensure!(
        source_item.name == indexed_note.title,
        "representative indexed note has wrong title"
    );
    ensure!(
        source_body == indexed_note.body_text,
        "representative indexed note has wrong body text"
    );
    Ok(())
}

async fn assert_representative_tag_edge_matches_source(
    mcp_pool: &sqlx::PgPool,
    mcp_user_id: uuid::Uuid,
    items: &[joplin_mcpd::indexer::source::JoplinItem],
) -> anyhow::Result<()> {
    let edge = sqlx::query_as::<_, IndexedTagEdge>(
        r#"
        SELECT edge.note_joplin_id, edge.tag_joplin_id, tags.title AS tag_title
        FROM joplin_mcp.note_tags_index edge
        JOIN joplin_mcp.tags_index tags
          ON tags.user_id = edge.user_id
         AND tags.joplin_id = edge.tag_joplin_id
        WHERE edge.user_id = $1
        ORDER BY edge.note_joplin_id ASC, edge.tag_joplin_id ASC
        LIMIT 1
        "#,
    )
    .bind(mcp_user_id)
    .fetch_optional(mcp_pool)
    .await
    .context("load representative indexed tag edge")?
    .context("no representative indexed tag edge exists")?;
    let source_tag = items
        .iter()
        .find(|item| item.item_type == JoplinItemType::Tag && item.jop_id == edge.tag_joplin_id)
        .context("representative tag edge source tag is missing")?;
    let source_edge = items
        .iter()
        .find(|item| {
            item.item_type == JoplinItemType::NoteTag
                && loose_item_value(item, "note_id").as_deref()
                    == Some(edge.note_joplin_id.as_str())
                && loose_item_value(item, "tag_id").as_deref() == Some(edge.tag_joplin_id.as_str())
        })
        .context("representative tag edge source row is missing")?;

    ensure!(
        source_tag.name == edge.tag_title,
        "representative indexed tag has wrong title"
    );
    ensure!(
        !source_edge.encrypted,
        "representative indexed tag edge came from encrypted source row"
    );
    Ok(())
}

fn loose_item_value(item: &joplin_mcpd::indexer::source::JoplinItem, key: &str) -> Option<String> {
    let content = std::str::from_utf8(&item.content).ok()?;
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(content)
        && let Some(object_value) = value.get(key)
    {
        return match object_value {
            serde_json::Value::String(value) => Some(value.clone()),
            serde_json::Value::Null => Some(String::new()),
            _ => Some(object_value.to_string()),
        };
    }

    content.lines().find_map(|line| {
        let (line_key, value) = line.split_once(':')?;
        (line_key.trim() == key).then(|| value.trim().to_string())
    })
}
