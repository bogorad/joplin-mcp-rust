use anyhow::{Context, bail};
use clap::Args;
use std::{
    env,
    path::{Path, PathBuf},
};
use url::Url;

#[derive(Debug, Clone, Args)]
pub struct ClientConfig {
    #[arg(long, env = "JP_MCP_SERVER_URL")]
    pub server_url: Option<Url>,
    #[arg(long)]
    pub token_file: Option<PathBuf>,
    #[arg(long)]
    pub test_id: Option<String>,
    #[arg(long)]
    pub server_fingerprint: Option<String>,
}

impl ClientConfig {
    pub fn token_file(&self) -> anyhow::Result<PathBuf> {
        if let Some(path) = &self.token_file {
            return Ok(path.clone());
        }
        Ok(runtime_dir()?.join("joplin-mcp-client/token"))
    }

    pub fn read_token(&self) -> anyhow::Result<String> {
        crate::token_file::read_token(&self.token_file()?)
    }

    pub fn server_url(&self) -> anyhow::Result<&Url> {
        self.server_url
            .as_ref()
            .context("server URL is missing; set --server-url or JP_MCP_SERVER_URL")
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
