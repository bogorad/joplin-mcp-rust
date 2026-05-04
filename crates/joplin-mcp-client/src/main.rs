use anyhow::Context;
use clap::{Parser, Subcommand};
use joplin_mcp_client::{
    api::ClientApi,
    bootstrap::bootstrap,
    config::{BootstrapOptions, ClientConfig},
    logging::init_logging,
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
        Command::Bootstrap(options) => {
            let log_guard = init_logging(&options.client.logging).context("initialize logging")?;
            let result = bootstrap(options).await;
            log_guard.flush();
            result
        }
        Command::Serve(config) => {
            let log_guard = init_logging(&config.logging).context("initialize logging")?;
            let result = serve_stdio_proxy(config).await;
            log_guard.flush();
            result
        }
        Command::Status(config) => {
            let log_guard = init_logging(&config.logging).context("initialize logging")?;
            let token = config.read_token().context("read token")?;
            let status = ClientApi::new(&config)?.status(&token).await?;
            println!("{}", serde_json::to_string_pretty(&status)?);
            tracing::info!(
                operation = "status",
                outcome = "completed",
                "client token status checked"
            );
            log_guard.flush();
            Ok(())
        }
        Command::Logout(config) => {
            let log_guard = init_logging(&config.logging).context("initialize logging")?;
            let token = config.read_token().context("read token")?;
            ClientApi::new(&config)?.revoke_token(&token).await?;
            remove_token(&config.token_file()?)?;
            tracing::info!(
                operation = "logout",
                outcome = "completed",
                "client token revoked and removed"
            );
            log_guard.flush();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn help_lists_required_commands_and_options() {
        let mut command = Cli::command();
        let mut help = command.render_long_help().to_string();
        for subcommand in ["bootstrap", "serve", "status", "logout"] {
            let mut command = Cli::command();
            let subcommand = command
                .find_subcommand_mut(subcommand)
                .expect("subcommand exists");
            help.push_str(&subcommand.render_long_help().to_string());
        }

        for required in [
            "bootstrap",
            "serve",
            "status",
            "logout",
            "--server-url",
            "--email",
            "--password-stdin",
            "--password-command",
            "--prompt-password",
            "--token-file",
            "--url-file",
            "--client-label",
            "--test-id",
            "--server-fingerprint",
            "--http-timeout-seconds",
        ] {
            assert!(help.contains(required), "missing help entry {required}");
        }
    }
}
