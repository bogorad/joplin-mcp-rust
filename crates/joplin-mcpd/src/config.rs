use anyhow::{Context, bail};
use ipnet::IpNet;
use serde::Deserialize;
use std::{fs, net::SocketAddr, path::Path};
use url::Url;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
        let config = Self::read(path)?;
        config.validate()?;
        Ok(config)
    }

    pub fn read(path: Option<&Path>) -> anyhow::Result<Self> {
        let config = match path {
            Some(path) => {
                let text = fs::read_to_string(path)
                    .with_context(|| format!("read config file {}", path.display()))?;
                toml::from_str(&text).context("parse config toml")?
            }
            None => Self::default(),
        };
        Ok(config)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.mcp.protocol_version != crate::contracts::MCP_PROTOCOL_VERSION {
            bail!("mcp.protocol_version must be 2025-06-18");
        }
        crate::security::validate_transport_policy(&self.server)?;
        if self.server.request_timeout_seconds == 0
            || self.server.slow_request_log_threshold_ms == 0
            || self.server.shutdown_grace_seconds == 0
        {
            bail!("server timeout and shutdown values must be greater than zero");
        }
        self.postgres.validate()?;
        if self.tokens.active_hmac_key_id.is_empty() {
            bail!("tokens.active_hmac_key_id is required");
        }
        if self.tokens.hmac_keys.is_empty() {
            bail!("tokens.hmac_keys must not be empty");
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
        if self.mcp.default_body_truncate_chars == 0 {
            bail!("mcp.default_body_truncate_chars must be greater than zero");
        }
        if self.mcp.tool_timeout_seconds == 0 {
            bail!("mcp.tool_timeout_seconds must be greater than zero");
        }
        if self.bootstrap_rate_limit.per_ip_per_minute == 0 {
            bail!("bootstrap_rate_limit.per_ip_per_minute must be greater than zero");
        }
        if self.bootstrap_rate_limit.per_email_per_hour == 0 {
            bail!("bootstrap_rate_limit.per_email_per_hour must be greater than zero");
        }
        if self.index.refresh_interval_seconds == 0
            || self.index.full_rebuild_interval_hours == 0
            || self.index.max_parallel_users == 0
            || self.index.incremental_lookback_max_seconds == 0
        {
            bail!("index timing and concurrency values must be greater than zero");
        }
        if self.logging.otlp_protocol != "http/protobuf" {
            bail!("logging.otlp_protocol must be http/protobuf");
        }
        crate::logging::validate_victorialogs_endpoint(&self.logging)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
            request_timeout_seconds: 5,
            slow_request_log_threshold_ms: 500,
            shutdown_grace_seconds: 5,
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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
pub struct PostgresConfig {
    pub joplin_dsn_file: Option<String>,
    pub mcp_dsn_file: Option<String>,
    pub runtime_max_connections: u32,
    pub indexer_max_connections: u32,
    pub acquire_timeout_seconds: u64,
    pub statement_timeout_seconds: u64,
}

impl PostgresConfig {
    fn validate(&self) -> anyhow::Result<()> {
        if self.runtime_max_connections == 0 || self.indexer_max_connections == 0 {
            bail!("postgres pool sizes must be greater than zero");
        }
        if self.acquire_timeout_seconds == 0 || self.statement_timeout_seconds == 0 {
            bail!("postgres timeouts must be greater than zero");
        }

        let joplin_dsn = read_dsn_identity(
            self.joplin_dsn_file
                .as_deref()
                .context("postgres.joplin_dsn_file is required")?,
        )
        .context("validate postgres.joplin_dsn_file")?;
        let mcp_dsn = read_dsn_identity(
            self.mcp_dsn_file
                .as_deref()
                .context("postgres.mcp_dsn_file is required")?,
        )
        .context("validate postgres.mcp_dsn_file")?;

        if joplin_dsn.database == mcp_dsn.database {
            bail!("postgres DSN files must target different databases");
        }
        if joplin_dsn.username == mcp_dsn.username {
            bail!("postgres DSN files must use different users");
        }

        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
struct DsnIdentity {
    username: String,
    database: String,
}

fn read_dsn_identity(path: &str) -> anyhow::Result<DsnIdentity> {
    let dsn = fs::read_to_string(path).with_context(|| "read postgres DSN credential file")?;
    parse_dsn_identity(dsn.trim()).with_context(|| "parse postgres DSN credential file")
}

fn parse_dsn_identity(dsn: &str) -> anyhow::Result<DsnIdentity> {
    let url = Url::parse(dsn).context("parse postgres DSN")?;
    let username = url.username();
    if username.is_empty() {
        bail!("postgres DSN username is required");
    }
    let database = url.path().trim_start_matches('/');
    if database.is_empty() {
        bail!("postgres DSN database is required");
    }
    Ok(DsnIdentity {
        username: username.to_string(),
        database: database.to_string(),
    })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct HmacKeyConfig {
    pub id: String,
    pub file: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
            tool_timeout_seconds: 5,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
    use std::io::Write;

    fn valid_config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().expect("tempdir");
        let joplin_dsn_file = dir.path().join("postgres-joplin-dsn");
        let mcp_dsn_file = dir.path().join("postgres-mcp-dsn");
        write_file(
            &joplin_dsn_file,
            "postgres://joplin_user@127.0.0.1/joplin_db",
        );
        write_file(&mcp_dsn_file, "postgres://mcp_user@127.0.0.1/mcp_db");

        let mut config = Config::default();
        config.postgres.joplin_dsn_file = Some(joplin_dsn_file.display().to_string());
        config.postgres.mcp_dsn_file = Some(mcp_dsn_file.display().to_string());
        config.postgres.runtime_max_connections = 12;
        config.postgres.indexer_max_connections = 4;
        config.postgres.acquire_timeout_seconds = 5;
        config.postgres.statement_timeout_seconds = 15;
        (dir, config)
    }

    fn write_file(path: &Path, value: &str) {
        let mut file = fs::File::create(path).expect("create file");
        file.write_all(value.as_bytes()).expect("write file");
    }

    #[test]
    fn valid_config_validates_for_localhost_test_mode() {
        let (_dir, config) = valid_config();
        config.validate().expect("valid config validates");
    }

    #[test]
    fn rejects_non_localhost_without_tls() {
        let (_dir, mut config) = valid_config();
        config.server.listen = "0.0.0.0:8081".parse().expect("valid address");
        config.server.allow_insecure_localhost = false;
        let error = config.validate().expect_err("config is rejected");
        assert!(error.to_string().contains("TLS is required"));
    }

    #[test]
    fn rejects_wrong_mcp_protocol_version() {
        let (_dir, mut config) = valid_config();
        config.mcp.protocol_version = "2024-11-05".to_string();
        let error = config.validate().expect_err("protocol version rejected");
        assert!(error.to_string().contains("mcp.protocol_version"));
    }

    #[test]
    fn rejects_zero_server_lifecycle_values() {
        let (_dir, mut config) = valid_config();
        config.server.request_timeout_seconds = 0;
        let error = config.validate().expect_err("zero server timeout rejected");
        assert!(error.to_string().contains("server timeout"));
    }

    #[test]
    fn rejects_missing_postgres_dsn_files() {
        let mut config = Config::default();
        config.postgres.runtime_max_connections = 12;
        config.postgres.indexer_max_connections = 4;
        config.postgres.acquire_timeout_seconds = 5;
        config.postgres.statement_timeout_seconds = 15;
        let error = config.validate().expect_err("DSN files are required");
        assert!(error.to_string().contains("postgres.joplin_dsn_file"));
    }

    #[test]
    fn rejects_same_postgres_database_or_user() {
        let (dir, mut config) = valid_config();
        let mcp_dsn_file = dir.path().join("postgres-mcp-dsn");
        write_file(&mcp_dsn_file, "postgres://mcp_user@127.0.0.1/joplin_db");
        let error = config
            .validate()
            .expect_err("same database must be rejected");
        assert!(error.to_string().contains("different databases"));

        write_file(&mcp_dsn_file, "postgres://joplin_user@127.0.0.1/mcp_db");
        config.postgres.mcp_dsn_file = Some(mcp_dsn_file.display().to_string());
        let error = config.validate().expect_err("same user must be rejected");
        assert!(error.to_string().contains("different users"));
    }

    #[test]
    fn rejects_static_joplin_password_in_config_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let joplin_dsn_file = dir.path().join("postgres-joplin-dsn");
        let mcp_dsn_file = dir.path().join("postgres-mcp-dsn");
        write_file(
            &joplin_dsn_file,
            "postgres://joplin_user@127.0.0.1/joplin_db",
        );
        write_file(&mcp_dsn_file, "postgres://mcp_user@127.0.0.1/mcp_db");
        write_file(
            &config_path,
            &format!(
                r#"
[joplin]
password = "not-allowed"

[postgres]
joplin_dsn_file = "{}"
mcp_dsn_file = "{}"
runtime_max_connections = 12
indexer_max_connections = 4
acquire_timeout_seconds = 5
statement_timeout_seconds = 15
"#,
                joplin_dsn_file.display(),
                mcp_dsn_file.display()
            ),
        );

        let error = Config::load(Some(&config_path)).expect_err("unknown password rejected");
        assert!(format!("{error:?}").contains("unknown field"));
    }

    #[test]
    fn parses_dsn_identity_without_exposing_secret_values() {
        let identity = parse_dsn_identity("postgres://joplin_user:secret@db.lan/joplin_db")
            .expect("valid DSN");
        assert_eq!(
            identity,
            DsnIdentity {
                username: "joplin_user".to_string(),
                database: "joplin_db".to_string()
            }
        );
    }
}
