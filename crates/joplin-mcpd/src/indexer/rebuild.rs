use crate::indexer::JoplinItemType;
use crate::indexer::item_content::parse_index_item;
use crate::indexer::parser::{ParsedItem, extract_resource_refs};
use crate::indexer::source::{JOPLIN_ITEM_BATCH_SIZE, JoplinItem, JoplinItemCursor, JoplinSource};
use anyhow::Context;
use sqlx::{PgPool, Postgres, QueryBuilder};
use std::collections::HashSet;
use uuid::Uuid;

const INSERT_BATCH_ROWS: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexStatus {
    Empty,
    Building,
    Ready,
    Failed,
    Stale,
}

impl IndexStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Building => "building",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Stale => "stale",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullRebuildOutcome {
    pub indexed_notes: usize,
    pub indexed_notebooks: usize,
    pub indexed_tags: usize,
    pub indexed_note_tags: usize,
    pub indexed_resources: usize,
    pub indexed_deleted_items: usize,
    pub skipped_encrypted: usize,
    pub skipped_malformed: usize,
    pub skipped_wrong_owner: usize,
}

#[derive(Debug, Default)]
struct RebuildRows {
    notes: Vec<NoteRow>,
    notebooks: Vec<NotebookRow>,
    tags: Vec<TagRow>,
    note_tags: Vec<NoteTagRow>,
    resources: Vec<ResourceRow>,
    deleted_items: Vec<DeletedItemRow>,
    skipped_encrypted: usize,
    skipped_malformed: usize,
    skipped_wrong_owner: usize,
}

impl RebuildRows {
    fn outcome(&self) -> FullRebuildOutcome {
        FullRebuildOutcome {
            indexed_notes: self.notes.len(),
            indexed_notebooks: self.notebooks.len(),
            indexed_tags: self.tags.len(),
            indexed_note_tags: self.note_tags.len(),
            indexed_resources: self.resources.len(),
            indexed_deleted_items: self.deleted_items.len(),
            skipped_encrypted: self.skipped_encrypted,
            skipped_malformed: self.skipped_malformed,
            skipped_wrong_owner: self.skipped_wrong_owner,
        }
    }

    fn last_seen_joplin_updated_time(&self) -> Option<i64> {
        self.notes
            .iter()
            .map(|row| row.updated_time)
            .chain(self.notebooks.iter().map(|row| row.updated_time))
            .chain(self.tags.iter().map(|row| row.updated_time))
            .chain(self.note_tags.iter().map(|row| row.updated_time))
            .chain(self.resources.iter().map(|row| row.updated_time))
            .chain(self.deleted_items.iter().filter_map(|row| row.deleted_time))
            .max()
    }
}

#[derive(Debug)]
struct NoteRow {
    joplin_item_id: String,
    joplin_id: String,
    parent_joplin_id: Option<String>,
    title: String,
    body_text: String,
    is_todo: bool,
    created_time: i64,
    updated_time: i64,
    resource_refs: Vec<String>,
}

#[derive(Debug)]
struct NotebookRow {
    joplin_item_id: String,
    joplin_id: String,
    parent_joplin_id: Option<String>,
    title: String,
    created_time: i64,
    updated_time: i64,
}

#[derive(Debug)]
struct TagRow {
    joplin_item_id: String,
    joplin_id: String,
    title: String,
    updated_time: i64,
}

#[derive(Debug)]
struct NoteTagRow {
    note_joplin_id: String,
    tag_joplin_id: String,
    updated_time: i64,
}

#[derive(Debug)]
struct ResourceRow {
    joplin_item_id: String,
    joplin_id: String,
    title: String,
    mime: Option<String>,
    size_bytes: Option<i64>,
    file_extension: Option<String>,
    updated_time: i64,
}

#[derive(Debug)]
struct DeletedItemRow {
    joplin_id: String,
    item_type: i32,
    deleted_time: Option<i64>,
}

pub async fn full_rebuild_user<S>(
    mcp_pool: &PgPool,
    source: &S,
    mcp_user_id: Uuid,
    joplin_user_id: &str,
) -> anyhow::Result<FullRebuildOutcome>
where
    S: JoplinSource,
{
    let previous_servable = has_servable_index(mcp_pool, mcp_user_id)
        .await
        .context("check previous index state")?;

    let result: anyhow::Result<FullRebuildOutcome> = async {
        let mut rows = RebuildRows::default();
        let mut after = None;

        loop {
            let items = source
                .changed_items_batch(joplin_user_id, None, after.as_ref(), JOPLIN_ITEM_BATCH_SIZE)
                .await
                .context("load Joplin item batch for full rebuild")?;
            if items.is_empty() {
                break;
            }

            let batch_len = items.len();
            after = items.last().map(JoplinItemCursor::from);
            append_rebuild_rows(&mut rows, joplin_user_id, items);
            if batch_len < JOPLIN_ITEM_BATCH_SIZE as usize {
                break;
            }
        }

        prune_dangling_note_tags(&mut rows);
        replace_derived_rows(mcp_pool, mcp_user_id, &rows).await?;
        Ok(rows.outcome())
    }
    .await;

    match result {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            record_rebuild_failure(mcp_pool, mcp_user_id, previous_servable, &error.to_string())
                .await
                .context("record rebuild failure")?;
            Err(error)
        }
    }
}

#[cfg(test)]
fn build_rebuild_rows(joplin_user_id: &str, items: Vec<JoplinItem>) -> RebuildRows {
    let mut rows = RebuildRows::default();
    append_rebuild_rows(&mut rows, joplin_user_id, items);
    prune_dangling_note_tags(&mut rows);
    rows
}

fn append_rebuild_rows(rows: &mut RebuildRows, joplin_user_id: &str, items: Vec<JoplinItem>) {
    for item in items {
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
            rows.deleted_items.push(DeletedItemRow {
                joplin_id: item.jop_id,
                item_type: item.item_type as i32,
                deleted_time: Some(deleted_time),
            });
            continue;
        }

        match item.item_type {
            JoplinItemType::Note => rows.notes.push(NoteRow {
                joplin_item_id: item.id.clone(),
                joplin_id: item.jop_id.clone(),
                parent_joplin_id: parent_id(&item, &parsed),
                title: parsed.title.clone(),
                body_text: parsed.body.clone(),
                is_todo: bool_metadata(&parsed, "is_todo"),
                created_time: integer_metadata(&parsed, "created_time")
                    .unwrap_or(item.created_time),
                updated_time: integer_metadata(&parsed, "updated_time")
                    .unwrap_or(item.updated_time),
                resource_refs: extract_resource_refs(&parsed.body),
            }),
            JoplinItemType::Folder => rows.notebooks.push(NotebookRow {
                joplin_item_id: item.id.clone(),
                joplin_id: item.jop_id.clone(),
                parent_joplin_id: parent_id(&item, &parsed),
                title: parsed.title.clone(),
                created_time: integer_metadata(&parsed, "created_time")
                    .unwrap_or(item.created_time),
                updated_time: integer_metadata(&parsed, "updated_time")
                    .unwrap_or(item.updated_time),
            }),
            JoplinItemType::Tag => rows.tags.push(TagRow {
                joplin_item_id: item.id.clone(),
                joplin_id: item.jop_id.clone(),
                title: parsed.title.clone(),
                updated_time: integer_metadata(&parsed, "updated_time")
                    .unwrap_or(item.updated_time),
            }),
            JoplinItemType::NoteTag => match note_tag_row(&parsed, item.updated_time) {
                Some(row) => rows.note_tags.push(row),
                None => rows.skipped_malformed += 1,
            },
            JoplinItemType::Resource => rows.resources.push(ResourceRow {
                joplin_item_id: item.id.clone(),
                joplin_id: item.jop_id.clone(),
                title: resource_title(&item, &parsed),
                mime: empty_to_none(item.mime_type),
                size_bytes: integer_metadata(&parsed, "size"),
                file_extension: string_metadata(&parsed, "file_extension"),
                updated_time: integer_metadata(&parsed, "updated_time")
                    .unwrap_or(item.updated_time),
            }),
            JoplinItemType::Revision => {
                rows.skipped_malformed += 1;
            }
        }
    }
}

async fn replace_derived_rows(
    mcp_pool: &PgPool,
    user_id: Uuid,
    rows: &RebuildRows,
) -> anyhow::Result<()> {
    let mut tx = mcp_pool
        .begin()
        .await
        .context("begin full rebuild transaction")?;

    sqlx::query(
        r#"
        INSERT INTO joplin_mcp.index_state (user_id, status, updated_at)
        VALUES ($1, $2, now())
        ON CONFLICT (user_id) DO UPDATE
        SET status = EXCLUDED.status,
            updated_at = now()
        "#,
    )
    .bind(user_id)
    .bind(IndexStatus::Building.as_str())
    .execute(tx.as_mut())
    .await
    .context("mark index building")?;

    for sql in DELETE_DERIVED_ROWS_SQL {
        sqlx::query(sql)
            .bind(user_id)
            .execute(tx.as_mut())
            .await
            .context("delete previous derived index rows")?;
    }

    insert_notebooks(&mut tx, user_id, &rows.notebooks).await?;
    insert_notes(&mut tx, user_id, &rows.notes).await?;
    insert_tags(&mut tx, user_id, &rows.tags).await?;
    insert_note_tags(&mut tx, user_id, &rows.note_tags).await?;
    insert_resources(&mut tx, user_id, &rows.resources).await?;
    insert_deleted_items(&mut tx, user_id, &rows.deleted_items).await?;

    sqlx::query(
        r#"
        UPDATE joplin_mcp.index_state
        SET status = $2,
            last_full_rebuild_at = now(),
            last_incremental_at = now(),
            last_checked_at = now(),
            last_seen_joplin_updated_time = $3,
            last_error = NULL,
            updated_at = now()
        WHERE user_id = $1
        "#,
    )
    .bind(user_id)
    .bind(IndexStatus::Ready.as_str())
    .bind(rows.last_seen_joplin_updated_time())
    .execute(tx.as_mut())
    .await
    .context("mark index ready")?;

    tx.commit()
        .await
        .context("commit full rebuild transaction")?;
    Ok(())
}

const DELETE_DERIVED_ROWS_SQL: &[&str] = &[
    "DELETE FROM joplin_mcp.notes_index WHERE user_id = $1",
    "DELETE FROM joplin_mcp.notebooks_index WHERE user_id = $1",
    "DELETE FROM joplin_mcp.tags_index WHERE user_id = $1",
    "DELETE FROM joplin_mcp.note_tags_index WHERE user_id = $1",
    "DELETE FROM joplin_mcp.resources_index WHERE user_id = $1",
    "DELETE FROM joplin_mcp.deleted_items_index WHERE user_id = $1",
];

async fn insert_notebooks(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: Uuid,
    rows: &[NotebookRow],
) -> anyhow::Result<()> {
    for chunk in rows.chunks(INSERT_BATCH_ROWS) {
        let mut builder = QueryBuilder::<Postgres>::new(
            r#"
            INSERT INTO joplin_mcp.notebooks_index
                (user_id, joplin_item_id, joplin_id, parent_joplin_id, title, created_time, updated_time)
            "#,
        );
        builder.push_values(chunk, |mut row_builder, row| {
            row_builder
                .push_bind(user_id)
                .push_bind(&row.joplin_item_id)
                .push_bind(&row.joplin_id)
                .push_bind(&row.parent_joplin_id)
                .push_bind(&row.title)
                .push_bind(row.created_time)
                .push_bind(row.updated_time);
        });
        builder
            .build()
            .execute(tx.as_mut())
            .await
            .context("insert notebook index rows")?;
    }
    Ok(())
}

async fn insert_notes(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: Uuid,
    rows: &[NoteRow],
) -> anyhow::Result<()> {
    for chunk in rows.chunks(INSERT_BATCH_ROWS) {
        let mut builder = QueryBuilder::<Postgres>::new(
            r#"
            INSERT INTO joplin_mcp.notes_index
                (user_id, joplin_item_id, joplin_id, parent_joplin_id, title, body_text, is_todo, created_time, updated_time, resource_refs)
            "#,
        );
        builder.push_values(chunk, |mut row_builder, row| {
            row_builder
                .push_bind(user_id)
                .push_bind(&row.joplin_item_id)
                .push_bind(&row.joplin_id)
                .push_bind(&row.parent_joplin_id)
                .push_bind(&row.title)
                .push_bind(&row.body_text)
                .push_bind(row.is_todo)
                .push_bind(row.created_time)
                .push_bind(row.updated_time)
                .push_bind(&row.resource_refs);
        });
        builder
            .build()
            .execute(tx.as_mut())
            .await
            .context("insert note index rows")?;
    }
    Ok(())
}

async fn insert_tags(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: Uuid,
    rows: &[TagRow],
) -> anyhow::Result<()> {
    for chunk in rows.chunks(INSERT_BATCH_ROWS) {
        let mut builder = QueryBuilder::<Postgres>::new(
            r#"
            INSERT INTO joplin_mcp.tags_index
                (user_id, joplin_item_id, joplin_id, title, updated_time)
            "#,
        );
        builder.push_values(chunk, |mut row_builder, row| {
            row_builder
                .push_bind(user_id)
                .push_bind(&row.joplin_item_id)
                .push_bind(&row.joplin_id)
                .push_bind(&row.title)
                .push_bind(row.updated_time);
        });
        builder
            .build()
            .execute(tx.as_mut())
            .await
            .context("insert tag index rows")?;
    }
    Ok(())
}

async fn insert_note_tags(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: Uuid,
    rows: &[NoteTagRow],
) -> anyhow::Result<()> {
    for chunk in rows.chunks(INSERT_BATCH_ROWS) {
        let mut builder = QueryBuilder::<Postgres>::new(
            r#"
            INSERT INTO joplin_mcp.note_tags_index
                (user_id, note_joplin_id, tag_joplin_id, updated_time)
            "#,
        );
        builder.push_values(chunk, |mut row_builder, row| {
            row_builder
                .push_bind(user_id)
                .push_bind(&row.note_joplin_id)
                .push_bind(&row.tag_joplin_id)
                .push_bind(row.updated_time);
        });
        builder
            .build()
            .execute(tx.as_mut())
            .await
            .context("insert note tag index rows")?;
    }
    Ok(())
}

async fn insert_resources(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: Uuid,
    rows: &[ResourceRow],
) -> anyhow::Result<()> {
    for chunk in rows.chunks(INSERT_BATCH_ROWS) {
        let mut builder = QueryBuilder::<Postgres>::new(
            r#"
            INSERT INTO joplin_mcp.resources_index
                (user_id, joplin_item_id, joplin_id, title, mime, size_bytes, file_extension, updated_time)
            "#,
        );
        builder.push_values(chunk, |mut row_builder, row| {
            row_builder
                .push_bind(user_id)
                .push_bind(&row.joplin_item_id)
                .push_bind(&row.joplin_id)
                .push_bind(&row.title)
                .push_bind(&row.mime)
                .push_bind(row.size_bytes)
                .push_bind(&row.file_extension)
                .push_bind(row.updated_time);
        });
        builder
            .build()
            .execute(tx.as_mut())
            .await
            .context("insert resource index rows")?;
    }
    Ok(())
}

async fn insert_deleted_items(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: Uuid,
    rows: &[DeletedItemRow],
) -> anyhow::Result<()> {
    for chunk in rows.chunks(INSERT_BATCH_ROWS) {
        let mut builder = QueryBuilder::<Postgres>::new(
            r#"
            INSERT INTO joplin_mcp.deleted_items_index
                (user_id, joplin_id, item_type, source, deleted_time)
            "#,
        );
        builder.push_values(chunk, |mut row_builder, row| {
            row_builder
                .push_bind(user_id)
                .push_bind(&row.joplin_id)
                .push_bind(row.item_type)
                .push_bind("full_rebuild")
                .push_bind(row.deleted_time);
        });
        builder
            .build()
            .execute(tx.as_mut())
            .await
            .context("insert deleted item index rows")?;
    }
    Ok(())
}

async fn has_servable_index(mcp_pool: &PgPool, user_id: Uuid) -> anyhow::Result<bool> {
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM joplin_mcp.index_state WHERE user_id = $1")
            .bind(user_id)
            .fetch_optional(mcp_pool)
            .await?;

    Ok(is_servable_status(status.as_deref()))
}

async fn record_rebuild_failure(
    mcp_pool: &PgPool,
    user_id: Uuid,
    previous_servable: bool,
    error: &str,
) -> anyhow::Result<()> {
    if previous_servable {
        sqlx::query(
            r#"
            UPDATE joplin_mcp.index_state
            SET last_error = $2,
                updated_at = now()
            WHERE user_id = $1
            "#,
        )
        .bind(user_id)
        .bind(error)
        .execute(mcp_pool)
        .await?;
    } else {
        sqlx::query(
            r#"
            INSERT INTO joplin_mcp.index_state (user_id, status, last_error, updated_at)
            VALUES ($1, $2, $3, now())
            ON CONFLICT (user_id) DO UPDATE
            SET status = EXCLUDED.status,
                last_error = EXCLUDED.last_error,
                updated_at = now()
            "#,
        )
        .bind(user_id)
        .bind(IndexStatus::Failed.as_str())
        .bind(error)
        .execute(mcp_pool)
        .await?;
    }

    Ok(())
}

fn deleted_time(parsed: &ParsedItem) -> Option<i64> {
    integer_metadata(parsed, "deleted_time").filter(|deleted_time| *deleted_time != 0)
}

fn is_servable_status(status: Option<&str>) -> bool {
    matches!(status, Some("ready") | Some("stale"))
}

fn parent_id(item: &JoplinItem, parsed: &ParsedItem) -> Option<String> {
    string_metadata(parsed, "parent_id").or_else(|| empty_to_none(item.jop_parent_id.clone()))
}

fn note_tag_row(parsed: &ParsedItem, fallback_updated_time: i64) -> Option<NoteTagRow> {
    Some(NoteTagRow {
        note_joplin_id: string_metadata(parsed, "note_id")?,
        tag_joplin_id: string_metadata(parsed, "tag_id")?,
        updated_time: integer_metadata(parsed, "updated_time").unwrap_or(fallback_updated_time),
    })
}

fn prune_dangling_note_tags(rows: &mut RebuildRows) {
    let note_ids: HashSet<&str> = rows
        .notes
        .iter()
        .map(|row| row.joplin_id.as_str())
        .collect();
    let tag_ids: HashSet<&str> = rows.tags.iter().map(|row| row.joplin_id.as_str()).collect();
    rows.note_tags.retain(|row| {
        note_ids.contains(row.note_joplin_id.as_str())
            && tag_ids.contains(row.tag_joplin_id.as_str())
    });
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
    fn builds_rows_for_all_supported_item_types() {
        let rows = build_rebuild_rows(
            "owner",
            vec![
                item(
                    "owner",
                    "note1",
                    JoplinItemType::Note,
                    &content(
                        "Note",
                        "body ![r](:/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)",
                        1,
                        "parent_id: notebook1\nis_todo: 1\n",
                    ),
                ),
                item(
                    "owner",
                    "notebook1",
                    JoplinItemType::Folder,
                    &content("Notebook", "", 2, "parent_id: \n"),
                ),
                item(
                    "owner",
                    "tag1",
                    JoplinItemType::Tag,
                    &content("Tag", "", 5, ""),
                ),
                item(
                    "owner",
                    "edge1",
                    JoplinItemType::NoteTag,
                    &content("Edge", "", 6, "note_id: note1\ntag_id: tag1\n"),
                ),
                item(
                    "owner",
                    "resource1",
                    JoplinItemType::Resource,
                    &content("Resource", "", 9, "size: 42\nfile_extension: png\n"),
                ),
            ],
        );

        assert_eq!(rows.notes.len(), 1);
        assert_eq!(rows.notes[0].parent_joplin_id.as_deref(), Some("notebook1"));
        assert!(rows.notes[0].is_todo);
        assert_eq!(
            rows.notes[0].resource_refs,
            ["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()]
        );
        assert_eq!(rows.notebooks.len(), 1);
        assert_eq!(rows.tags.len(), 1);
        assert_eq!(rows.note_tags.len(), 1);
        assert_eq!(rows.note_tags[0].note_joplin_id, "note1");
        assert_eq!(rows.note_tags[0].tag_joplin_id, "tag1");
        assert_eq!(rows.resources.len(), 1);
        assert_eq!(rows.resources[0].size_bytes, Some(42));
        assert_eq!(rows.resources[0].file_extension.as_deref(), Some("png"));
    }

    #[test]
    fn skips_encrypted_wrong_owner_and_malformed_items() {
        let mut encrypted = item("owner", "encrypted", JoplinItemType::Note, "");
        encrypted.encrypted = true;
        let wrong_owner = item(
            "other",
            "other-note",
            JoplinItemType::Note,
            &content("Other", "", 1, ""),
        );
        let mut malformed = item("owner", "bad", JoplinItemType::Note, "");
        malformed.content = vec![0xff];

        let rows = build_rebuild_rows("owner", vec![encrypted, wrong_owner, malformed]);

        assert_eq!(rows.notes.len(), 0);
        assert_eq!(rows.skipped_encrypted, 1);
        assert_eq!(rows.skipped_wrong_owner, 1);
        assert_eq!(rows.skipped_malformed, 1);
    }

    #[test]
    fn deleted_items_are_not_added_to_active_tables() {
        let rows = build_rebuild_rows(
            "owner",
            vec![item(
                "owner",
                "deleted-note",
                JoplinItemType::Note,
                &content("Deleted", "", 1, "deleted_time: 55\n"),
            )],
        );

        assert!(rows.notes.is_empty());
        assert_eq!(rows.deleted_items.len(), 1);
        assert_eq!(rows.deleted_items[0].joplin_id, "deleted-note");
        assert_eq!(rows.deleted_items[0].deleted_time, Some(55));
    }

    #[test]
    fn deletion_order_matches_full_rebuild_contract() {
        assert_eq!(
            DELETE_DERIVED_ROWS_SQL,
            [
                "DELETE FROM joplin_mcp.notes_index WHERE user_id = $1",
                "DELETE FROM joplin_mcp.notebooks_index WHERE user_id = $1",
                "DELETE FROM joplin_mcp.tags_index WHERE user_id = $1",
                "DELETE FROM joplin_mcp.note_tags_index WHERE user_id = $1",
                "DELETE FROM joplin_mcp.resources_index WHERE user_id = $1",
                "DELETE FROM joplin_mcp.deleted_items_index WHERE user_id = $1",
            ]
        );
    }

    #[test]
    fn previous_servable_statuses_are_preserved_on_failure() {
        assert!(is_servable_status(Some("ready")));
        assert!(is_servable_status(Some("stale")));
        assert!(!is_servable_status(Some("building")));
    }

    #[test]
    fn failed_status_is_available_for_first_rebuild_failure() {
        assert_eq!(IndexStatus::Failed.as_str(), "failed");
    }

    #[test]
    fn insert_batch_size_stays_bounded_for_live_note_bodies() {
        assert_eq!(INSERT_BATCH_ROWS, 100);
    }

    #[test]
    fn source_fetch_batch_size_stays_bounded_for_live_note_bodies() {
        assert_eq!(JOPLIN_ITEM_BATCH_SIZE, 500);
        let source = include_str!("rebuild.rs");
        assert!(source.contains("changed_items_batch"));
    }

    #[test]
    fn malformed_note_tag_edges_are_skipped() {
        let rows = build_rebuild_rows(
            "owner",
            vec![item(
                "owner",
                "edge",
                JoplinItemType::NoteTag,
                &content("Edge", "", 6, "note_id: note1\n"),
            )],
        );

        assert!(rows.note_tags.is_empty());
        assert_eq!(rows.skipped_malformed, 1);
    }

    #[test]
    fn live_database_note_content_without_footer_is_indexed() {
        let mut live_note = item("owner", "live-note", JoplinItemType::Note, "live body");
        live_note.name = "Live title".to_string();
        live_note.jop_parent_id = "notebook1".to_string();
        live_note.created_time = 123;
        live_note.updated_time = 456;

        let rows = build_rebuild_rows("owner", vec![live_note]);

        assert_eq!(rows.notes.len(), 1);
        assert_eq!(rows.notes[0].title, "Live title");
        assert_eq!(rows.notes[0].body_text, "live body");
        assert_eq!(rows.notes[0].parent_joplin_id.as_deref(), Some("notebook1"));
        assert_eq!(rows.notes[0].created_time, 123);
        assert_eq!(rows.notes[0].updated_time, 456);
        assert_eq!(rows.skipped_malformed, 0);
    }

    #[test]
    fn live_database_note_tag_content_without_footer_is_indexed() {
        let rows = build_rebuild_rows(
            "owner",
            vec![
                item("owner", "live-note", JoplinItemType::Note, "live body"),
                item(
                    "owner",
                    "live-tag",
                    JoplinItemType::Tag,
                    r#"{"title":"Live tag"}"#,
                ),
                item(
                    "owner",
                    "live-edge",
                    JoplinItemType::NoteTag,
                    "note_id: live-note\ntag_id: live-tag",
                ),
            ],
        );

        assert_eq!(rows.note_tags.len(), 1);
        assert_eq!(rows.note_tags[0].note_joplin_id, "live-note");
        assert_eq!(rows.note_tags[0].tag_joplin_id, "live-tag");
        assert_eq!(rows.note_tags[0].updated_time, 20);
        assert_eq!(rows.skipped_malformed, 0);
    }

    #[test]
    fn dangling_live_note_tag_edges_are_pruned() {
        let rows = build_rebuild_rows(
            "owner",
            vec![
                item("owner", "live-note", JoplinItemType::Note, "live body"),
                item(
                    "owner",
                    "live-tag",
                    JoplinItemType::Tag,
                    r#"{"title":"Live tag"}"#,
                ),
                item(
                    "owner",
                    "valid-edge",
                    JoplinItemType::NoteTag,
                    r#"{"note_id":"live-note","tag_id":"live-tag"}"#,
                ),
                item(
                    "owner",
                    "dangling-edge",
                    JoplinItemType::NoteTag,
                    r#"{"note_id":"live-note","tag_id":"missing-tag"}"#,
                ),
            ],
        );

        assert_eq!(rows.note_tags.len(), 1);
        assert_eq!(rows.note_tags[0].tag_joplin_id, "live-tag");
    }

    #[test]
    fn outcome_reports_indexed_and_skipped_counts() {
        let mut encrypted = item("owner", "encrypted", JoplinItemType::Note, "");
        encrypted.encrypted = true;
        let rows = build_rebuild_rows(
            "owner",
            vec![
                item(
                    "owner",
                    "tag1",
                    JoplinItemType::Tag,
                    &content("Tag", "", 5, ""),
                ),
                encrypted,
            ],
        );

        let outcome = rows.outcome();

        assert_eq!(outcome.indexed_tags, 1);
        assert_eq!(outcome.skipped_encrypted, 1);
    }
}
