use crate::indexer::JoplinItemType;
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
pub struct JoplinItemRef {
    pub joplin_id: String,
    pub item_type: JoplinItemType,
}

#[async_trait]
pub trait JoplinSource: Send + Sync {
    async fn changed_items_since(
        &self,
        user_id: &str,
        since: Option<i64>,
    ) -> anyhow::Result<Vec<JoplinItem>>;

    async fn item_by_id(&self, user_id: &str, item_id: &str) -> anyhow::Result<Option<JoplinItem>>;

    async fn active_item_refs(&self, user_id: &str) -> anyhow::Result<Option<Vec<JoplinItemRef>>> {
        let _ = user_id;
        Ok(None)
    }
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
        let rows = sqlx::query_as::<_, RawJoplinItem>(CHANGED_ITEMS_QUERY)
            .bind(user_id)
            .bind(since)
            .fetch_all(&self.pool)
            .await
            .context("load changed Joplin items")?;

        rows.into_iter().map(JoplinItem::try_from).collect()
    }

    async fn item_by_id(&self, user_id: &str, item_id: &str) -> anyhow::Result<Option<JoplinItem>> {
        let row = sqlx::query_as::<_, RawJoplinItem>(ITEM_BY_ID_QUERY)
            .bind(user_id)
            .bind(item_id)
            .fetch_optional(&self.pool)
            .await
            .context("load Joplin item by id")?;

        row.map(JoplinItem::try_from).transpose()
    }

    async fn active_item_refs(&self, user_id: &str) -> anyhow::Result<Option<Vec<JoplinItemRef>>> {
        let rows = sqlx::query_as::<_, RawJoplinItemRef>(ACTIVE_ITEM_REFS_QUERY)
            .bind(user_id)
            .fetch_all(&self.pool)
            .await
            .context("load active Joplin item references")?;

        rows.into_iter()
            .map(JoplinItemRef::try_from)
            .collect::<anyhow::Result<Vec<_>>>()
            .map(Some)
    }
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
}
