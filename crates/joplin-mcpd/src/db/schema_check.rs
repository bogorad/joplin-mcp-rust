use anyhow::{Context, bail};
use sqlx::{PgPool, Row};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredColumn {
    pub table: &'static str,
    pub column: &'static str,
    pub data_types: &'static [&'static str],
}

pub const REQUIRED_JOPLIN_COLUMNS: &[RequiredColumn] = &[
    RequiredColumn {
        table: "users",
        column: "id",
        data_types: &["character varying"],
    },
    RequiredColumn {
        table: "users",
        column: "email",
        data_types: &["character varying", "text"],
    },
    RequiredColumn {
        table: "items",
        column: "id",
        data_types: &["character varying"],
    },
    RequiredColumn {
        table: "items",
        column: "owner_id",
        data_types: &["character varying"],
    },
    RequiredColumn {
        table: "items",
        column: "content",
        data_types: &["bytea"],
    },
    RequiredColumn {
        table: "items",
        column: "name",
        data_types: &["text"],
    },
    RequiredColumn {
        table: "items",
        column: "mime_type",
        data_types: &["character varying", "text"],
    },
    RequiredColumn {
        table: "items",
        column: "updated_time",
        data_types: &["bigint"],
    },
    RequiredColumn {
        table: "items",
        column: "created_time",
        data_types: &["bigint"],
    },
    RequiredColumn {
        table: "items",
        column: "jop_id",
        data_types: &["character varying", "text"],
    },
    RequiredColumn {
        table: "items",
        column: "jop_parent_id",
        data_types: &["character varying", "text"],
    },
    RequiredColumn {
        table: "items",
        column: "jop_type",
        data_types: &["integer"],
    },
    RequiredColumn {
        table: "items",
        column: "jop_encryption_applied",
        data_types: &["integer"],
    },
];

const EXTERNAL_STORAGE_ERROR: &str = "items.content is empty across sampled rows; external Joplin content storage is unsupported in joplin-mcpd v1";

pub async fn validate_joplin_source_schema(pool: &PgPool) -> anyhow::Result<()> {
    let actual = fetch_actual_columns(pool).await?;
    validate_columns(&actual)?;
    validate_external_storage(pool).await?;
    Ok(())
}

pub async fn fetch_actual_columns(
    pool: &PgPool,
) -> anyhow::Result<BTreeMap<(String, String), String>> {
    let rows = sqlx::query(
        r#"
        SELECT table_name, column_name, data_type
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name IN ('users', 'items')
        ORDER BY table_name, column_name
        "#,
    )
    .fetch_all(pool)
    .await
    .context("read Joplin source information_schema columns")?;

    let mut actual = BTreeMap::new();
    for row in rows {
        actual.insert(
            (
                row.get::<String, _>("table_name"),
                row.get::<String, _>("column_name"),
            ),
            row.get::<String, _>("data_type"),
        );
    }

    Ok(actual)
}

pub fn validate_columns(actual: &BTreeMap<(String, String), String>) -> anyhow::Result<()> {
    for required in REQUIRED_JOPLIN_COLUMNS {
        let key = (required.table.to_string(), required.column.to_string());
        let Some(actual_type) = actual.get(&key) else {
            anyhow::bail!(
                "missing required Joplin column {}.{}",
                required.table,
                required.column
            );
        };
        if !required.data_types.contains(&actual_type.as_str()) {
            anyhow::bail!(
                "wrong Joplin column type for {}.{}: expected one of {}, got {}",
                required.table,
                required.column,
                required.data_types.join(", "),
                actual_type
            );
        }
    }
    Ok(())
}

pub async fn validate_external_storage(pool: &PgPool) -> anyhow::Result<()> {
    let rows = sqlx::query(
        r#"
        SELECT content
        FROM items
        WHERE jop_encryption_applied = 0
          AND jop_type <> 9
        ORDER BY updated_time DESC, id
        LIMIT 100
        "#,
    )
    .fetch_all(pool)
    .await
    .context("sample Joplin item content")?;

    let mut sample = Vec::with_capacity(rows.len());
    for row in rows {
        sample.push(row.try_get::<Option<Vec<u8>>, _>("content")?);
    }

    validate_external_storage_sample(&sample)
}

pub fn validate_external_storage_sample(sample: &[Option<Vec<u8>>]) -> anyhow::Result<()> {
    if sample.is_empty() {
        return Ok(());
    }
    if sample
        .iter()
        .all(|content| content.as_ref().is_none_or(Vec::is_empty))
    {
        bail!(EXTERNAL_STORAGE_ERROR);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_fixture_joplin_columns() {
        validate_columns(&fixture_columns()).expect("fixture schema is valid");
    }

    #[test]
    fn accepts_live_joplin_text_columns_for_string_metadata() {
        let mut actual = fixture_columns();
        actual.insert(
            ("users".to_string(), "email".to_string()),
            "text".to_string(),
        );
        for column in ["mime_type", "jop_id", "jop_parent_id"] {
            actual.insert(
                ("items".to_string(), column.to_string()),
                "text".to_string(),
            );
        }

        validate_columns(&actual).expect("live text-backed string columns are valid");
    }

    #[test]
    fn detects_missing_required_column() {
        let actual = BTreeMap::new();
        let err = validate_columns(&actual).expect_err("schema is invalid");
        assert!(err.to_string().contains("missing required Joplin column"));
    }

    #[test]
    fn detects_wrong_required_column_type() {
        let mut actual = fixture_columns();
        actual.insert(
            ("items".to_string(), "updated_time".to_string()),
            "text".to_string(),
        );

        let err = validate_columns(&actual).expect_err("schema is invalid");
        assert!(
            err.to_string()
                .contains("wrong Joplin column type for items.updated_time")
        );
    }

    #[test]
    fn ignores_empty_external_storage_sample() {
        validate_external_storage_sample(&[]).expect("empty server is allowed");
    }

    #[test]
    fn accepts_sample_with_content() {
        validate_external_storage_sample(&[
            None,
            Some(Vec::new()),
            Some(b"id: note\nbody".to_vec()),
        ])
        .expect("content-backed server is allowed");
    }

    #[test]
    fn rejects_sample_with_only_empty_content() {
        let err = validate_external_storage_sample(&[None, Some(Vec::new())])
            .expect_err("external storage is rejected");
        assert_eq!(err.to_string(), EXTERNAL_STORAGE_ERROR);
    }

    fn fixture_columns() -> BTreeMap<(String, String), String> {
        let mut actual = BTreeMap::new();
        for required in REQUIRED_JOPLIN_COLUMNS {
            actual.insert(
                (required.table.to_string(), required.column.to_string()),
                required.data_types[0].to_string(),
            );
        }
        actual
    }
}
