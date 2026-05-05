use anyhow::{Context, bail};
use clap::Args;
use std::{
    env, fs,
    path::{Path, PathBuf},
};
use url::Url;

#[derive(Debug, Clone, Args)]
pub struct ClientConfig {
    #[arg(long, env = "JP_MCP_SERVER_URL")]
    pub server_url: Option<Url>,
    #[arg(long)]
    pub url_file: Option<PathBuf>,
    #[arg(long)]
    pub token_file: Option<PathBuf>,
    #[arg(long)]
    pub test_id: Option<String>,
    #[arg(long, env = "JP_MCP_HTTP_TIMEOUT_SECONDS", default_value_t = 3)]
    pub http_timeout_seconds: u64,
    #[command(flatten)]
    pub logging: ClientLoggingConfig,
}

#[derive(Debug, Clone, Args)]
pub struct ClientLoggingConfig {
    #[arg(
        long,
        env = "JP_MCP_OTLP_LOGS_ENDPOINT",
        default_value = "http://victorialogs.lan:9428/insert/opentelemetry/v1/logs"
    )]
    pub otlp_logs_endpoint: Url,
    #[arg(
        long,
        env = "JP_MCP_OTLP_LOGS_PROTOCOL",
        default_value = "http/protobuf"
    )]
    pub otlp_protocol: String,
    #[arg(long, env = "JP_MCP_DEPLOYMENT_ENVIRONMENT", default_value = "local")]
    pub deployment_environment: String,
}

impl Default for ClientLoggingConfig {
    fn default() -> Self {
        Self {
            otlp_logs_endpoint: Url::parse(
                "http://victorialogs.lan:9428/insert/opentelemetry/v1/logs",
            )
            .expect("valid url"),
            otlp_protocol: "http/protobuf".to_string(),
            deployment_environment: "local".to_string(),
        }
    }
}

impl ClientConfig {
    pub fn token_file(&self) -> anyhow::Result<PathBuf> {
        if let Some(path) = &self.token_file {
            return Ok(path.clone());
        }
        Ok(default_token_path(&runtime_dir()?))
    }

    pub fn read_token(&self) -> anyhow::Result<String> {
        crate::token_file::read_token(&self.token_file()?)
    }

    pub fn server_url(&self) -> anyhow::Result<Url> {
        if let Some(url) = &self.server_url {
            return Ok(url.clone());
        }
        if let Some(path) = &self.url_file {
            let value = fs::read_to_string(path)
                .with_context(|| format!("read server URL file {}", path.display()))?;
            return Url::parse(value.trim()).context("parse server URL file");
        }
        bail!("server URL is missing; set --server-url, --url-file, or JP_MCP_SERVER_URL");
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.http_timeout_seconds == 0 {
            bail!("HTTP timeout must be greater than zero");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Args)]
pub struct BootstrapOptions {
    #[command(flatten)]
    pub client: ClientConfig,
    #[arg(long)]
    pub email: String,
    #[arg(long)]
    pub password_stdin: bool,
    #[arg(long)]
    pub password_command: Option<String>,
    #[arg(long)]
    pub prompt_password: bool,
    #[arg(long, default_value = "joplin-mcp-client")]
    pub client_label: String,
    #[arg(long)]
    pub print_token: bool,
}

pub fn default_token_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("joplin-mcp-client/token")
}

pub fn runtime_dir() -> anyhow::Result<PathBuf> {
    if let Some(value) = env::var_os("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(value));
    }
    let fallback = Path::new("/run/joplin-mcp-client");
    if fallback.is_dir() {
        return Ok(fallback.to_path_buf());
    }
    bail!("XDG_RUNTIME_DIR is not set and /run/joplin-mcp-client is unavailable");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_token_path_uses_xdg_runtime_shape() {
        assert_eq!(
            default_token_path(Path::new("/run/user/1000")),
            PathBuf::from("/run/user/1000/joplin-mcp-client/token")
        );
    }

    #[test]
    fn explicit_token_file_overrides_default() {
        let config = ClientConfig {
            server_url: None,
            url_file: None,
            token_file: Some(PathBuf::from("/tmp/custom-token")),
            test_id: None,
            http_timeout_seconds: 5,
            logging: ClientLoggingConfig::default(),
        };
        assert_eq!(
            config.token_file().expect("token path"),
            PathBuf::from("/tmp/custom-token")
        );
    }

    #[test]
    fn url_file_supplies_server_url() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("server-url");
        fs::write(&path, "https://joplin-mcp.lan\n").expect("write url");
        let config = ClientConfig {
            server_url: None,
            url_file: Some(path),
            token_file: None,
            test_id: None,
            http_timeout_seconds: 5,
            logging: ClientLoggingConfig::default(),
        };

        assert_eq!(
            config.server_url().expect("server url").as_str(),
            "https://joplin-mcp.lan/"
        );
    }

    #[test]
    fn server_url_missing_fails_before_network() {
        let config = ClientConfig {
            server_url: None,
            url_file: None,
            token_file: Some(PathBuf::from("/tmp/token")),
            test_id: None,
            http_timeout_seconds: 5,
            logging: ClientLoggingConfig::default(),
        };

        let err = config.server_url().expect_err("server URL is required");
        assert!(err.to_string().contains("server URL is missing"));
    }

    #[test]
    fn rejects_zero_http_timeout() {
        let config = ClientConfig {
            server_url: Some(Url::parse("https://joplin-mcp.lan").expect("url")),
            url_file: None,
            token_file: Some(PathBuf::from("/tmp/token")),
            test_id: None,
            http_timeout_seconds: 0,
            logging: ClientLoggingConfig::default(),
        };

        let err = config.validate().expect_err("zero timeout is rejected");
        assert!(err.to_string().contains("HTTP timeout"));
    }
}
