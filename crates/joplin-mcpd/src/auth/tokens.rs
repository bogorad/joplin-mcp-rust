use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

pub const RAW_TOKEN_PREFIX: &str = "mcp_";
pub const TOKEN_RANDOM_BYTES: usize = 32;
pub const HMAC_KEY_BYTES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenHash {
    pub hmac_key_id: String,
    pub bytes: [u8; 32],
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
            .is_ok_and(|bytes| bytes.len() >= 32)
    })
}

pub fn validate_hmac_key(key: &[u8]) -> anyhow::Result<()> {
    if key.len() != HMAC_KEY_BYTES {
        anyhow::bail!("token HMAC key must be exactly 32 raw bytes");
    }
    Ok(())
}

pub fn token_hash(hmac_key_id: impl Into<String>, hmac_key: &[u8], raw_token: &str) -> TokenHash {
    let mut mac = HmacSha256::new_from_slice(hmac_key).expect("HMAC accepts any key length");
    mac.update(raw_token.as_bytes());
    let bytes = mac.finalize().into_bytes().into();
    TokenHash {
        hmac_key_id: hmac_key_id.into(),
        bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_token_has_mcp_prefix_and_random_body() {
        let token = generate_raw_token();
        assert!(token.starts_with(RAW_TOKEN_PREFIX));
        assert!(validate_raw_token(&token));
    }

    #[test]
    fn hashes_token_without_storing_raw_token() {
        let key = [7_u8; 32];
        let token = "mcp_test-token";
        let hash = token_hash("2026-05", &key, token);
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
}
