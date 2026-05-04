use anyhow::{Context, bail};
use ipnet::IpNet;
use serde::Deserialize;
use std::{fs, net::SocketAddr, path::Path};
use url::Url;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub joplin: JoplinConfig,
    pub postgres: PostgresConfig,
    pub tokens: TokenConfig,
    pub bootstrap_rate_limit: BootstrapRateLimitConfig,
    pub mcp: McpConfig,
    pub index: IndexConfig,
    pub logging: LoggingConfig,
}

impl Config {
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let config = match path {
            Some(path) => {
                let text = fs::read_to_string(path)
                    .with_context(|| format!("read config file {}", path.display()))?;
                toml::from_str(&text).context("parse config toml")?
            }
            None => Self::default(),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.server.listen.ip().is_unspecified()
            && self.server.tls_mode == TlsMode::Disabled
            && !self.server.allow_insecure_localhost
        {
            bail!("TLS is required for non-localhost listeners");
        }
        if self.tokens.active_hmac_key_id.is_empty() {
            bail!("tokens.active_hmac_key_id is required");
        }
        if !self
            .tokens
            .hmac_keys
            .iter()
            .any(|key| key.id == self.tokens.active_hmac_key_id)
        {
            bail!("tokens.active_hmac_key_id is missing from tokens.hmac_keys");
        }
        if self.mcp.max_response_bytes == 0 {
            bail!("mcp.max_response_bytes must be greater than zero");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub public_base_url: Url,
    pub lan_cidrs: Vec<IpNet>,
    pub tls_mode: TlsMode,
    pub allowed_origins: Vec<String>,
    pub allow_insecure_localhost: bool,
    pub trusted_proxies: Vec<IpNet>,
    pub forwarded_header: String,
    pub request_timeout_seconds: u64,
    pub slow_request_log_threshold_ms: u64,
    pub shutdown_grace_seconds: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8081".parse().expect("valid listen address"),
            public_base_url: Url::parse("http://127.0.0.1:8081").expect("valid url"),
            lan_cidrs: Vec::new(),
            tls_mode: TlsMode::Disabled,
            allowed_origins: vec!["http://127.0.0.1:8081".to_string()],
            allow_insecure_localhost: true,
            trusted_proxies: Vec::new(),
            forwarded_header: "x-forwarded-for".to_string(),
            request_timeout_seconds: 30,
            slow_request_log_threshold_ms: 2000,
            shutdown_grace_seconds: 30,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    Required,
    Disabled,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct JoplinConfig {
    pub base_url: Url,
}

impl Default for JoplinConfig {
    fn default() -> Self {
        Self {
            base_url: Url::parse("http://127.0.0.1:58080").expect("valid url"),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PostgresConfig {
    pub joplin_dsn_file: Option<String>,
    pub mcp_dsn_file: Option<String>,
    pub runtime_max_connections: u32,
    pub indexer_max_connections: u32,
    pub acquire_timeout_seconds: u64,
    pub statement_timeout_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TokenConfig {
    pub hmac_keys: Vec<HmacKeyConfig>,
    pub active_hmac_key_id: String,
    pub default_scope: String,
    pub expires_after_days: u64,
    pub allow_non_expiring_tokens: bool,
}

impl Default for TokenConfig {
    fn default() -> Self {
        Self {
            hmac_keys: vec![HmacKeyConfig {
                id: "local-dev".to_string(),
                file: None,
            }],
            active_hmac_key_id: "local-dev".to_string(),
            default_scope: "read".to_string(),
            expires_after_days: 90,
            allow_non_expiring_tokens: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct HmacKeyConfig {
    pub id: String,
    pub file: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BootstrapRateLimitConfig {
    pub per_ip_per_minute: u32,
    pub per_email_per_hour: u32,
}

impl Default for BootstrapRateLimitConfig {
    fn default() -> Self {
        Self {
            per_ip_per_minute: 5,
            per_email_per_hour: 20,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    pub protocol_version: String,
    pub max_response_bytes: usize,
    pub default_body_truncate_chars: usize,
    pub tool_timeout_seconds: u64,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            protocol_version: crate::contracts::MCP_PROTOCOL_VERSION.to_string(),
            max_response_bytes: 65_536,
            default_body_truncate_chars: 8000,
            tool_timeout_seconds: 20,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct IndexConfig {
    pub source: String,
    pub refresh_interval_seconds: u64,
    pub full_rebuild_interval_hours: u64,
    pub max_parallel_users: u32,
    pub text_search_config: String,
    pub incremental_lookback_max_seconds: u64,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            source: "joplin_db".to_string(),
            refresh_interval_seconds: 60,
            full_rebuild_interval_hours: 24,
            max_parallel_users: 4,
            text_search_config: "simple".to_string(),
            incremental_lookback_max_seconds: 86_400,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    pub service_name: String,
    pub otlp_logs_endpoint: Url,
    pub otlp_protocol: String,
    pub email_display: String,
    pub deployment_environment: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            service_name: "joplin-mcpd".to_string(),
            otlp_logs_endpoint: Url::parse(
                "http://victorialogs.lan:9428/insert/opentelemetry/v1/logs",
            )
            .expect("valid url"),
            otlp_protocol: "http/protobuf".to_string(),
            email_display: "redacted".to_string(),
            deployment_environment: "local".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_validates_for_localhost_test_mode() {
        Config::default()
            .validate()
            .expect("default config validates");
    }

    #[test]
    fn rejects_non_localhost_without_tls() {
        let mut config = Config::default();
        config.server.listen = "0.0.0.0:8081".parse().expect("valid address");
        config.server.allow_insecure_localhost = false;
        let error = config.validate().expect_err("config is rejected");
        assert!(error.to_string().contains("TLS is required"));
    }
}
