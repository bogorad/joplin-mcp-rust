use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredColumn {
    pub table: &'static str,
    pub column: &'static str,
    pub data_type: &'static str,
}

pub const REQUIRED_JOPLIN_COLUMNS: &[RequiredColumn] = &[
    RequiredColumn {
        table: "users",
        column: "id",
        data_type: "text",
    },
    RequiredColumn {
        table: "users",
        column: "email",
        data_type: "text",
    },
    RequiredColumn {
        table: "items",
        column: "id",
        data_type: "text",
    },
    RequiredColumn {
        table: "items",
        column: "owner_id",
        data_type: "text",
    },
    RequiredColumn {
        table: "items",
        column: "content",
        data_type: "text",
    },
    RequiredColumn {
        table: "items",
        column: "jop_id",
        data_type: "text",
    },
    RequiredColumn {
        table: "items",
        column: "jop_type",
        data_type: "integer",
    },
    RequiredColumn {
        table: "items",
        column: "jop_encryption_applied",
        data_type: "integer",
    },
];

pub fn validate_columns(actual: &BTreeMap<(&str, &str), &str>) -> anyhow::Result<()> {
    for required in REQUIRED_JOPLIN_COLUMNS {
        let Some(actual_type) = actual.get(&(required.table, required.column)) else {
            anyhow::bail!(
                "missing required Joplin column {}.{}",
                required.table,
                required.column
            );
        };
        if *actual_type != required.data_type {
            anyhow::bail!(
                "wrong Joplin column type for {}.{}: expected {}, got {}",
                required.table,
                required.column,
                required.data_type,
                actual_type
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_missing_required_column() {
        let actual = BTreeMap::new();
        let err = validate_columns(&actual).expect_err("schema is invalid");
        assert!(err.to_string().contains("missing required Joplin column"));
    }
}
