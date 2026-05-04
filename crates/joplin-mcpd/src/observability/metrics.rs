use once_cell::sync::Lazy;
use opentelemetry::metrics::{Counter, Histogram};
use opentelemetry::{KeyValue, global};
use sha2::{Digest, Sha256};
use std::time::Duration;
use uuid::Uuid;

pub const INDEX_LAG_SECONDS: &str = "index_lag_seconds";
pub const INDEX_REFRESH_DURATION_SECONDS: &str = "index_refresh_duration_seconds";
pub const MCP_TOOL_DURATION_SECONDS: &str = "mcp_tool_duration_seconds";
pub const MCP_TOOL_ERRORS_TOTAL: &str = "mcp_tool_errors_total";
pub const BOOTSTRAP_LOGIN_TOTAL: &str = "bootstrap_login_total";
pub const POSTGRES_POOL_WAIT_SECONDS: &str = "postgres_pool_wait_seconds";

pub const LABEL_USER_HASH: &str = "user.hash";
pub const LABEL_OUTCOME: &str = "outcome";
pub const LABEL_TOOL: &str = "tool";
pub const LABEL_ERROR_KIND: &str = "error.kind";
pub const LABEL_POOL: &str = "pool";

pub const BOOTSTRAP_OUTCOMES: &[&str] = &["success", "failed", "rate_limited"];
pub const INDEX_REFRESH_OUTCOMES: &[&str] = &["success", "failed", "skipped_lock", "full_rebuild"];
pub const POSTGRES_POOLS: &[&str] = &["runtime", "indexer"];
pub const ALERT_INDEX_STATUS_FAILED: &str = "index_status_failed";
pub const ALERT_BOOTSTRAP_ERRORS_ABOVE_THRESHOLD: &str = "bootstrap_errors_above_threshold";
pub const ALERT_VICTORIALOGS_INGESTION_FAILURE: &str = "victorialogs_ingestion_failure";
pub const ALERT_POSTGRES_POOL_SATURATION: &str = "postgres_pool_saturation";
pub const ALERT_MCP_TOOL_ERROR_RATE_ABOVE_THRESHOLD: &str = "mcp_tool_error_rate_above_threshold";
pub const MINIMUM_ALERTS: &[&str] = &[
    ALERT_INDEX_STATUS_FAILED,
    ALERT_BOOTSTRAP_ERRORS_ABOVE_THRESHOLD,
    ALERT_VICTORIALOGS_INGESTION_FAILURE,
    ALERT_POSTGRES_POOL_SATURATION,
    ALERT_MCP_TOOL_ERROR_RATE_ABOVE_THRESHOLD,
];
pub const MCP_ERROR_KINDS: &[&str] = &[
    "validation",
    "auth",
    "not_found",
    "index_not_ready",
    "rate_limited",
    "unsupported",
    "internal",
];

const MCP_TOOLS: &[&str] = &[
    "status",
    "list_notebooks",
    "list_notes",
    "search_notes",
    "get_note",
    "get_note_excerpt",
    "get_recent_notes",
    "list_tags",
    "get_notes_by_tag",
    "get_notebook_tree",
    "get_changes_since",
    "get_note_resources",
];

static INDEX_LAG: Lazy<Histogram<f64>> = Lazy::new(|| {
    global::meter("joplin-mcpd")
        .f64_histogram(INDEX_LAG_SECONDS)
        .with_unit("s")
        .with_description("Seconds since the user's last completed index refresh.")
        .build()
});

static INDEX_REFRESH_DURATION: Lazy<Histogram<f64>> = Lazy::new(|| {
    global::meter("joplin-mcpd")
        .f64_histogram(INDEX_REFRESH_DURATION_SECONDS)
        .with_unit("s")
        .with_description("Index refresh duration in seconds.")
        .build()
});

static MCP_TOOL_DURATION: Lazy<Histogram<f64>> = Lazy::new(|| {
    global::meter("joplin-mcpd")
        .f64_histogram(MCP_TOOL_DURATION_SECONDS)
        .with_unit("s")
        .with_description("MCP tool execution duration in seconds.")
        .build()
});

static MCP_TOOL_ERRORS: Lazy<Counter<u64>> = Lazy::new(|| {
    global::meter("joplin-mcpd")
        .u64_counter(MCP_TOOL_ERRORS_TOTAL)
        .with_description("MCP tool errors by bounded tool and error kind.")
        .build()
});

static BOOTSTRAP_LOGIN: Lazy<Counter<u64>> = Lazy::new(|| {
    global::meter("joplin-mcpd")
        .u64_counter(BOOTSTRAP_LOGIN_TOTAL)
        .with_description("Bootstrap login attempts by bounded outcome.")
        .build()
});

static POSTGRES_POOL_WAIT: Lazy<Histogram<f64>> = Lazy::new(|| {
    global::meter("joplin-mcpd")
        .f64_histogram(POSTGRES_POOL_WAIT_SECONDS)
        .with_unit("s")
        .with_description("Postgres pool connection wait duration in seconds.")
        .build()
});

pub fn record_index_lag(user_id: Uuid, seconds: f64) {
    INDEX_LAG.record(
        seconds,
        &[KeyValue::new(LABEL_USER_HASH, user_hash_label(user_id))],
    );
}

pub fn record_index_refresh_duration(outcome: &str, duration: Duration) {
    INDEX_REFRESH_DURATION.record(
        duration.as_secs_f64(),
        &[KeyValue::new(
            LABEL_OUTCOME,
            bounded(outcome, INDEX_REFRESH_OUTCOMES, "failed"),
        )],
    );
}

pub fn record_mcp_tool_duration(tool: &str, duration: Duration) {
    MCP_TOOL_DURATION.record(
        duration.as_secs_f64(),
        &[KeyValue::new(LABEL_TOOL, bounded_tool(tool))],
    );
}

pub fn record_mcp_tool_error(tool: &str, error_kind: &str) {
    MCP_TOOL_ERRORS.add(
        1,
        &[
            KeyValue::new(LABEL_TOOL, bounded_tool(tool)),
            KeyValue::new(
                LABEL_ERROR_KIND,
                bounded(error_kind, MCP_ERROR_KINDS, "internal"),
            ),
        ],
    );
}

pub fn record_bootstrap_login(outcome: &str) {
    BOOTSTRAP_LOGIN.add(
        1,
        &[KeyValue::new(
            LABEL_OUTCOME,
            bounded(outcome, BOOTSTRAP_OUTCOMES, "failed"),
        )],
    );
}

pub fn record_postgres_pool_wait(pool: &str, duration: Duration) {
    POSTGRES_POOL_WAIT.record(
        duration.as_secs_f64(),
        &[KeyValue::new(
            LABEL_POOL,
            bounded(pool, POSTGRES_POOLS, "runtime"),
        )],
    );
}

pub fn bounded_tool(tool: &str) -> &'static str {
    bounded(tool, MCP_TOOLS, "unknown")
}

fn bounded(value: &str, allowed: &'static [&'static str], fallback: &'static str) -> &'static str {
    allowed
        .iter()
        .copied()
        .find(|candidate| *candidate == value)
        .unwrap_or(fallback)
}

fn user_hash_label(user_id: Uuid) -> String {
    let digest = Sha256::digest(user_id.as_bytes());
    let mut label = String::with_capacity(16);
    for byte in &digest[..8] {
        use std::fmt::Write as _;
        write!(&mut label, "{byte:02x}").expect("write to string");
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_required_metric_names() {
        assert_eq!(INDEX_LAG_SECONDS, "index_lag_seconds");
        assert_eq!(
            INDEX_REFRESH_DURATION_SECONDS,
            "index_refresh_duration_seconds"
        );
        assert_eq!(MCP_TOOL_DURATION_SECONDS, "mcp_tool_duration_seconds");
        assert_eq!(MCP_TOOL_ERRORS_TOTAL, "mcp_tool_errors_total");
        assert_eq!(BOOTSTRAP_LOGIN_TOTAL, "bootstrap_login_total");
        assert_eq!(POSTGRES_POOL_WAIT_SECONDS, "postgres_pool_wait_seconds");
    }

    #[test]
    fn exposes_minimum_alert_definitions() {
        assert_eq!(
            MINIMUM_ALERTS,
            [
                "index_status_failed",
                "bootstrap_errors_above_threshold",
                "victorialogs_ingestion_failure",
                "postgres_pool_saturation",
                "mcp_tool_error_rate_above_threshold"
            ]
        );
    }

    #[test]
    fn keeps_metric_labels_bounded() {
        assert_eq!(bounded_tool("search_notes"), "search_notes");
        assert_eq!(bounded_tool("made_up_tool"), "unknown");
        assert_eq!(bounded("panic", MCP_ERROR_KINDS, "internal"), "internal");
        assert_eq!(bounded("indexer", POSTGRES_POOLS, "runtime"), "indexer");
    }

    #[test]
    fn user_label_is_hash_not_uuid_or_email() {
        let user_id = Uuid::parse_str("00000000-0000-0001-8000-000000000000").expect("uuid");
        let label = user_hash_label(user_id);

        assert_eq!(label.len(), 16);
        assert!(label.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_ne!(label, user_id.to_string());
        assert!(!label.contains('@'));
    }
}
