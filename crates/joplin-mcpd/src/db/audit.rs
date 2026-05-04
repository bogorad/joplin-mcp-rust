use crate::db::pool as db_pool;
use serde_json::{Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditEventType {
    BootstrapLogin,
    TokenMint,
    TokenRevoke,
    SchemaValidation,
    IndexerFailure,
}

impl AuditEventType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BootstrapLogin => "bootstrap_login",
            Self::TokenMint => "token_mint",
            Self::TokenRevoke => "token_revoke",
            Self::SchemaValidation => "schema_validation",
            Self::IndexerFailure => "indexer_failure",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditOutcome {
    Success,
    Failed,
    Noop,
}

impl AuditOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Noop => "noop",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRecord {
    pub user_id: Option<Uuid>,
    pub event_type: AuditEventType,
    pub outcome: AuditOutcome,
    pub client_label: Option<String>,
    pub remote_ip: Option<String>,
    pub metadata: Value,
}

impl AuditRecord {
    pub fn new(event_type: AuditEventType, outcome: AuditOutcome) -> Self {
        Self {
            user_id: None,
            event_type,
            outcome,
            client_label: None,
            remote_ip: None,
            metadata: Value::Object(Map::new()),
        }
    }

    pub fn user_id(mut self, user_id: Uuid) -> Self {
        self.user_id = Some(user_id);
        self
    }

    pub fn client_label(mut self, client_label: impl Into<String>) -> Self {
        self.client_label = Some(client_label.into());
        self
    }

    pub fn remote_ip(mut self, remote_ip: Option<String>) -> Self {
        self.remote_ip = remote_ip;
        self
    }

    pub fn metadata(mut self, metadata: Value) -> Self {
        self.metadata = sanitize_metadata(metadata);
        self
    }
}

pub async fn write_audit_log(pool: &PgPool, record: AuditRecord) -> anyhow::Result<()> {
    let mut conn = db_pool::acquire_runtime(pool).await?;

    sqlx::query(
        r#"
        INSERT INTO joplin_mcp.audit_log (
            id,
            user_id,
            event_type,
            outcome,
            client_label,
            remote_ip,
            metadata
        )
        VALUES ($1, $2, $3, $4, $5, CAST($6 AS inet), $7)
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(record.user_id)
    .bind(record.event_type.as_str())
    .bind(record.outcome.as_str())
    .bind(record.client_label.as_deref())
    .bind(record.remote_ip.as_deref())
    .bind(record.metadata)
    .execute(&mut *conn)
    .await?;

    Ok(())
}

pub fn sanitize_metadata(value: Value) -> Value {
    sanitize_value(None, value)
}

fn sanitize_value(key: Option<&str>, value: Value) -> Value {
    if key.is_some_and(is_forbidden_key) {
        return Value::String("[redacted]".to_string());
    }

    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| {
                    let sanitized = sanitize_value(Some(&key), value);
                    (key, sanitized)
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| sanitize_value(None, value))
                .collect(),
        ),
        Value::String(value) => Value::String(truncate_string(value)),
        other => other,
    }
}

fn is_forbidden_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase();
    [
        "authorization",
        "auth_header",
        "body",
        "decrypted",
        "password",
        "raw_token",
        "secret",
        "token",
        "token_hash",
    ]
    .iter()
    .any(|forbidden| normalized.contains(forbidden))
}

fn truncate_string(value: String) -> String {
    const MAX_METADATA_STRING_CHARS: usize = 256;
    if value.chars().count() <= MAX_METADATA_STRING_CHARS {
        return value;
    }

    value
        .chars()
        .take(MAX_METADATA_STRING_CHARS)
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn event_and_outcome_strings_match_schema_values() {
        assert_eq!(AuditEventType::BootstrapLogin.as_str(), "bootstrap_login");
        assert_eq!(AuditEventType::TokenMint.as_str(), "token_mint");
        assert_eq!(AuditEventType::TokenRevoke.as_str(), "token_revoke");
        assert_eq!(
            AuditEventType::SchemaValidation.as_str(),
            "schema_validation"
        );
        assert_eq!(AuditEventType::IndexerFailure.as_str(), "indexer_failure");
        assert_eq!(AuditOutcome::Success.as_str(), "success");
        assert_eq!(AuditOutcome::Failed.as_str(), "failed");
        assert_eq!(AuditOutcome::Noop.as_str(), "noop");
    }

    #[test]
    fn metadata_redacts_secrets_and_note_bodies_recursively() {
        let metadata = sanitize_metadata(json!({
            "failure": "joplin_rejected",
            "password": "secret",
            "raw_token": "mcp_secret",
            "token_hash": "hash",
            "authorization": "Bearer mcp_secret",
            "note_body": "note text",
            "nested": {
                "decrypted_secret": "value",
                "safe": "kept"
            }
        }));

        assert_eq!(metadata["failure"], "joplin_rejected");
        assert_eq!(metadata["password"], "[redacted]");
        assert_eq!(metadata["raw_token"], "[redacted]");
        assert_eq!(metadata["token_hash"], "[redacted]");
        assert_eq!(metadata["authorization"], "[redacted]");
        assert_eq!(metadata["note_body"], "[redacted]");
        assert_eq!(metadata["nested"]["decrypted_secret"], "[redacted]");
        assert_eq!(metadata["nested"]["safe"], "kept");
    }

    #[test]
    fn metadata_truncates_long_strings() {
        let metadata = sanitize_metadata(json!({
            "safe": "x".repeat(300)
        }));

        assert_eq!(metadata["safe"].as_str().expect("string").len(), 256);
    }
}
