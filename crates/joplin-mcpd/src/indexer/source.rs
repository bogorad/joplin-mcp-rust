use crate::{db::pool as db_pool, indexer::JoplinItemType};
use anyhow::Context;
use async_trait::async_trait;
use sqlx::{FromRow, PgPool};

#[cfg(test)]
const SELECT_ITEM_COLUMNS: &str = r#"
    id,
    owner_id,
    content,
    name,
    mime_type,
    updated_time,
    created_time,
    jop_id,
    jop_parent_id,
    jop_type,
    jop_encryption_applied
"#;

pub const JOPLIN_ITEM_BATCH_SIZE: u32 = 500;

#[cfg(test)]
const CHANGED_ITEMS_QUERY: &str = r#"
    SELECT
        id,
        owner_id,
        content,
        name,
        mime_type,
        updated_time,
        created_time,
        jop_id,
        jop_parent_id,
        jop_type,
        jop_encryption_applied
    FROM items
    WHERE owner_id = $1
      AND ($2::bigint IS NULL OR updated_time > $2)
      AND jop_type IN (1, 2, 5, 6, 9)
    ORDER BY updated_time ASC, id ASC
"#;

const CHANGED_ITEMS_BATCH_QUERY: &str = r#"
    SELECT
        id,
        owner_id,
        content,
        name,
        mime_type,
        updated_time,
        created_time,
        jop_id,
        jop_parent_id,
        jop_type,
        jop_encryption_applied
    FROM items
    WHERE owner_id = $1
      AND ($2::bigint IS NULL OR updated_time > $2)
      AND ($3::bigint IS NULL OR updated_time > $3 OR (updated_time = $3 AND id > $4))
      AND jop_type IN (1, 2, 5, 6, 9)
    ORDER BY updated_time ASC, id ASC
    LIMIT $5
"#;

const ITEM_BY_ID_QUERY: &str = r#"
    SELECT
        id,
        owner_id,
        content,
        name,
        mime_type,
        updated_time,
        created_time,
        jop_id,
        jop_parent_id,
        jop_type,
        jop_encryption_applied
    FROM items
    WHERE owner_id = $1
      AND id = $2
	      AND jop_type IN (1, 2, 5, 6, 9)
	"#;

const ACTIVE_ITEM_REFS_QUERY: &str = r#"
    SELECT jop_id, jop_type
    FROM items
    WHERE owner_id = $1
      AND jop_type IN (1, 2, 5, 9)
    ORDER BY jop_id ASC
"#;

const SOURCE_WATERMARKS_QUERY: &str = r#"
    SELECT owner_id, max(updated_time) AS source_watermark
    FROM items
    WHERE owner_id = ANY($1)
      AND jop_type IN (1, 2, 5, 6, 9)
    GROUP BY owner_id
    ORDER BY owner_id ASC
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoplinItem {
    pub id: String,
    pub owner_id: String,
    pub content: Vec<u8>,
    pub name: String,
    pub mime_type: String,
    pub updated_time: i64,
    pub created_time: i64,
    pub jop_id: String,
    pub jop_parent_id: String,
    pub item_type: JoplinItemType,
    pub encrypted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoplinItemCursor {
    pub updated_time: i64,
    pub id: String,
}

impl From<&JoplinItem> for JoplinItemCursor {
    fn from(item: &JoplinItem) -> Self {
        Self {
            updated_time: item.updated_time,
            id: item.id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoplinItemRef {
    pub joplin_id: String,
    pub item_type: JoplinItemType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoplinSourceWatermark {
    pub owner_id: String,
    pub source_watermark: i64,
}

#[async_trait]
pub trait JoplinSource: Send + Sync {
    async fn changed_items_since(
        &self,
        user_id: &str,
        since: Option<i64>,
    ) -> anyhow::Result<Vec<JoplinItem>>;

    async fn changed_items_batch(
        &self,
        user_id: &str,
        since: Option<i64>,
        after: Option<&JoplinItemCursor>,
        limit: u32,
    ) -> anyhow::Result<Vec<JoplinItem>> {
        let mut items = self.changed_items_since(user_id, since).await?;
        items.sort_by(|left, right| {
            left.updated_time
                .cmp(&right.updated_time)
                .then_with(|| left.id.cmp(&right.id))
        });
        if let Some(after) = after {
            items.retain(|item| item_is_after_cursor(item, after));
        }
        items.truncate(limit.max(1) as usize);
        Ok(items)
    }

    async fn item_by_id(&self, user_id: &str, item_id: &str) -> anyhow::Result<Option<JoplinItem>>;

    async fn active_item_refs(&self, user_id: &str) -> anyhow::Result<Option<Vec<JoplinItemRef>>> {
        let _ = user_id;
        Ok(None)
    }

    async fn source_watermarks(
        &self,
        user_ids: &[String],
    ) -> anyhow::Result<Vec<JoplinSourceWatermark>>;
}

#[derive(Debug, Clone)]
pub struct JoplinDbSource {
    pool: PgPool,
}

impl JoplinDbSource {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl JoplinSource for JoplinDbSource {
    async fn changed_items_since(
        &self,
        user_id: &str,
        since: Option<i64>,
    ) -> anyhow::Result<Vec<JoplinItem>> {
        let mut all_items = Vec::new();
        let mut after = None;

        loop {
            let batch = self
                .changed_items_batch(user_id, since, after.as_ref(), JOPLIN_ITEM_BATCH_SIZE)
                .await?;
            if batch.is_empty() {
                break;
            }

            let batch_len = batch.len();
            after = batch.last().map(JoplinItemCursor::from);
            all_items.extend(batch);
            if batch_len < JOPLIN_ITEM_BATCH_SIZE as usize {
                break;
            }
        }

        Ok(all_items)
    }

    async fn changed_items_batch(
        &self,
        user_id: &str,
        since: Option<i64>,
        after: Option<&JoplinItemCursor>,
        limit: u32,
    ) -> anyhow::Result<Vec<JoplinItem>> {
        let after_updated_time = after.map(|cursor| cursor.updated_time);
        let after_id = after.map(|cursor| cursor.id.as_str());
        let mut conn = db_pool::acquire_indexer(&self.pool).await?;

        let rows = sqlx::query_as::<_, RawJoplinItem>(CHANGED_ITEMS_BATCH_QUERY)
            .bind(user_id)
            .bind(since)
            .bind(after_updated_time)
            .bind(after_id)
            .bind(i64::from(limit.max(1)))
            .fetch_all(&mut *conn)
            .await
            .context("load changed Joplin item batch")?;

        rows.into_iter().map(JoplinItem::try_from).collect()
    }

    async fn item_by_id(&self, user_id: &str, item_id: &str) -> anyhow::Result<Option<JoplinItem>> {
        let mut conn = db_pool::acquire_indexer(&self.pool).await?;

        let row = sqlx::query_as::<_, RawJoplinItem>(ITEM_BY_ID_QUERY)
            .bind(user_id)
            .bind(item_id)
            .fetch_optional(&mut *conn)
            .await
            .context("load Joplin item by id")?;

        row.map(JoplinItem::try_from).transpose()
    }

    async fn active_item_refs(&self, user_id: &str) -> anyhow::Result<Option<Vec<JoplinItemRef>>> {
        let mut conn = db_pool::acquire_indexer(&self.pool).await?;

        let rows = sqlx::query_as::<_, RawJoplinItemRef>(ACTIVE_ITEM_REFS_QUERY)
            .bind(user_id)
            .fetch_all(&mut *conn)
            .await
            .context("load active Joplin item references")?;

        rows.into_iter()
            .map(JoplinItemRef::try_from)
            .collect::<anyhow::Result<Vec<_>>>()
            .map(Some)
    }

    async fn source_watermarks(
        &self,
        user_ids: &[String],
    ) -> anyhow::Result<Vec<JoplinSourceWatermark>> {
        if user_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut conn = db_pool::acquire_indexer(&self.pool).await?;

        sqlx::query_as::<_, RawJoplinSourceWatermark>(SOURCE_WATERMARKS_QUERY)
            .bind(user_ids)
            .fetch_all(&mut *conn)
            .await
            .context("load Joplin source watermarks")
            .map(|rows| {
                rows.into_iter()
                    .map(|row| JoplinSourceWatermark {
                        owner_id: row.owner_id,
                        source_watermark: row.source_watermark,
                    })
                    .collect()
            })
    }
}

fn item_is_after_cursor(item: &JoplinItem, cursor: &JoplinItemCursor) -> bool {
    item.updated_time > cursor.updated_time
        || (item.updated_time == cursor.updated_time && item.id.as_str() > cursor.id.as_str())
}

#[derive(Debug, FromRow)]
struct RawJoplinItem {
    id: String,
    owner_id: String,
    content: Vec<u8>,
    name: String,
    mime_type: String,
    updated_time: i64,
    created_time: i64,
    jop_id: String,
    jop_parent_id: String,
    jop_type: i32,
    jop_encryption_applied: i32,
}

#[derive(Debug, FromRow)]
struct RawJoplinItemRef {
    jop_id: String,
    jop_type: i32,
}

#[derive(Debug, FromRow)]
struct RawJoplinSourceWatermark {
    owner_id: String,
    source_watermark: i64,
}

impl TryFrom<RawJoplinItem> for JoplinItem {
    type Error = anyhow::Error;

    fn try_from(raw: RawJoplinItem) -> Result<Self, Self::Error> {
        let item_type = JoplinItemType::from_i32(raw.jop_type)
            .with_context(|| format!("unsupported Joplin item type {}", raw.jop_type))?;
        Ok(Self {
            id: raw.id,
            owner_id: raw.owner_id,
            content: raw.content,
            name: raw.name,
            mime_type: raw.mime_type,
            updated_time: raw.updated_time,
            created_time: raw.created_time,
            jop_id: raw.jop_id,
            jop_parent_id: raw.jop_parent_id,
            item_type,
            encrypted: raw.jop_encryption_applied != 0,
        })
    }
}

impl TryFrom<RawJoplinItemRef> for JoplinItemRef {
    type Error = anyhow::Error;

    fn try_from(raw: RawJoplinItemRef) -> Result<Self, Self::Error> {
        let item_type = JoplinItemType::from_i32(raw.jop_type)
            .with_context(|| format!("unsupported Joplin item type {}", raw.jop_type))?;
        Ok(Self {
            joplin_id: raw.jop_id,
            item_type,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_are_owner_only_and_do_not_use_share_tables() {
        for query in [CHANGED_ITEMS_QUERY, ITEM_BY_ID_QUERY] {
            assert!(query.contains("owner_id = $1"));
            assert!(!query.contains("user_items"));
            assert!(!query.contains("shares"));
            assert!(!query.contains("share_users"));
        }
    }

    #[test]
    fn queries_exclude_revisions_and_unknown_item_types() {
        for query in [CHANGED_ITEMS_QUERY, ITEM_BY_ID_QUERY] {
            assert!(query.contains("jop_type IN (1, 2, 5, 6, 9)"));
            assert!(!query.contains("13"));
        }
    }

    #[test]
    fn active_item_refs_support_reconciliation_without_note_tag_edges() {
        assert!(ACTIVE_ITEM_REFS_QUERY.contains("owner_id = $1"));
        assert!(ACTIVE_ITEM_REFS_QUERY.contains("jop_type IN (1, 2, 5, 9)"));
        assert!(!ACTIVE_ITEM_REFS_QUERY.contains("6"));
        assert!(ACTIVE_ITEM_REFS_QUERY.contains("ORDER BY jop_id ASC"));
    }

    #[test]
    fn changed_items_query_supports_optional_since_filter() {
        assert!(CHANGED_ITEMS_QUERY.contains("$2::bigint IS NULL OR updated_time > $2"));
        assert!(CHANGED_ITEMS_QUERY.contains("ORDER BY updated_time ASC, id ASC"));
    }

    #[test]
    fn changed_items_batch_query_uses_keyset_cursor_and_limit() {
        assert!(CHANGED_ITEMS_BATCH_QUERY.contains("updated_time > $3"));
        assert!(CHANGED_ITEMS_BATCH_QUERY.contains("updated_time = $3 AND id > $4"));
        assert!(CHANGED_ITEMS_BATCH_QUERY.contains("ORDER BY updated_time ASC, id ASC"));
        assert!(CHANGED_ITEMS_BATCH_QUERY.contains("LIMIT $5"));
    }

    #[test]
    fn source_watermark_query_is_grouped_and_content_free() {
        assert!(SOURCE_WATERMARKS_QUERY.contains("owner_id = ANY($1)"));
        assert!(SOURCE_WATERMARKS_QUERY.contains("max(updated_time) AS source_watermark"));
        assert!(SOURCE_WATERMARKS_QUERY.contains("GROUP BY owner_id"));
        assert!(!SOURCE_WATERMARKS_QUERY.contains("content"));
    }

    #[test]
    fn maps_raw_item_and_marks_encryption() {
        let raw = RawJoplinItem {
            id: "server-id".to_string(),
            owner_id: "user-id".to_string(),
            content: b"body".to_vec(),
            name: "note.md".to_string(),
            mime_type: "text/plain".to_string(),
            updated_time: 20,
            created_time: 10,
            jop_id: "client-id".to_string(),
            jop_parent_id: "parent-id".to_string(),
            jop_type: JoplinItemType::Note as i32,
            jop_encryption_applied: 1,
        };

        let item = JoplinItem::try_from(raw).expect("raw item maps");

        assert_eq!(item.item_type, JoplinItemType::Note);
        assert!(item.encrypted);
    }

    #[test]
    fn rejects_unexpected_item_type_after_query_mapping() {
        let raw = RawJoplinItem {
            id: "server-id".to_string(),
            owner_id: "user-id".to_string(),
            content: Vec::new(),
            name: "unknown".to_string(),
            mime_type: "application/octet-stream".to_string(),
            updated_time: 20,
            created_time: 10,
            jop_id: "client-id".to_string(),
            jop_parent_id: String::new(),
            jop_type: 99,
            jop_encryption_applied: 0,
        };

        let err = JoplinItem::try_from(raw).expect_err("unknown type is rejected");
        assert!(err.to_string().contains("unsupported Joplin item type 99"));
    }

    #[test]
    fn selected_columns_match_source_contract() {
        for column in [
            "id",
            "owner_id",
            "content",
            "name",
            "mime_type",
            "updated_time",
            "created_time",
            "jop_id",
            "jop_parent_id",
            "jop_type",
            "jop_encryption_applied",
        ] {
            assert!(SELECT_ITEM_COLUMNS.contains(column));
        }
    }

    #[tokio::test]
    async fn default_changed_item_batch_uses_cursor_and_limit() {
        #[derive(Debug)]
        struct MemorySource {
            items: Vec<JoplinItem>,
        }

        #[async_trait]
        impl JoplinSource for MemorySource {
            async fn changed_items_since(
                &self,
                _user_id: &str,
                since: Option<i64>,
            ) -> anyhow::Result<Vec<JoplinItem>> {
                Ok(self
                    .items
                    .iter()
                    .filter(|item| since.is_none_or(|since| item.updated_time > since))
                    .cloned()
                    .collect())
            }

            async fn item_by_id(
                &self,
                _user_id: &str,
                _item_id: &str,
            ) -> anyhow::Result<Option<JoplinItem>> {
                Ok(None)
            }

            async fn source_watermarks(
                &self,
                _user_ids: &[String],
            ) -> anyhow::Result<Vec<JoplinSourceWatermark>> {
                Ok(Vec::new())
            }
        }

        let source = MemorySource {
            items: vec![
                item("server-c", 12),
                item("server-a", 11),
                item("server-b", 12),
            ],
        };

        let first = source
            .changed_items_batch("owner", Some(10), None, 2)
            .await
            .expect("first batch");
        let cursor = JoplinItemCursor::from(first.last().expect("cursor item"));
        let second = source
            .changed_items_batch("owner", Some(10), Some(&cursor), 2)
            .await
            .expect("second batch");

        assert_eq!(
            first
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["server-a", "server-b"]
        );
        assert_eq!(
            second
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["server-c"]
        );
    }

    fn item(id: &str, updated_time: i64) -> JoplinItem {
        JoplinItem {
            id: id.to_string(),
            owner_id: "owner".to_string(),
            content: Vec::new(),
            name: String::new(),
            mime_type: String::new(),
            updated_time,
            created_time: updated_time,
            jop_id: id.to_string(),
            jop_parent_id: String::new(),
            item_type: JoplinItemType::Note,
            encrypted: false,
        }
    }
}
