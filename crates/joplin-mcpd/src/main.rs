use anyhow::Context;
use clap::Parser;
use joplin_mcpd::{config::Config, http::serve, logging::init_logging};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Args {
    #[arg(long, env = "JP_MCPD_CONFIG")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = Config::load(args.config.as_deref()).context("load config")?;
    let log_guard = init_logging(&config.logging).context("initialize logging")?;

    serve(config).await?;
    log_guard.flush();
    Ok(())
}
