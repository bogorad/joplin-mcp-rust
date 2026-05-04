use crate::{config::HmacKeyConfig, db::pool as db_pool};
use anyhow::Context;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;
use sqlx::{FromRow, PgPool};
use std::fs;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

pub const RAW_TOKEN_PREFIX: &str = "mcp_";
pub const TOKEN_RANDOM_BYTES: usize = 32;
pub const HMAC_KEY_BYTES: usize = 32;
pub const DEFAULT_LAST_SEEN_UPDATE_INTERVAL: Duration = Duration::minutes(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenHash {
    pub hmac_key_id: String,
    pub bytes: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HmacKey {
    pub id: String,
    pub bytes: [u8; HMAC_KEY_BYTES],
}

impl HmacKey {
    pub fn new(id: impl Into<String>, bytes: &[u8]) -> anyhow::Result<Self> {
        validate_hmac_key(bytes)?;
        let mut key = [0_u8; HMAC_KEY_BYTES];
        key.copy_from_slice(bytes);
        Ok(Self {
            id: id.into(),
            bytes: key,
        })
    }
}

pub fn load_hmac_keys(configs: &[HmacKeyConfig]) -> anyhow::Result<Vec<HmacKey>> {
    configs
        .iter()
        .map(|config| {
            let file = config
                .file
                .as_deref()
                .with_context(|| format!("tokens.hmac_keys.{}.file is required", config.id))?;
            let bytes =
                fs::read(file).with_context(|| format!("read HMAC key file for {}", config.id))?;
            HmacKey::new(config.id.clone(), &bytes)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenInsert {
    pub id: Uuid,
    pub user_id: Uuid,
    pub token_hash: TokenHash,
    pub label: String,
    pub scope: String,
    pub created_from_ip: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedToken {
    pub raw_token: String,
    pub insert: TokenInsert,
}

#[derive(Debug, Clone, FromRow, PartialEq, Eq)]
pub struct TokenRecord {
    pub id: Uuid,
    pub user_id: Uuid,
    pub token_hash: Vec<u8>,
    pub hmac_key_id: String,
    pub label: String,
    pub scope: String,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub user_disabled_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum TokenAuthError {
    #[error("missing bearer token")]
    Missing,
    #[error("malformed bearer token")]
    Malformed,
    #[error("unknown bearer token")]
    Unknown,
    #[error("bearer token is revoked")]
    Revoked,
    #[error("bearer token is expired")]
    Expired,
    #[error("bearer token user is disabled")]
    UserDisabled,
}

#[derive(Debug, Clone)]
pub struct TokenRepository {
    pool: PgPool,
}

impl TokenRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn insert_generated_token(&self, token: &TokenInsert) -> anyhow::Result<()> {
        let mut conn = db_pool::acquire_runtime(&self.pool).await?;

        sqlx::query(
            r#"
            INSERT INTO joplin_mcp.mcp_tokens (
                id,
                user_id,
                token_hash,
                hmac_key_id,
                label,
                scope,
                created_from_ip,
                expires_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, CAST($7 AS inet), $8)
            "#,
        )
        .bind(token.id)
        .bind(token.user_id)
        .bind(token.token_hash.bytes.as_slice())
        .bind(&token.token_hash.hmac_key_id)
        .bind(&token.label)
        .bind(&token.scope)
        .bind(token.created_from_ip.as_deref())
        .bind(token.expires_at)
        .execute(&mut *conn)
        .await?;

        Ok(())
    }

    pub async fn create_token(
        &self,
        user_id: Uuid,
        key: &HmacKey,
        label: impl Into<String>,
        scope: impl Into<String>,
        expires_at: Option<DateTime<Utc>>,
        created_from_ip: Option<String>,
    ) -> anyhow::Result<GeneratedToken> {
        let generated =
            generate_token_for_insert(user_id, key, label, scope, expires_at, created_from_ip);
        self.insert_generated_token(&generated.insert).await?;
        Ok(generated)
    }

    pub async fn find_by_hash(&self, hash: &TokenHash) -> anyhow::Result<Option<TokenRecord>> {
        let mut conn = db_pool::acquire_runtime(&self.pool).await?;

        let token = sqlx::query_as::<_, TokenRecord>(
            r#"
            SELECT
                token.id,
                token.user_id,
                token.token_hash,
                token.hmac_key_id,
                token.label,
                token.scope,
                token.created_at,
                token.last_seen_at,
                token.revoked_at,
                token.expires_at,
                users.disabled_at AS user_disabled_at
            FROM joplin_mcp.mcp_tokens token
            JOIN joplin_mcp.mcp_users users ON users.id = token.user_id
            WHERE token.token_hash = $1 AND token.hmac_key_id = $2
            "#,
        )
        .bind(hash.bytes.as_slice())
        .bind(&hash.hmac_key_id)
        .fetch_optional(&mut *conn)
        .await?;

        Ok(token)
    }

    pub async fn authenticate_bearer(
        &self,
        authorization: Option<&str>,
        keys: &[HmacKey],
        now: DateTime<Utc>,
    ) -> Result<TokenRecord, TokenAuthError> {
        let raw_token = parse_bearer_token(authorization)?;

        for key in keys {
            let hash = token_hash(&key.id, &key.bytes, raw_token)
                .map_err(|_| TokenAuthError::Malformed)?;
            let token = self
                .find_by_hash(&hash)
                .await
                .map_err(|_| TokenAuthError::Unknown)?;
            if let Some(token) = token {
                return classify_token_record(Some(token), now);
            }
        }

        Err(TokenAuthError::Unknown)
    }

    pub async fn revoke_token(
        &self,
        token_id: Uuid,
        revoked_by: Option<Uuid>,
        reason: Option<&str>,
        revoked_from_ip: Option<&str>,
    ) -> anyhow::Result<bool> {
        let mut conn = db_pool::acquire_runtime(&self.pool).await?;

        let result = sqlx::query(
            r#"
            UPDATE joplin_mcp.mcp_tokens
            SET
                revoked_at = COALESCE(revoked_at, now()),
                revoked_by = COALESCE(revoked_by, $2),
                revoke_reason = COALESCE(revoke_reason, $3),
                revoked_from_ip = COALESCE(revoked_from_ip, CAST($4 AS inet))
            WHERE id = $1 AND revoked_at IS NULL
            "#,
        )
        .bind(token_id)
        .bind(revoked_by)
        .bind(reason)
        .bind(revoked_from_ip)
        .execute(&mut *conn)
        .await?;

        Ok(result.rows_affected() == 1)
    }

    pub async fn touch_last_seen_if_stale(
        &self,
        token_id: Uuid,
        now: DateTime<Utc>,
        min_interval: Duration,
    ) -> anyhow::Result<bool> {
        let stale_before = now - min_interval;
        let mut conn = db_pool::acquire_runtime(&self.pool).await?;

        let result = sqlx::query(
            r#"
            UPDATE joplin_mcp.mcp_tokens
            SET last_seen_at = $2
            WHERE id = $1
              AND revoked_at IS NULL
              AND (last_seen_at IS NULL OR last_seen_at < $3)
            "#,
        )
        .bind(token_id)
        .bind(now)
        .bind(stale_before)
        .execute(&mut *conn)
        .await?;

        Ok(result.rows_affected() == 1)
    }
}

pub fn generate_raw_token() -> String {
    let mut bytes = [0_u8; TOKEN_RANDOM_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    format!("{RAW_TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
}

pub fn validate_raw_token(token: &str) -> bool {
    token.strip_prefix(RAW_TOKEN_PREFIX).is_some_and(|body| {
        URL_SAFE_NO_PAD
            .decode(body)
            .is_ok_and(|bytes| bytes.len() == TOKEN_RANDOM_BYTES)
    })
}

pub fn parse_bearer_token(authorization: Option<&str>) -> Result<&str, TokenAuthError> {
    let Some(authorization) = authorization else {
        return Err(TokenAuthError::Missing);
    };
    let Some(token) = authorization.strip_prefix("Bearer ") else {
        return Err(TokenAuthError::Malformed);
    };

    if validate_raw_token(token) {
        Ok(token)
    } else {
        Err(TokenAuthError::Malformed)
    }
}

pub fn validate_hmac_key(key: &[u8]) -> anyhow::Result<()> {
    if key.len() != HMAC_KEY_BYTES {
        anyhow::bail!("token HMAC key must be exactly 32 raw bytes");
    }
    Ok(())
}

pub fn token_hash(
    hmac_key_id: impl Into<String>,
    hmac_key: &[u8],
    raw_token: &str,
) -> anyhow::Result<TokenHash> {
    validate_hmac_key(hmac_key)?;
    let mut mac = HmacSha256::new_from_slice(hmac_key).expect("HMAC accepts any key length");
    mac.update(raw_token.as_bytes());
    let bytes = mac.finalize().into_bytes().into();
    Ok(TokenHash {
        hmac_key_id: hmac_key_id.into(),
        bytes,
    })
}

pub fn generate_token_for_insert(
    user_id: Uuid,
    key: &HmacKey,
    label: impl Into<String>,
    scope: impl Into<String>,
    expires_at: Option<DateTime<Utc>>,
    created_from_ip: Option<String>,
) -> GeneratedToken {
    let raw_token = generate_raw_token();
    let token_hash = token_hash(&key.id, &key.bytes, &raw_token).expect("validated HMAC key");

    GeneratedToken {
        raw_token,
        insert: TokenInsert {
            id: Uuid::new_v4(),
            user_id,
            token_hash,
            label: label.into(),
            scope: scope.into(),
            created_from_ip,
            expires_at,
        },
    }
}

pub fn classify_token_record(
    token: Option<TokenRecord>,
    now: DateTime<Utc>,
) -> Result<TokenRecord, TokenAuthError> {
    let Some(token) = token else {
        return Err(TokenAuthError::Unknown);
    };

    if token.revoked_at.is_some() {
        return Err(TokenAuthError::Revoked);
    }
    if token.expires_at.is_some_and(|expires_at| expires_at <= now) {
        return Err(TokenAuthError::Expired);
    }
    if token.user_disabled_at.is_some() {
        return Err(TokenAuthError::UserDisabled);
    }

    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> HmacKey {
        HmacKey::new("2026-05", &[7_u8; 32]).expect("valid key")
    }

    fn token_record(
        revoked_at: Option<DateTime<Utc>>,
        expires_at: Option<DateTime<Utc>>,
        user_disabled_at: Option<DateTime<Utc>>,
    ) -> TokenRecord {
        let now = Utc::now();
        TokenRecord {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            token_hash: vec![1_u8; 32],
            hmac_key_id: "2026-05".to_string(),
            label: "test".to_string(),
            scope: "read".to_string(),
            created_at: now,
            last_seen_at: None,
            revoked_at,
            expires_at,
            user_disabled_at,
        }
    }

    #[test]
    fn generated_token_has_mcp_prefix_and_random_body() {
        let token = generate_raw_token();
        assert!(token.starts_with(RAW_TOKEN_PREFIX));
        assert!(validate_raw_token(&token));
    }

    #[test]
    fn hashes_token_without_storing_raw_token() {
        let key = [7_u8; 32];
        let token = generate_raw_token();
        let hash = token_hash("2026-05", &key, &token).expect("hash token");
        assert_eq!(hash.hmac_key_id, "2026-05");
        assert_ne!(hash.bytes.as_slice(), token.as_bytes());
        assert_eq!(hash.bytes.len(), 32);
    }

    #[test]
    fn rejects_hmac_keys_that_are_not_32_raw_bytes() {
        validate_hmac_key(&[0_u8; 32]).expect("valid key");
        validate_hmac_key(&[0_u8; 31]).expect_err("short key rejected");
        validate_hmac_key(&[0_u8; 33]).expect_err("long key rejected");
    }

    #[test]
    fn rejects_malformed_raw_tokens() {
        assert!(!validate_raw_token("not_mcp_token"));
        assert!(!validate_raw_token("mcp_not-base64url!"));
        assert!(!validate_raw_token(&format!(
            "{}{}",
            RAW_TOKEN_PREFIX,
            URL_SAFE_NO_PAD.encode([1_u8; 31])
        )));
        assert!(!validate_raw_token(&format!(
            "{}{}",
            RAW_TOKEN_PREFIX,
            URL_SAFE_NO_PAD.encode([1_u8; 33])
        )));
    }

    #[test]
    fn parses_only_strict_bearer_tokens() {
        let raw = generate_raw_token();
        let header = format!("Bearer {raw}");

        assert_eq!(parse_bearer_token(Some(&header)), Ok(raw.as_str()));
        assert_eq!(parse_bearer_token(None), Err(TokenAuthError::Missing));
        assert_eq!(parse_bearer_token(Some("")), Err(TokenAuthError::Malformed));
        assert_eq!(
            parse_bearer_token(Some(&raw)),
            Err(TokenAuthError::Malformed)
        );
        assert_eq!(
            parse_bearer_token(Some("Bearer mcp_short")),
            Err(TokenAuthError::Malformed)
        );
    }

    #[test]
    fn classifies_unknown_revoked_and_expired_tokens() {
        let now = Utc::now();

        assert_eq!(
            classify_token_record(None, now).expect_err("missing record"),
            TokenAuthError::Unknown
        );
        assert_eq!(
            classify_token_record(Some(token_record(Some(now), None, None)), now)
                .expect_err("revoked"),
            TokenAuthError::Revoked
        );
        assert_eq!(
            classify_token_record(Some(token_record(None, Some(now), None)), now)
                .expect_err("expired"),
            TokenAuthError::Expired
        );
        assert_eq!(
            classify_token_record(Some(token_record(None, None, Some(now))), now)
                .expect_err("disabled"),
            TokenAuthError::UserDisabled
        );
    }

    #[test]
    fn accepts_active_unexpired_token_records() {
        let now = Utc::now();
        let record = token_record(None, Some(now + Duration::days(1)), None);
        assert_eq!(
            classify_token_record(Some(record.clone()), now).expect("active token"),
            record
        );
    }

    #[test]
    fn generated_insert_contains_hash_not_raw_token() {
        let generated = generate_token_for_insert(
            Uuid::new_v4(),
            &key(),
            "workstation",
            "read",
            Some(Utc::now() + Duration::days(90)),
            None,
        );

        assert!(validate_raw_token(&generated.raw_token));
        assert_eq!(generated.insert.token_hash.bytes.len(), 32);
        assert_ne!(
            generated.insert.token_hash.bytes.as_slice(),
            generated.raw_token.as_bytes()
        );
        assert_eq!(generated.insert.label, "workstation");
    }
}
