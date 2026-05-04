use crate::contracts::is_joplin_user_id;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

pub const BOOTSTRAP_AUTH_FAILED_MESSAGE: &str = "bootstrap authentication failed";

const JOPLIN_USER_BY_ID_QUERY: &str = r#"
    SELECT id, email
    FROM users
    WHERE id = $1
"#;

const MCP_USER_UPSERT_QUERY: &str = r#"
    INSERT INTO joplin_mcp.mcp_users (id, joplin_user_id, joplin_email, last_login_at)
    VALUES ($1, $2, $3, now())
    ON CONFLICT (joplin_user_id)
    DO UPDATE SET
      joplin_email = EXCLUDED.joplin_email,
      last_login_at = now()
    RETURNING id, joplin_user_id, joplin_email
"#;

pub type BootstrapAuthResult<T> = Result<T, BootstrapAuthError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapAuthFailure {
    JoplinRejected,
    JoplinUnavailable,
    InvalidJoplinResponse,
    JoplinUserNotFound,
    McpUserUpsertFailed,
    SessionInvalidationFailed,
    RateLimited,
}

impl BootstrapAuthFailure {
    pub fn sanitized_detail(self) -> &'static str {
        match self {
            Self::JoplinRejected => "joplin_rejected_credentials",
            Self::JoplinUnavailable => "joplin_unavailable",
            Self::InvalidJoplinResponse => "invalid_joplin_session_response",
            Self::JoplinUserNotFound => "joplin_user_not_found",
            Self::McpUserUpsertFailed => "mcp_user_upsert_failed",
            Self::SessionInvalidationFailed => "joplin_session_invalidation_failed",
            Self::RateLimited => "rate_limited",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("bootstrap authentication failed")]
pub struct BootstrapAuthError {
    failure: BootstrapAuthFailure,
}

impl BootstrapAuthError {
    pub fn new(failure: BootstrapAuthFailure) -> Self {
        Self { failure }
    }

    pub fn failure(&self) -> BootstrapAuthFailure {
        self.failure
    }

    pub fn client_message(&self) -> &'static str {
        BOOTSTRAP_AUTH_FAILED_MESSAGE
    }

    pub fn sanitized_detail(&self) -> &'static str {
        self.failure.sanitized_detail()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoplinSession {
    pub id: String,
    pub user_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct JoplinUser {
    pub id: String,
    pub email: String,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct McpUser {
    pub id: Uuid,
    pub joplin_user_id: String,
    pub joplin_email: String,
}

#[async_trait]
pub trait JoplinAuthenticator: Send + Sync {
    async fn authenticate(&self, email: &str, password: &str)
    -> BootstrapAuthResult<JoplinSession>;
    async fn invalidate_session(&self, session_id: &str) -> BootstrapAuthResult<()>;
}

#[derive(Debug, Clone)]
pub struct HttpJoplinAuthenticator {
    client: reqwest::Client,
    base_url: url::Url,
}

impl HttpJoplinAuthenticator {
    pub fn new(base_url: url::Url) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
        }
    }
}

#[async_trait]
impl JoplinAuthenticator for HttpJoplinAuthenticator {
    async fn authenticate(
        &self,
        email: &str,
        password: &str,
    ) -> BootstrapAuthResult<JoplinSession> {
        #[derive(Serialize)]
        struct AuthRequest<'a> {
            email: &'a str,
            password: &'a str,
        }

        let url = self
            .base_url
            .join("/api/sessions")
            .map_err(|_| BootstrapAuthError::new(BootstrapAuthFailure::JoplinUnavailable))?;
        let response = self
            .client
            .post(url)
            .json(&AuthRequest { email, password })
            .send()
            .await
            .map_err(|_| BootstrapAuthError::new(BootstrapAuthFailure::JoplinUnavailable))?;

        if !response.status().is_success() {
            return Err(BootstrapAuthError::new(
                BootstrapAuthFailure::JoplinRejected,
            ));
        }

        let body = response
            .bytes()
            .await
            .map_err(|_| BootstrapAuthError::new(BootstrapAuthFailure::JoplinUnavailable))?;
        parse_joplin_session_response(&body)
    }

    async fn invalidate_session(&self, session_id: &str) -> BootstrapAuthResult<()> {
        let url = self
            .base_url
            .join(&format!("/api/sessions/{session_id}"))
            .map_err(|_| {
                BootstrapAuthError::new(BootstrapAuthFailure::SessionInvalidationFailed)
            })?;
        let response = self.client.delete(url).send().await.map_err(|_| {
            BootstrapAuthError::new(BootstrapAuthFailure::SessionInvalidationFailed)
        })?;
        if response.status().is_success() || response.status().as_u16() == 404 {
            return Ok(());
        }
        if response.status().as_u16() == 400 {
            let body = response.text().await.unwrap_or_default();
            if body.contains("Not allowed") {
                return Ok(());
            }
        }
        Err(BootstrapAuthError::new(
            BootstrapAuthFailure::SessionInvalidationFailed,
        ))
    }
}

pub fn parse_joplin_session_response(body: &[u8]) -> BootstrapAuthResult<JoplinSession> {
    #[derive(Deserialize)]
    struct RawJoplinSession {
        id: Option<String>,
        user_id: Option<String>,
    }

    let raw: RawJoplinSession = serde_json::from_slice(body)
        .map_err(|_| BootstrapAuthError::new(BootstrapAuthFailure::InvalidJoplinResponse))?;
    let id = raw
        .id
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| BootstrapAuthError::new(BootstrapAuthFailure::InvalidJoplinResponse))?;
    let user_id = raw
        .user_id
        .filter(|user_id| is_joplin_user_id(user_id))
        .ok_or_else(|| BootstrapAuthError::new(BootstrapAuthFailure::InvalidJoplinResponse))?;

    Ok(JoplinSession { id, user_id })
}

pub async fn resolve_joplin_user_by_id(
    pool: &PgPool,
    joplin_user_id: &str,
) -> BootstrapAuthResult<JoplinUser> {
    if !is_joplin_user_id(joplin_user_id) {
        return Err(BootstrapAuthError::new(
            BootstrapAuthFailure::InvalidJoplinResponse,
        ));
    }

    sqlx::query_as::<_, JoplinUser>(JOPLIN_USER_BY_ID_QUERY)
        .bind(joplin_user_id)
        .fetch_optional(pool)
        .await
        .map_err(|_| BootstrapAuthError::new(BootstrapAuthFailure::JoplinUnavailable))?
        .ok_or_else(|| BootstrapAuthError::new(BootstrapAuthFailure::JoplinUserNotFound))
}

pub async fn upsert_mcp_user_by_joplin_user(
    pool: &PgPool,
    joplin_user: &JoplinUser,
) -> BootstrapAuthResult<McpUser> {
    if !is_joplin_user_id(&joplin_user.id) {
        return Err(BootstrapAuthError::new(
            BootstrapAuthFailure::InvalidJoplinResponse,
        ));
    }

    sqlx::query_as::<_, McpUser>(MCP_USER_UPSERT_QUERY)
        .bind(Uuid::new_v4())
        .bind(&joplin_user.id)
        .bind(&joplin_user.email)
        .fetch_one(pool)
        .await
        .map_err(|_| BootstrapAuthError::new(BootstrapAuthFailure::McpUserUpsertFailed))
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER_ID: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn parses_valid_joplin_session_response() {
        let session = parse_joplin_session_response(
            br#"{"id":"joplin-session-id","user_id":"0123456789abcdef0123456789abcdef"}"#,
        )
        .expect("session response parses");

        assert_eq!(session.id, "joplin-session-id");
        assert_eq!(session.user_id, USER_ID);
    }

    #[test]
    fn parses_live_joplin_session_response_with_short_user_id() {
        let session = parse_joplin_session_response(
            br#"{"id":"joplin-session-id","user_id":"Abcdefghijklmnopqr_12-"}"#,
        )
        .expect("live session response parses");

        assert_eq!(session.user_id, "Abcdefghijklmnopqr_12-");
    }

    #[test]
    fn rejects_missing_or_invalid_joplin_session_identity() {
        for body in [
            br#"{}"#.as_slice(),
            br#"{"id":"","user_id":"0123456789abcdef0123456789abcdef"}"#.as_slice(),
            br#"{"id":"session","user_id":""}"#.as_slice(),
            br#"{"id":"session","user_id":"not-a-joplin-id"}"#.as_slice(),
            br#"{"id":"session"}"#.as_slice(),
        ] {
            let err = parse_joplin_session_response(body).expect_err("response is rejected");
            assert_eq!(err.failure(), BootstrapAuthFailure::InvalidJoplinResponse);
            assert_eq!(err.client_message(), BOOTSTRAP_AUTH_FAILED_MESSAGE);
            assert_eq!(err.to_string(), BOOTSTRAP_AUTH_FAILED_MESSAGE);
        }
    }

    #[test]
    fn exposes_uniform_client_visible_bootstrap_errors() {
        for failure in [
            BootstrapAuthFailure::JoplinRejected,
            BootstrapAuthFailure::JoplinUnavailable,
            BootstrapAuthFailure::InvalidJoplinResponse,
            BootstrapAuthFailure::JoplinUserNotFound,
            BootstrapAuthFailure::McpUserUpsertFailed,
            BootstrapAuthFailure::SessionInvalidationFailed,
            BootstrapAuthFailure::RateLimited,
        ] {
            let err = BootstrapAuthError::new(failure);
            assert_eq!(err.client_message(), BOOTSTRAP_AUTH_FAILED_MESSAGE);
            assert_eq!(err.to_string(), BOOTSTRAP_AUTH_FAILED_MESSAGE);
            assert!(!err.sanitized_detail().contains('@'));
            assert!(!err.sanitized_detail().contains("password"));
            assert!(!err.sanitized_detail().contains("session-id"));
        }
    }

    #[test]
    fn joplin_lookup_uses_user_id_only() {
        assert!(JOPLIN_USER_BY_ID_QUERY.contains("FROM users"));
        assert!(JOPLIN_USER_BY_ID_QUERY.contains("WHERE id = $1"));
        assert!(!JOPLIN_USER_BY_ID_QUERY.contains("WHERE email"));
    }

    #[test]
    fn mcp_user_upsert_keys_identity_on_joplin_user_id() {
        assert!(MCP_USER_UPSERT_QUERY.contains("joplin_mcp.mcp_users"));
        assert!(MCP_USER_UPSERT_QUERY.contains("ON CONFLICT (joplin_user_id)"));
        assert!(MCP_USER_UPSERT_QUERY.contains("joplin_email = EXCLUDED.joplin_email"));
        assert!(MCP_USER_UPSERT_QUERY.contains("last_login_at = now()"));
    }
}
