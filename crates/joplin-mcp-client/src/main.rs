use anyhow::Context;
use clap::{Parser, Subcommand};
use joplin_mcp_client::{
    bootstrap::bootstrap,
    config::{BootstrapOptions, ClientConfig},
    proxy::serve_stdio_proxy,
    token_file::remove_token,
};

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Bootstrap(BootstrapOptions),
    Serve(ClientConfig),
    Status(ClientConfig),
    Logout(ClientConfig),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Bootstrap(options) => bootstrap(options).await,
        Command::Serve(config) => serve_stdio_proxy(config).await,
        Command::Status(config) => {
            let token = config.read_token().context("read token")?;
            println!("token_file=present token_len={}", token.len());
            Ok(())
        }
        Command::Logout(config) => {
            remove_token(&config.token_file()?)?;
            Ok(())
        }
    }
}
