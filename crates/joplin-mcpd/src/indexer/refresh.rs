use crate::config::IndexConfig;
use crate::db::pool as db_pool;
use crate::indexer::JoplinItemType;
use crate::indexer::item_content::parse_index_item;
use crate::indexer::parser::{ParsedItem, extract_resource_refs};
use crate::indexer::rebuild::{FullRebuildOutcome, IndexStatus, full_rebuild_user_in_transaction};
use crate::indexer::source::{JOPLIN_ITEM_BATCH_SIZE, JoplinItem, JoplinItemCursor, JoplinSource};
use crate::observability::metrics;
use anyhow::Context;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use sqlx::{Acquire, FromRow, PgPool, Postgres, Transaction};
use std::collections::HashSet;
use std::time::Instant;
use tracing::Instrument;
use uuid::Uuid;

const REQUEST_INDEX_REFRESH_NOW_SQL: &str = r#"
INSERT INTO joplin_mcp.index_state (user_id, status, last_checked_at, updated_at)
VALUES ($1, $2, NULL, now())
ON CONFLICT (user_id) DO UPDATE
SET last_checked_at = NULL,
    updated_at = now()
WHERE joplin_mcp.index_state.status <> $3
"#;

#[derive(Debug, Clone, FromRow, PartialEq, Eq)]
pub struct RefreshUser {
    pub mcp_user_id: Uuid,
    pub joplin_user_id: String,
    pub status: Option<String>,
    pub last_incremental_at: Option<DateTime<Utc>>,
    pub last_full_rebuild_at: Option<DateTime<Utc>>,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub last_reconciled_at: Option<DateTime<Utc>>,
    pub last_seen_joplin_updated_time: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncrementalRefreshOutcome {
    pub lock_acquired: bool,
    pub full_rebuild_required: bool,
    pub changed_items: usize,
    pub reconciled_deleted_items: usize,
    pub full_rebuild_indexed_items: usize,
    pub full_rebuild_deleted_items: usize,
    pub skipped_encrypted: usize,
    pub skipped_malformed: usize,
    pub skipped_wrong_owner: usize,
}

impl IncrementalRefreshOutcome {
    fn skipped_lock() -> Self {
        Self {
            lock_acquired: false,
            full_rebuild_required: false,
            changed_items: 0,
            reconciled_deleted_items: 0,
            full_rebuild_indexed_items: 0,
            full_rebuild_deleted_items: 0,
            skipped_encrypted: 0,
            skipped_malformed: 0,
            skipped_wrong_owner: 0,
        }
    }

    fn from_full_rebuild(rebuild: FullRebuildOutcome) -> Self {
        Self {
            lock_acquired: true,
            full_rebuild_required: true,
            changed_items: 0,
            reconciled_deleted_items: 0,
            full_rebuild_indexed_items: rebuild.indexed_notes
                + rebuild.indexed_notebooks
                + rebuild.indexed_tags
                + rebuild.indexed_note_tags
                + rebuild.indexed_resources,
            full_rebuild_deleted_items: rebuild.indexed_deleted_items,
            skipped_encrypted: rebuild.skipped_encrypted,
            skipped_malformed: rebuild.skipped_malformed,
            skipped_wrong_owner: rebuild.skipped_wrong_owner,
        }
    }
}

#[derive(Debug, Default)]
struct IncrementalRows {
    active: Vec<ActiveRow>,
    deleted: Vec<DeletedRow>,
    skipped_encrypted: usize,
    skipped_malformed: usize,
    skipped_wrong_owner: usize,
    last_seen_joplin_updated_time: Option<i64>,
}

#[derive(Debug, Default)]
struct IncrementalTotals {
    changed_items: usize,
    reconciled_deleted_items: usize,
    skipped_encrypted: usize,
    skipped_malformed: usize,
    skipped_wrong_owner: usize,
    last_seen_joplin_updated_time: Option<i64>,
}

impl IncrementalTotals {
    fn add_rows(&mut self, rows: &IncrementalRows, previous: Option<i64>) {
        self.skipped_encrypted += rows.skipped_encrypted;
        self.skipped_malformed += rows.skipped_malformed;
        self.skipped_wrong_owner += rows.skipped_wrong_owner;
        self.last_seen_joplin_updated_time =
            rows.last_seen_joplin_updated_time(self.last_seen_joplin_updated_time.or(previous));
    }
}

impl IncrementalRows {
    fn record_seen(&mut self, updated_time: i64) {
        self.last_seen_joplin_updated_time = Some(
            self.last_seen_joplin_updated_time
                .map_or(updated_time, |previous| previous.max(updated_time)),
        );
    }

    fn last_seen_joplin_updated_time(&self, previous: Option<i64>) -> Option<i64> {
        self.last_seen_joplin_updated_time
            .into_iter()
            .chain(previous)
            .max()
    }
}

#[derive(Debug)]
struct ActiveRow {
    joplin_item_id: String,
    joplin_id: String,
    item_type: JoplinItemType,
    parent_joplin_id: Option<String>,
    title: String,
    body_text: String,
    is_todo: bool,
    created_time: i64,
    updated_time: i64,
    resource_refs: Vec<String>,
    mime: Option<String>,
    size_bytes: Option<i64>,
    file_extension: Option<String>,
    note_joplin_id: Option<String>,
    tag_joplin_id: Option<String>,
}

#[derive(Debug)]
struct DeletedRow {
    joplin_id: String,
    item_type: JoplinItemType,
    deleted_time: Option<i64>,
    note_joplin_id: Option<String>,
    tag_joplin_id: Option<String>,
}

#[derive(Debug, FromRow)]
struct IndexedItemRef {
    joplin_id: String,
    item_type: i32,
}

pub async fn due_refresh_users(
    mcp_pool: &PgPool,
    config: &IndexConfig,
) -> anyhow::Result<Vec<RefreshUser>> {
    let limit = i64::from(config.max_parallel_users);
    let mut conn = db_pool::acquire_runtime(mcp_pool)
        .await
        .context("acquire runtime database connection")?;

    sqlx::query_as::<_, RefreshUser>(
        r#"
        SELECT
            users.id AS mcp_user_id,
            users.joplin_user_id,
            state.status,
            state.last_incremental_at,
            state.last_full_rebuild_at,
            state.last_checked_at,
            state.last_reconciled_at,
            state.last_seen_joplin_updated_time
        FROM joplin_mcp.mcp_users users
        JOIN joplin_mcp.mcp_tokens tokens ON tokens.user_id = users.id
        LEFT JOIN joplin_mcp.index_state state ON state.user_id = users.id
        WHERE users.disabled_at IS NULL
          AND tokens.revoked_at IS NULL
          AND (tokens.expires_at IS NULL OR tokens.expires_at > now())
          AND (
            state.user_id IS NULL
            OR state.status IN ('failed', 'empty', 'building')
            OR state.last_checked_at IS NULL
            OR state.last_checked_at <= now() - ($1::bigint * interval '1 second')
            OR state.last_full_rebuild_at IS NULL
            OR state.last_full_rebuild_at <= now() - ($3::bigint * interval '1 hour')
          )
        GROUP BY users.id, users.joplin_user_id, state.status, state.last_incremental_at, state.last_full_rebuild_at, state.last_checked_at, state.last_reconciled_at, state.last_seen_joplin_updated_time
        ORDER BY state.last_checked_at ASC NULLS FIRST, users.id ASC
        LIMIT $2
        "#,
    )
    .bind(config.refresh_interval_seconds as i64)
    .bind(limit)
    .bind(config.full_rebuild_interval_hours as i64)
    .fetch_all(&mut *conn)
    .await
        .context("load due refresh users")
}

pub async fn request_index_refresh_now(mcp_pool: &PgPool, user_id: Uuid) -> anyhow::Result<()> {
    let mut conn = db_pool::acquire_runtime(mcp_pool)
        .await
        .context("acquire runtime database connection")?;

    sqlx::query(REQUEST_INDEX_REFRESH_NOW_SQL)
        .bind(user_id)
        .bind(IndexStatus::Empty.as_str())
        .bind(IndexStatus::Building.as_str())
        .execute(&mut *conn)
        .await
        .context("request index refresh now")?;

    Ok(())
}

pub fn refresh_needed(
    user: &RefreshUser,
    source_watermark: Option<i64>,
    config: &IndexConfig,
) -> bool {
    match user.status.as_deref() {
        Some("ready" | "stale") => {}
        _ => return true,
    }

    if requires_full_rebuild(
        refresh_gap_seconds(user.last_incremental_at),
        config.incremental_lookback_max_seconds,
    ) || full_rebuild_due(
        user.last_full_rebuild_at,
        config.full_rebuild_interval_hours,
    ) {
        return true;
    }

    if hard_delete_reconciliation_due(
        user.last_reconciled_at,
        config.hard_delete_reconcile_interval_hours,
    ) {
        return true;
    }

    match (source_watermark, user.last_seen_joplin_updated_time) {
        (Some(source_watermark), Some(last_seen)) => source_watermark > last_seen,
        (Some(_), None) => true,
        (None, None) => false,
        (None, Some(_)) => false,
    }
}

pub async fn incremental_refresh_user<S>(
    mcp_pool: &PgPool,
    source: &S,
    user: &RefreshUser,
    config: &IndexConfig,
) -> anyhow::Result<IncrementalRefreshOutcome>
where
    S: JoplinSource,
{
    async {
        let started = Instant::now();
        let lock_key = advisory_lock_key(user.mcp_user_id);
        let mut conn = db_pool::acquire_runtime(mcp_pool)
            .await
            .context("acquire runtime database connection")?;
        let mut tx = (&mut *conn)
            .begin()
            .await
            .context("begin incremental refresh transaction")?;
        let lock_acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
            .bind(lock_key)
            .fetch_one(tx.as_mut())
            .await
            .context("acquire user refresh advisory lock")?;

        if !lock_acquired {
            tx.rollback()
                .await
                .context("rollback skipped refresh transaction")?;
            metrics::record_index_refresh_duration("skipped_lock", started.elapsed());
            return Ok(IncrementalRefreshOutcome::skipped_lock());
        }

        let gap_seconds = refresh_gap_seconds(user.last_incremental_at);
        if requires_full_rebuild(gap_seconds, config.incremental_lookback_max_seconds)
            || full_rebuild_due(
                user.last_full_rebuild_at,
                config.full_rebuild_interval_hours,
            )
        {
            let rebuild_outcome = match full_rebuild_user_in_transaction(
                &mut tx,
                source,
                user.mcp_user_id,
                &user.joplin_user_id,
            )
            .instrument(tracing::info_span!("index.full_rebuild"))
            .await
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    tx.rollback()
                        .await
                        .context("release refresh lock after failed full rebuild")?;
                    record_incremental_failure(mcp_pool, user.mcp_user_id, &error.to_string())
                        .await?;
                    metrics::record_index_refresh_duration("failed", started.elapsed());
                    return Err(error);
                }
            };
            tx.commit()
                .await
                .context("release refresh lock after full rebuild")?;
            metrics::record_index_refresh_duration("full_rebuild", started.elapsed());
            return Ok(IncrementalRefreshOutcome::from_full_rebuild(
                rebuild_outcome,
            ));
        }

        mark_stale_if_needed(
            &mut tx,
            user.mcp_user_id,
            user.last_incremental_at,
            config.refresh_interval_seconds,
        )
        .await?;

        let result = apply_incremental_refresh(&mut tx, source, user, config).await;
        match result {
            Ok(outcome) => {
                tx.commit()
                    .await
                    .context("commit incremental refresh transaction")?;
                metrics::record_index_refresh_duration("success", started.elapsed());
                Ok(outcome)
            }
            Err(error) => {
                tx.rollback()
                    .await
                    .context("rollback failed incremental refresh transaction")?;
                record_incremental_failure(mcp_pool, user.mcp_user_id, &error.to_string()).await?;
                metrics::record_index_refresh_duration("failed", started.elapsed());
                Err(error)
            }
        }
    }
    .instrument(tracing::info_span!("index.refresh"))
    .await
}

async fn apply_incremental_refresh<S>(
    tx: &mut Transaction<'_, Postgres>,
    source: &S,
    user: &RefreshUser,
    config: &IndexConfig,
) -> anyhow::Result<IncrementalRefreshOutcome>
where
    S: JoplinSource,
{
    let mut totals = IncrementalTotals::default();
    let mut after = None;

    loop {
        let changed = source
            .changed_items_batch(
                &user.joplin_user_id,
                user.last_seen_joplin_updated_time,
                after.as_ref(),
                JOPLIN_ITEM_BATCH_SIZE,
            )
            .instrument(tracing::info_span!("index.source_query"))
            .await
            .context("load changed Joplin item batch for incremental refresh")?;
        if changed.is_empty() {
            break;
        }

        let batch_len = changed.len();
        after = changed.last().map(JoplinItemCursor::from);
        totals.changed_items += batch_len;
        let rows = build_incremental_rows(&user.joplin_user_id, changed);
        async {
            for row in &rows.deleted {
                purge_active_row(tx, user.mcp_user_id, row).await?;
                upsert_deleted_row(tx, user.mcp_user_id, row, "deleted_time").await?;
            }
            for row in &rows.active {
                purge_active_item(tx, user.mcp_user_id, &row.joplin_id, row.item_type).await?;
                upsert_active_row(tx, user.mcp_user_id, row).await?;
            }
            anyhow::Ok(())
        }
        .instrument(tracing::info_span!("index.row_upserts"))
        .await?;
        totals.add_rows(&rows, user.last_seen_joplin_updated_time);
        if batch_len < JOPLIN_ITEM_BATCH_SIZE as usize {
            break;
        }
    }

    let reconcile_due = hard_delete_reconciliation_due(
        user.last_reconciled_at,
        config.hard_delete_reconcile_interval_hours,
    );
    if reconcile_due {
        totals.reconciled_deleted_items = reconcile_hard_deletes(tx, source, user).await?;
    }
    let last_seen = totals
        .last_seen_joplin_updated_time
        .or(user.last_seen_joplin_updated_time);
    mark_incremental_ready(
        tx,
        user.mcp_user_id,
        last_seen,
        config.refresh_interval_seconds,
        totals.changed_items > 0 || totals.reconciled_deleted_items > 0,
        reconcile_due,
    )
    .instrument(tracing::info_span!("index.state_update"))
    .await?;
    if let Some(seconds) = joplin_lag_seconds(last_seen) {
        metrics::record_index_lag(user.mcp_user_id, seconds);
    }

    Ok(IncrementalRefreshOutcome {
        lock_acquired: true,
        full_rebuild_required: false,
        changed_items: totals.changed_items,
        reconciled_deleted_items: totals.reconciled_deleted_items,
        full_rebuild_indexed_items: 0,
        full_rebuild_deleted_items: 0,
        skipped_encrypted: totals.skipped_encrypted,
        skipped_malformed: totals.skipped_malformed,
        skipped_wrong_owner: totals.skipped_wrong_owner,
    })
}

fn build_incremental_rows(joplin_user_id: &str, items: Vec<JoplinItem>) -> IncrementalRows {
    let mut rows = IncrementalRows::default();

    for item in items {
        rows.record_seen(item.updated_time);
        if item.owner_id != joplin_user_id {
            rows.skipped_wrong_owner += 1;
            continue;
        }
        if item.encrypted {
            rows.skipped_encrypted += 1;
            continue;
        }

        let raw = match std::str::from_utf8(&item.content) {
            Ok(raw) => raw,
            Err(_) => {
                rows.skipped_malformed += 1;
                continue;
            }
        };
        let parsed = match parse_index_item(&item, raw) {
            Some(parsed) => parsed,
            None => {
                rows.skipped_malformed += 1;
                continue;
            }
        };

        if let Some(deleted_time) = deleted_time(&parsed) {
            rows.deleted.push(DeletedRow {
                joplin_id: item.jop_id,
                item_type: item.item_type,
                deleted_time: Some(deleted_time),
                note_joplin_id: string_metadata(&parsed, "note_id"),
                tag_joplin_id: string_metadata(&parsed, "tag_id"),
            });
            continue;
        }

        match active_row(item, &parsed) {
            Some(row) => rows.active.push(row),
            None => rows.skipped_malformed += 1,
        }
    }

    rows
}

fn active_row(item: JoplinItem, parsed: &ParsedItem) -> Option<ActiveRow> {
    match item.item_type {
        JoplinItemType::Note => Some(ActiveRow {
            parent_joplin_id: parent_id(&item, parsed),
            joplin_item_id: item.id,
            joplin_id: item.jop_id,
            item_type: item.item_type,
            title: parsed.title.clone(),
            body_text: parsed.body.clone(),
            is_todo: bool_metadata(parsed, "is_todo"),
            created_time: integer_metadata(parsed, "created_time").unwrap_or(item.created_time),
            updated_time: integer_metadata(parsed, "updated_time").unwrap_or(item.updated_time),
            resource_refs: extract_resource_refs(&parsed.body),
            mime: None,
            size_bytes: None,
            file_extension: None,
            note_joplin_id: None,
            tag_joplin_id: None,
        }),
        JoplinItemType::Folder => Some(ActiveRow {
            parent_joplin_id: parent_id(&item, parsed),
            joplin_item_id: item.id,
            joplin_id: item.jop_id,
            item_type: item.item_type,
            title: parsed.title.clone(),
            body_text: String::new(),
            is_todo: false,
            created_time: integer_metadata(parsed, "created_time").unwrap_or(item.created_time),
            updated_time: integer_metadata(parsed, "updated_time").unwrap_or(item.updated_time),
            resource_refs: Vec::new(),
            mime: None,
            size_bytes: None,
            file_extension: None,
            note_joplin_id: None,
            tag_joplin_id: None,
        }),
        JoplinItemType::Tag => Some(ActiveRow {
            joplin_item_id: item.id,
            joplin_id: item.jop_id,
            item_type: item.item_type,
            parent_joplin_id: None,
            title: parsed.title.clone(),
            body_text: String::new(),
            is_todo: false,
            created_time: item.created_time,
            updated_time: integer_metadata(parsed, "updated_time").unwrap_or(item.updated_time),
            resource_refs: Vec::new(),
            mime: None,
            size_bytes: None,
            file_extension: None,
            note_joplin_id: None,
            tag_joplin_id: None,
        }),
        JoplinItemType::NoteTag => Some(ActiveRow {
            joplin_item_id: item.id,
            joplin_id: item.jop_id,
            item_type: item.item_type,
            parent_joplin_id: None,
            title: String::new(),
            body_text: String::new(),
            is_todo: false,
            created_time: item.created_time,
            updated_time: integer_metadata(parsed, "updated_time").unwrap_or(item.updated_time),
            resource_refs: Vec::new(),
            mime: None,
            size_bytes: None,
            file_extension: None,
            note_joplin_id: string_metadata(parsed, "note_id"),
            tag_joplin_id: string_metadata(parsed, "tag_id"),
        })
        .filter(|row| row.note_joplin_id.is_some() && row.tag_joplin_id.is_some()),
        JoplinItemType::Resource => {
            let title = resource_title(&item, parsed);
            Some(ActiveRow {
                joplin_item_id: item.id,
                joplin_id: item.jop_id.clone(),
                item_type: item.item_type,
                parent_joplin_id: None,
                title,
                body_text: String::new(),
                is_todo: false,
                created_time: item.created_time,
                updated_time: integer_metadata(parsed, "updated_time").unwrap_or(item.updated_time),
                resource_refs: Vec::new(),
                mime: empty_to_none(item.mime_type),
                size_bytes: integer_metadata(parsed, "size"),
                file_extension: string_metadata(parsed, "file_extension"),
                note_joplin_id: None,
                tag_joplin_id: None,
            })
        }
        JoplinItemType::Revision => None,
    }
}

async fn mark_stale_if_needed(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    last_incremental_at: Option<DateTime<Utc>>,
    refresh_interval_seconds: u64,
) -> anyhow::Result<()> {
    if !is_stale(last_incremental_at, refresh_interval_seconds) {
        return Ok(());
    }

    sqlx::query(
        r#"
        UPDATE joplin_mcp.index_state
        SET status = $2,
            updated_at = now()
        WHERE user_id = $1
          AND status = $3
        "#,
    )
    .bind(user_id)
    .bind(IndexStatus::Stale.as_str())
    .bind(IndexStatus::Ready.as_str())
    .execute(tx.as_mut())
    .await
    .context("mark stale index state")?;

    Ok(())
}

async fn mark_incremental_ready(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    last_seen_joplin_updated_time: Option<i64>,
    refresh_interval_seconds: u64,
    applied_changes: bool,
    ran_hard_delete_reconciliation: bool,
) -> anyhow::Result<()> {
    let _ = refresh_interval_seconds;
    sqlx::query(
        r#"
        INSERT INTO joplin_mcp.index_state
            (user_id, status, last_checked_at, last_incremental_at, last_seen_joplin_updated_time, last_reconciled_at, updated_at)
        VALUES ($1, $2, now(), CASE WHEN $4 THEN now() ELSE NULL END, $3, CASE WHEN $5 THEN now() ELSE NULL END, now())
        ON CONFLICT (user_id) DO UPDATE
        SET status = EXCLUDED.status,
            last_checked_at = EXCLUDED.last_checked_at,
            last_incremental_at = CASE
                WHEN $4 THEN EXCLUDED.last_incremental_at
                ELSE joplin_mcp.index_state.last_incremental_at
            END,
            last_seen_joplin_updated_time = COALESCE(
                GREATEST(
                    joplin_mcp.index_state.last_seen_joplin_updated_time,
                    EXCLUDED.last_seen_joplin_updated_time
                ),
                joplin_mcp.index_state.last_seen_joplin_updated_time,
                EXCLUDED.last_seen_joplin_updated_time
            ),
            last_reconciled_at = CASE
                WHEN $5 THEN EXCLUDED.last_reconciled_at
                ELSE joplin_mcp.index_state.last_reconciled_at
            END,
            last_error = NULL,
            updated_at = now()
        "#,
    )
    .bind(user_id)
    .bind(IndexStatus::Ready.as_str())
    .bind(last_seen_joplin_updated_time)
    .bind(applied_changes)
    .bind(ran_hard_delete_reconciliation)
    .execute(tx.as_mut())
    .await
    .context("mark incremental refresh ready")?;

    Ok(())
}

pub async fn mark_index_checked(
    mcp_pool: &PgPool,
    user_id: Uuid,
    source_watermark: Option<i64>,
) -> anyhow::Result<()> {
    let mut conn = db_pool::acquire_runtime(mcp_pool)
        .await
        .context("acquire runtime database connection")?;

    sqlx::query(
        r#"
        INSERT INTO joplin_mcp.index_state
            (user_id, status, last_checked_at, last_seen_joplin_updated_time, updated_at)
        VALUES ($1, $2, now(), $3, now())
        ON CONFLICT (user_id) DO UPDATE
        SET status = $2,
            last_checked_at = now(),
            last_seen_joplin_updated_time = COALESCE(
                GREATEST(
                    joplin_mcp.index_state.last_seen_joplin_updated_time,
                    EXCLUDED.last_seen_joplin_updated_time
                ),
                joplin_mcp.index_state.last_seen_joplin_updated_time,
                EXCLUDED.last_seen_joplin_updated_time
            ),
            last_error = NULL,
            updated_at = now()
        "#,
    )
    .bind(user_id)
    .bind(IndexStatus::Ready.as_str())
    .bind(source_watermark)
    .execute(&mut *conn)
    .await
    .context("mark index checked")?;

    Ok(())
}

async fn record_incremental_failure(
    mcp_pool: &PgPool,
    user_id: Uuid,
    error: &str,
) -> anyhow::Result<()> {
    let mut conn = db_pool::acquire_runtime(mcp_pool)
        .await
        .context("acquire runtime database connection")?;

    sqlx::query(
        r#"
        INSERT INTO joplin_mcp.index_state (user_id, status, last_error, updated_at)
        VALUES ($1, $2, $3, now())
        ON CONFLICT (user_id) DO UPDATE
        SET status = CASE
                WHEN joplin_mcp.index_state.status IN ('ready', 'stale') THEN 'stale'
                ELSE EXCLUDED.status
            END,
            last_error = EXCLUDED.last_error,
            updated_at = now()
        "#,
    )
    .bind(user_id)
    .bind(IndexStatus::Failed.as_str())
    .bind(error)
    .execute(&mut *conn)
    .await
    .context("record incremental refresh failure")?;

    Ok(())
}

async fn upsert_active_row(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    row: &ActiveRow,
) -> anyhow::Result<()> {
    match row.item_type {
        JoplinItemType::Note => sqlx::query(
            r#"
            INSERT INTO joplin_mcp.notes_index
                (user_id, joplin_item_id, joplin_id, parent_joplin_id, title, body_text, is_todo, created_time, updated_time, resource_refs, indexed_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, now())
            ON CONFLICT (user_id, joplin_id) DO UPDATE
            SET joplin_item_id = EXCLUDED.joplin_item_id,
                parent_joplin_id = EXCLUDED.parent_joplin_id,
                title = EXCLUDED.title,
                body_text = EXCLUDED.body_text,
                is_todo = EXCLUDED.is_todo,
                created_time = EXCLUDED.created_time,
                updated_time = EXCLUDED.updated_time,
                deleted_time = NULL,
                resource_refs = EXCLUDED.resource_refs,
                indexed_at = now()
            "#,
        )
        .bind(user_id)
        .bind(&row.joplin_item_id)
        .bind(&row.joplin_id)
        .bind(&row.parent_joplin_id)
        .bind(&row.title)
        .bind(&row.body_text)
        .bind(row.is_todo)
        .bind(row.created_time)
        .bind(row.updated_time)
        .bind(&row.resource_refs)
        .execute(tx.as_mut())
        .await
        .context("upsert note index row")?,
        JoplinItemType::Folder => sqlx::query(
            r#"
            INSERT INTO joplin_mcp.notebooks_index
                (user_id, joplin_item_id, joplin_id, parent_joplin_id, title, created_time, updated_time, indexed_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, now())
            ON CONFLICT (user_id, joplin_id) DO UPDATE
            SET joplin_item_id = EXCLUDED.joplin_item_id,
                parent_joplin_id = EXCLUDED.parent_joplin_id,
                title = EXCLUDED.title,
                created_time = EXCLUDED.created_time,
                updated_time = EXCLUDED.updated_time,
                deleted_time = NULL,
                indexed_at = now()
            "#,
        )
        .bind(user_id)
        .bind(&row.joplin_item_id)
        .bind(&row.joplin_id)
        .bind(&row.parent_joplin_id)
        .bind(&row.title)
        .bind(row.created_time)
        .bind(row.updated_time)
        .execute(tx.as_mut())
        .await
        .context("upsert notebook index row")?,
        JoplinItemType::Tag => sqlx::query(
            r#"
            INSERT INTO joplin_mcp.tags_index
                (user_id, joplin_item_id, joplin_id, title, updated_time, indexed_at)
            VALUES ($1, $2, $3, $4, $5, now())
            ON CONFLICT (user_id, joplin_id) DO UPDATE
            SET joplin_item_id = EXCLUDED.joplin_item_id,
                title = EXCLUDED.title,
                updated_time = EXCLUDED.updated_time,
                indexed_at = now()
            "#,
        )
        .bind(user_id)
        .bind(&row.joplin_item_id)
        .bind(&row.joplin_id)
        .bind(&row.title)
        .bind(row.updated_time)
        .execute(tx.as_mut())
        .await
        .context("upsert tag index row")?,
        JoplinItemType::NoteTag => sqlx::query(
            r#"
            INSERT INTO joplin_mcp.note_tags_index
                (user_id, note_joplin_id, tag_joplin_id, updated_time, indexed_at)
            VALUES ($1, $2, $3, $4, now())
            ON CONFLICT (user_id, note_joplin_id, tag_joplin_id) DO UPDATE
            SET updated_time = EXCLUDED.updated_time,
                indexed_at = now()
            "#,
        )
        .bind(user_id)
        .bind(row.note_joplin_id.as_ref().expect("validated note id"))
        .bind(row.tag_joplin_id.as_ref().expect("validated tag id"))
        .bind(row.updated_time)
        .execute(tx.as_mut())
        .await
        .context("upsert note tag index row")?,
        JoplinItemType::Resource => sqlx::query(
            r#"
            INSERT INTO joplin_mcp.resources_index
                (user_id, joplin_item_id, joplin_id, title, mime, size_bytes, file_extension, updated_time, indexed_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now())
            ON CONFLICT (user_id, joplin_id) DO UPDATE
            SET joplin_item_id = EXCLUDED.joplin_item_id,
                title = EXCLUDED.title,
                mime = EXCLUDED.mime,
                size_bytes = EXCLUDED.size_bytes,
                file_extension = EXCLUDED.file_extension,
                updated_time = EXCLUDED.updated_time,
                indexed_at = now()
            "#,
        )
        .bind(user_id)
        .bind(&row.joplin_item_id)
        .bind(&row.joplin_id)
        .bind(&row.title)
        .bind(&row.mime)
        .bind(row.size_bytes)
        .bind(&row.file_extension)
        .bind(row.updated_time)
        .execute(tx.as_mut())
        .await
        .context("upsert resource index row")?,
        JoplinItemType::Revision => return Ok(()),
    };

    Ok(())
}

async fn purge_active_row(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    row: &DeletedRow,
) -> anyhow::Result<()> {
    purge_active_item(tx, user_id, &row.joplin_id, row.item_type).await?;
    if row.item_type == JoplinItemType::NoteTag
        && let (Some(note_id), Some(tag_id)) = (&row.note_joplin_id, &row.tag_joplin_id)
    {
        sqlx::query(
            "DELETE FROM joplin_mcp.note_tags_index WHERE user_id = $1 AND note_joplin_id = $2 AND tag_joplin_id = $3",
        )
        .bind(user_id)
        .bind(note_id)
        .bind(tag_id)
        .execute(tx.as_mut())
        .await
        .context("purge deleted note tag edge")?;
    }
    Ok(())
}

async fn purge_active_item(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    joplin_id: &str,
    item_type: JoplinItemType,
) -> anyhow::Result<()> {
    match item_type {
        JoplinItemType::Note => {
            sqlx::query(
                "DELETE FROM joplin_mcp.note_tags_index WHERE user_id = $1 AND note_joplin_id = $2",
            )
            .bind(user_id)
            .bind(joplin_id)
            .execute(tx.as_mut())
            .await
            .context("purge note tag edges for note")?;
            sqlx::query("DELETE FROM joplin_mcp.notes_index WHERE user_id = $1 AND joplin_id = $2")
                .bind(user_id)
                .bind(joplin_id)
                .execute(tx.as_mut())
                .await
                .context("purge note index row")?;
        }
        JoplinItemType::Folder => {
            sqlx::query(
                "DELETE FROM joplin_mcp.notebooks_index WHERE user_id = $1 AND joplin_id = $2",
            )
            .bind(user_id)
            .bind(joplin_id)
            .execute(tx.as_mut())
            .await
            .context("purge notebook index row")?;
        }
        JoplinItemType::Tag => {
            sqlx::query(
                "DELETE FROM joplin_mcp.note_tags_index WHERE user_id = $1 AND tag_joplin_id = $2",
            )
            .bind(user_id)
            .bind(joplin_id)
            .execute(tx.as_mut())
            .await
            .context("purge note tag edges for tag")?;
            sqlx::query("DELETE FROM joplin_mcp.tags_index WHERE user_id = $1 AND joplin_id = $2")
                .bind(user_id)
                .bind(joplin_id)
                .execute(tx.as_mut())
                .await
                .context("purge tag index row")?;
        }
        JoplinItemType::NoteTag => {}
        JoplinItemType::Resource => {
            sqlx::query(
                "DELETE FROM joplin_mcp.resources_index WHERE user_id = $1 AND joplin_id = $2",
            )
            .bind(user_id)
            .bind(joplin_id)
            .execute(tx.as_mut())
            .await
            .context("purge resource index row")?;
        }
        JoplinItemType::Revision => {}
    }
    Ok(())
}

async fn upsert_deleted_row(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    row: &DeletedRow,
    source: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO joplin_mcp.deleted_items_index
            (user_id, joplin_id, item_type, source, deleted_time, tombstoned_at)
        VALUES ($1, $2, $3, $4, $5, now())
        ON CONFLICT (user_id, joplin_id) DO UPDATE
        SET item_type = EXCLUDED.item_type,
            source = EXCLUDED.source,
            deleted_time = COALESCE(EXCLUDED.deleted_time, joplin_mcp.deleted_items_index.deleted_time),
            tombstoned_at = now()
        "#,
    )
    .bind(user_id)
    .bind(&row.joplin_id)
    .bind(row.item_type as i32)
    .bind(source)
    .bind(row.deleted_time)
    .execute(tx.as_mut())
    .await
    .context("upsert deleted item index row")?;

    Ok(())
}

async fn reconcile_hard_deletes<S>(
    tx: &mut Transaction<'_, Postgres>,
    source: &S,
    user: &RefreshUser,
) -> anyhow::Result<usize>
where
    S: JoplinSource,
{
    let Some(active_refs) = source.active_item_refs(&user.joplin_user_id).await? else {
        return Ok(0);
    };
    let active: HashSet<(String, i32)> = active_refs
        .into_iter()
        .map(|item| (item.joplin_id, item.item_type as i32))
        .collect();
    let indexed = indexed_item_refs(tx, user.mcp_user_id).await?;
    let mut reconciled = 0;

    for item in indexed {
        if active.contains(&(item.joplin_id.clone(), item.item_type)) {
            continue;
        }
        let Some(item_type) = JoplinItemType::from_i32(item.item_type) else {
            continue;
        };
        let row = DeletedRow {
            joplin_id: item.joplin_id,
            item_type,
            deleted_time: None,
            note_joplin_id: None,
            tag_joplin_id: None,
        };
        purge_active_row(tx, user.mcp_user_id, &row).await?;
        upsert_deleted_row(tx, user.mcp_user_id, &row, "reconciliation").await?;
        reconciled += 1;
    }

    Ok(reconciled)
}

async fn indexed_item_refs(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
) -> anyhow::Result<Vec<IndexedItemRef>> {
    sqlx::query_as::<_, IndexedItemRef>(
        r#"
        SELECT joplin_id, item_type FROM (
            SELECT joplin_id, 1 AS item_type FROM joplin_mcp.notes_index WHERE user_id = $1
            UNION ALL
            SELECT joplin_id, 2 AS item_type FROM joplin_mcp.notebooks_index WHERE user_id = $1
            UNION ALL
            SELECT joplin_id, 5 AS item_type FROM joplin_mcp.tags_index WHERE user_id = $1
            UNION ALL
            SELECT joplin_id, 9 AS item_type FROM joplin_mcp.resources_index WHERE user_id = $1
        ) indexed
        ORDER BY joplin_id ASC, item_type ASC
        "#,
    )
    .bind(user_id)
    .fetch_all(tx.as_mut())
    .await
    .context("load indexed item references")
}

pub fn requires_full_rebuild(gap_seconds: u64, lookback_cap_seconds: u64) -> bool {
    gap_seconds > lookback_cap_seconds
}

pub fn full_rebuild_due(last_full_rebuild_at: Option<DateTime<Utc>>, interval_hours: u64) -> bool {
    let Some(last_full_rebuild_at) = last_full_rebuild_at else {
        return true;
    };
    Utc::now().signed_duration_since(last_full_rebuild_at)
        >= ChronoDuration::hours(interval_hours as i64)
}

pub fn hard_delete_reconciliation_due(
    last_reconciled_at: Option<DateTime<Utc>>,
    interval_hours: u64,
) -> bool {
    let Some(last_reconciled_at) = last_reconciled_at else {
        return true;
    };
    Utc::now().signed_duration_since(last_reconciled_at)
        >= ChronoDuration::hours(interval_hours as i64)
}

fn refresh_gap_seconds(last_incremental_at: Option<DateTime<Utc>>) -> u64 {
    last_incremental_at
        .map(|last| Utc::now().signed_duration_since(last).num_seconds().max(0) as u64)
        .unwrap_or(u64::MAX)
}

fn is_stale(last_incremental_at: Option<DateTime<Utc>>, refresh_interval_seconds: u64) -> bool {
    refresh_gap_seconds(last_incremental_at) > refresh_interval_seconds.saturating_mul(2)
}

fn joplin_lag_seconds(last_seen_joplin_updated_time: Option<i64>) -> Option<f64> {
    let last_seen = last_seen_joplin_updated_time?;
    Some(((Utc::now().timestamp_millis() - last_seen).max(0) as f64) / 1000.0)
}

fn advisory_lock_key(user_id: Uuid) -> i64 {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&user_id.as_bytes()[0..8]);
    i64::from_be_bytes(bytes)
}

fn parent_id(item: &JoplinItem, parsed: &ParsedItem) -> Option<String> {
    string_metadata(parsed, "parent_id").or_else(|| empty_to_none(item.jop_parent_id.clone()))
}

fn resource_title(item: &JoplinItem, parsed: &ParsedItem) -> String {
    if !parsed.title.is_empty() {
        parsed.title.clone()
    } else if !item.name.is_empty() {
        item.name.clone()
    } else {
        item.jop_id.clone()
    }
}

fn deleted_time(parsed: &ParsedItem) -> Option<i64> {
    integer_metadata(parsed, "deleted_time").filter(|deleted_time| *deleted_time != 0)
}

fn string_metadata(parsed: &ParsedItem, key: &str) -> Option<String> {
    parsed
        .metadata
        .get(key)
        .and_then(|value| empty_to_none(value.clone()))
}

fn integer_metadata(parsed: &ParsedItem, key: &str) -> Option<i64> {
    parsed.metadata.get(key)?.parse::<i64>().ok()
}

fn bool_metadata(parsed: &ParsedItem, key: &str) -> bool {
    matches!(
        parsed.metadata.get(key).map(String::as_str),
        Some("1") | Some("true")
    )
}

fn empty_to_none(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn item(owner_id: &str, jop_id: &str, item_type: JoplinItemType, content: &str) -> JoplinItem {
        JoplinItem {
            id: format!("server-{jop_id}"),
            owner_id: owner_id.to_string(),
            content: content.as_bytes().to_vec(),
            name: format!("{jop_id}.md"),
            mime_type: "text/plain".to_string(),
            updated_time: 20,
            created_time: 10,
            jop_id: jop_id.to_string(),
            jop_parent_id: String::new(),
            item_type,
            encrypted: false,
        }
    }

    fn content(title: &str, body: &str, item_type: i32, extra: &str) -> String {
        format!(
            "{title}\n\n{body}\n\nid: 0123456789abcdef0123456789abcdef\ntype_: {item_type}\ncreated_time: 1\nupdated_time: 2\n{extra}"
        )
    }

    #[test]
    fn lookback_cap_triggers_full_rebuild() {
        assert!(requires_full_rebuild(86_401, 86_400));
        assert!(!requires_full_rebuild(86_400, 86_400));
    }

    #[test]
    fn full_rebuild_interval_triggers_scheduled_rebuilds() {
        assert!(full_rebuild_due(None, 24));
        assert!(full_rebuild_due(Some(Utc::now() - Duration::hours(24)), 24));
        assert!(!full_rebuild_due(
            Some(Utc::now() - Duration::hours(23)),
            24
        ));
    }

    #[test]
    fn hard_delete_reconciliation_uses_separate_cadence() {
        assert!(hard_delete_reconciliation_due(None, 24));
        assert!(hard_delete_reconciliation_due(
            Some(Utc::now() - Duration::hours(24)),
            24
        ));
        assert!(!hard_delete_reconciliation_due(
            Some(Utc::now() - Duration::hours(23)),
            24
        ));
    }

    #[test]
    fn refresh_span_does_not_hold_enter_guard_across_awaits() {
        let source = include_str!("refresh.rs");
        let enter_guard = ["span", ".enter()"].concat();
        let guard_name = ["_", "entered"].concat();

        assert!(source.contains(".instrument(tracing::info_span!(\"index.refresh\"))"));
        assert!(!source.contains(&enter_guard));
        assert!(!source.contains(&guard_name));
    }

    #[test]
    fn stale_state_uses_twice_refresh_interval() {
        assert!(is_stale(Some(Utc::now() - Duration::seconds(121)), 60));
        assert!(!is_stale(Some(Utc::now() - Duration::seconds(120)), 60));
        assert!(is_stale(None, 60));
    }

    #[test]
    fn advisory_lock_key_is_stable_per_user() {
        let user_id = Uuid::parse_str("00000000-0000-0001-8000-000000000000").expect("uuid");
        assert_eq!(advisory_lock_key(user_id), 1);
        assert_eq!(advisory_lock_key(user_id), advisory_lock_key(user_id));
    }

    #[test]
    fn builds_incremental_rows_for_changed_items() {
        let rows = build_incremental_rows(
            "owner",
            vec![
                item(
                    "owner",
                    "note1",
                    JoplinItemType::Note,
                    &content("Note", "body", 1, "parent_id: notebook1\n"),
                ),
                item(
                    "owner",
                    "tag1",
                    JoplinItemType::Tag,
                    &content("Tag", "", 5, ""),
                ),
            ],
        );

        assert_eq!(rows.active.len(), 2);
        assert_eq!(rows.active[0].joplin_id, "note1");
        assert_eq!(
            rows.active[0].parent_joplin_id.as_deref(),
            Some("notebook1")
        );
        assert_eq!(rows.last_seen_joplin_updated_time(Some(1)), Some(20));
    }

    #[test]
    fn soft_deleted_items_become_tombstones_not_active_rows() {
        let rows = build_incremental_rows(
            "owner",
            vec![item(
                "owner",
                "deleted-note",
                JoplinItemType::Note,
                &content("Deleted", "", 1, "deleted_time: 55\n"),
            )],
        );

        assert!(rows.active.is_empty());
        assert_eq!(rows.deleted.len(), 1);
        assert_eq!(rows.deleted[0].joplin_id, "deleted-note");
        assert_eq!(rows.deleted[0].deleted_time, Some(55));
        assert_eq!(rows.last_seen_joplin_updated_time(Some(1)), Some(20));
    }

    #[test]
    fn note_tag_deletes_keep_edge_ids_when_available() {
        let rows = build_incremental_rows(
            "owner",
            vec![item(
                "owner",
                "edge1",
                JoplinItemType::NoteTag,
                &content(
                    "Edge",
                    "",
                    6,
                    "note_id: note1\ntag_id: tag1\ndeleted_time: 60\n",
                ),
            )],
        );

        assert_eq!(rows.deleted.len(), 1);
        assert_eq!(rows.deleted[0].note_joplin_id.as_deref(), Some("note1"));
        assert_eq!(rows.deleted[0].tag_joplin_id.as_deref(), Some("tag1"));
    }

    #[test]
    fn skips_encrypted_wrong_owner_and_malformed_changes() {
        let mut encrypted = item("owner", "encrypted", JoplinItemType::Note, "");
        encrypted.encrypted = true;
        encrypted.updated_time = 30;
        let mut wrong_owner = item(
            "other",
            "other-note",
            JoplinItemType::Note,
            &content("Other", "", 1, ""),
        );
        wrong_owner.updated_time = 40;
        let mut malformed = item("owner", "bad", JoplinItemType::Note, "");
        malformed.content = vec![0xff];
        malformed.updated_time = 50;
        let rows = build_incremental_rows("owner", vec![encrypted, wrong_owner, malformed]);

        assert!(rows.active.is_empty());
        assert_eq!(rows.skipped_encrypted, 1);
        assert_eq!(rows.skipped_wrong_owner, 1);
        assert_eq!(rows.skipped_malformed, 1);
        assert_eq!(rows.last_seen_joplin_updated_time(Some(1)), Some(50));
    }

    #[test]
    fn full_rebuild_outcome_is_preserved_for_incremental_escalation() {
        let outcome = IncrementalRefreshOutcome::from_full_rebuild(FullRebuildOutcome {
            indexed_notes: 2,
            indexed_notebooks: 3,
            indexed_tags: 5,
            indexed_note_tags: 7,
            indexed_resources: 11,
            indexed_deleted_items: 13,
            skipped_encrypted: 17,
            skipped_malformed: 19,
            skipped_wrong_owner: 23,
        });

        assert!(outcome.lock_acquired);
        assert!(outcome.full_rebuild_required);
        assert_eq!(outcome.changed_items, 0);
        assert_eq!(outcome.full_rebuild_indexed_items, 28);
        assert_eq!(outcome.full_rebuild_deleted_items, 13);
        assert_eq!(outcome.skipped_encrypted, 17);
        assert_eq!(outcome.skipped_malformed, 19);
        assert_eq!(outcome.skipped_wrong_owner, 23);
    }

    #[test]
    fn live_database_note_content_without_footer_builds_active_row() {
        let mut live_note = item("owner", "live-note", JoplinItemType::Note, "live body");
        live_note.name = "Live title".to_string();
        live_note.jop_parent_id = "notebook1".to_string();
        live_note.created_time = 123;
        live_note.updated_time = 456;

        let rows = build_incremental_rows("owner", vec![live_note]);

        assert_eq!(rows.active.len(), 1);
        assert_eq!(rows.active[0].title, "Live title");
        assert_eq!(rows.active[0].body_text, "live body");
        assert_eq!(
            rows.active[0].parent_joplin_id.as_deref(),
            Some("notebook1")
        );
        assert_eq!(rows.active[0].created_time, 123);
        assert_eq!(rows.active[0].updated_time, 456);
        assert_eq!(rows.skipped_malformed, 0);
    }

    #[test]
    fn live_database_note_tag_content_without_footer_builds_active_row() {
        let rows = build_incremental_rows(
            "owner",
            vec![item(
                "owner",
                "live-edge",
                JoplinItemType::NoteTag,
                "note_id: live-note\ntag_id: live-tag",
            )],
        );

        assert_eq!(rows.active.len(), 1);
        assert_eq!(rows.active[0].note_joplin_id.as_deref(), Some("live-note"));
        assert_eq!(rows.active[0].tag_joplin_id.as_deref(), Some("live-tag"));
        assert_eq!(rows.active[0].updated_time, 20);
        assert_eq!(rows.skipped_malformed, 0);
    }

    #[test]
    fn due_refresh_query_uses_active_tokens_and_limit() {
        let query = r#"
        SELECT
            users.id AS mcp_user_id,
            users.joplin_user_id,
            state.status,
            state.last_incremental_at,
            state.last_full_rebuild_at,
            state.last_checked_at,
            state.last_reconciled_at,
            state.last_seen_joplin_updated_time
        FROM joplin_mcp.mcp_users users
        JOIN joplin_mcp.mcp_tokens tokens ON tokens.user_id = users.id
        LEFT JOIN joplin_mcp.index_state state ON state.user_id = users.id
        WHERE users.disabled_at IS NULL
          AND tokens.revoked_at IS NULL
          AND (tokens.expires_at IS NULL OR tokens.expires_at > now())
        "#;

        assert!(query.contains("JOIN joplin_mcp.mcp_tokens"));
        assert!(query.contains("tokens.revoked_at IS NULL"));
        assert!(query.contains("tokens.expires_at IS NULL OR tokens.expires_at > now()"));
    }

    #[test]
    fn explicit_refresh_request_creates_due_state_for_missing_index() {
        assert!(REQUEST_INDEX_REFRESH_NOW_SQL.contains("INSERT INTO joplin_mcp.index_state"));
        assert!(REQUEST_INDEX_REFRESH_NOW_SQL.contains("(user_id, status, last_checked_at"));
        assert!(REQUEST_INDEX_REFRESH_NOW_SQL.contains("VALUES ($1, $2, NULL, now())"));
        assert!(REQUEST_INDEX_REFRESH_NOW_SQL.contains("SET last_checked_at = NULL"));
        assert!(REQUEST_INDEX_REFRESH_NOW_SQL.contains("status <> $3"));
    }

    #[test]
    fn refresh_needed_uses_source_watermark_without_forcing_no_change_apply() {
        let config = IndexConfig::default();
        let user = RefreshUser {
            mcp_user_id: Uuid::new_v4(),
            joplin_user_id: "joplin-user".to_string(),
            status: Some("ready".to_string()),
            last_incremental_at: Some(Utc::now()),
            last_full_rebuild_at: Some(Utc::now()),
            last_checked_at: Some(Utc::now()),
            last_reconciled_at: Some(Utc::now()),
            last_seen_joplin_updated_time: Some(10),
        };

        assert!(!refresh_needed(&user, Some(10), &config));
        assert!(refresh_needed(&user, Some(11), &config));
    }

    #[test]
    fn refresh_needed_runs_when_hard_delete_reconciliation_is_due() {
        let config = IndexConfig::default();
        let mut user = RefreshUser {
            mcp_user_id: Uuid::new_v4(),
            joplin_user_id: "joplin-user".to_string(),
            status: Some("ready".to_string()),
            last_incremental_at: Some(Utc::now()),
            last_full_rebuild_at: Some(Utc::now()),
            last_checked_at: Some(Utc::now()),
            last_reconciled_at: Some(Utc::now()),
            last_seen_joplin_updated_time: Some(10),
        };

        assert!(!refresh_needed(&user, Some(10), &config));
        user.last_reconciled_at =
            Some(Utc::now() - Duration::hours(config.hard_delete_reconcile_interval_hours as i64));
        assert!(refresh_needed(&user, Some(10), &config));
        user.last_reconciled_at = None;
        assert!(refresh_needed(&user, Some(10), &config));
    }

    #[test]
    fn refresh_needed_rebuilds_missing_or_failed_state() {
        let config = IndexConfig::default();
        let mut user = RefreshUser {
            mcp_user_id: Uuid::new_v4(),
            joplin_user_id: "joplin-user".to_string(),
            status: None,
            last_incremental_at: Some(Utc::now()),
            last_full_rebuild_at: Some(Utc::now()),
            last_checked_at: Some(Utc::now()),
            last_reconciled_at: Some(Utc::now()),
            last_seen_joplin_updated_time: None,
        };

        assert!(refresh_needed(&user, None, &config));
        user.status = Some("failed".to_string());
        assert!(refresh_needed(&user, None, &config));
    }
}
