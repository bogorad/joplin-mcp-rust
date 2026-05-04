use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};

pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
pub const READ_ONLY_MODE: &str = "read-only";
pub const UNSUPPORTED_SHARED_NOTEBOOKS: &str = "unsupported";

pub const MODULE_BOUNDARIES: &[&str] = &[
    "auth does not parse notes",
    "indexer does not validate HTTP tokens",
    "mcp tools read index tables only",
    "logging never receives raw secrets",
    "client never connects to Postgres",
];

pub const SCOPE_LIMITS: &[&str] = &[
    "unencrypted-only",
    "read-only",
    "owner-items-only",
    "lan-only",
    "no-e2ee-decryption",
    "no-write-tools",
    "no-recipient-shared-notebook-visibility",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Validation,
    Auth,
    NotFound,
    IndexNotReady,
    RateLimited,
    Unsupported,
    Internal,
}

impl Display for ErrorCode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let code = match self {
            Self::Validation => "validation",
            Self::Auth => "auth",
            Self::NotFound => "not_found",
            Self::IndexNotReady => "index_not_ready",
            Self::RateLimited => "rate_limited",
            Self::Unsupported => "unsupported",
            Self::Internal => "internal",
        };
        f.write_str(code)
    }
}

pub fn is_joplin_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn is_joplin_user_id(value: &str) -> bool {
    is_joplin_id(value)
        || (value.len() == 22
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_32_char_hex_joplin_ids() {
        assert!(is_joplin_id("0123456789abcdef0123456789abcdef"));
        assert!(is_joplin_user_id("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(is_joplin_user_id("Abcdefghijklmnopqr_12-"));
        assert!(!is_joplin_id("0123456789abcdef0123456789abcde"));
        assert!(!is_joplin_id("0123456789abcdef0123456789abcdeg"));
        assert!(!is_joplin_id("0123456789abcdef0123456789abcdef00"));
        assert!(!is_joplin_user_id("Abcdefghijklmnopqr_12!"));
    }

    #[test]
    fn exposes_required_error_codes() {
        let codes = [
            ErrorCode::Validation,
            ErrorCode::Auth,
            ErrorCode::NotFound,
            ErrorCode::IndexNotReady,
            ErrorCode::RateLimited,
            ErrorCode::Unsupported,
            ErrorCode::Internal,
        ];
        let rendered: Vec<String> = codes.iter().map(ToString::to_string).collect();
        assert_eq!(
            rendered,
            [
                "validation",
                "auth",
                "not_found",
                "index_not_ready",
                "rate_limited",
                "unsupported",
                "internal"
            ]
        );
    }
}
