use crate::{
    api::{BootstrapRequest, ClientApi},
    config::BootstrapOptions,
    token_file::{read_token, write_token},
};
use anyhow::{Context, bail};
use std::{io::Read, process::Command};

pub async fn bootstrap(options: BootstrapOptions) -> anyhow::Result<()> {
    let api = ClientApi::new(&options.client)?;
    let token_file = options.client.token_file()?;
    if token_file.exists() {
        let token = read_token(&token_file)?;
        if api.check_token(&token).await? {
            tracing::info!(
                test.id = options.client.test_id.as_deref().unwrap_or(""),
                client.label = %options.client_label,
                operation = "bootstrap_login",
                outcome = "reused_token",
                "valid existing token reused"
            );
            if options.print_token {
                println!("{token}");
            }
            return Ok(());
        }
    }

    let password = read_password(&options)?;
    tracing::info!(
        test.id = options.client.test_id.as_deref().unwrap_or(""),
        client.label = %options.client_label,
        operation = "bootstrap_login",
        outcome = "started",
        "bootstrap login started"
    );
    let body = match api
        .bootstrap_login(&BootstrapRequest {
            email: &options.email,
            password: &password,
            client_label: &options.client_label,
        })
        .await
    {
        Ok(body) => body,
        Err(error) => {
            tracing::warn!(
                test.id = options.client.test_id.as_deref().unwrap_or(""),
                client.label = %options.client_label,
                operation = "bootstrap_login",
                outcome = "failed",
                "bootstrap login failed"
            );
            return Err(error);
        }
    };
    write_token(&token_file, &body.token)?;
    tracing::info!(
        test.id = options.client.test_id.as_deref().unwrap_or(""),
        client.label = %options.client_label,
        operation = "bootstrap_login",
        outcome = "completed",
        "bootstrap login completed"
    );
    if options.print_token {
        println!("{}", body.token);
    }
    Ok(())
}

fn read_password(options: &BootstrapOptions) -> anyhow::Result<String> {
    if options.password_stdin {
        let mut password = String::new();
        std::io::stdin().read_to_string(&mut password)?;
        return Ok(password.trim_end_matches(['\r', '\n']).to_string());
    }
    if options.password_command.is_some() {
        let command = options.password_command.as_deref().expect("checked");
        let output = Command::new("sh")
            .arg("-c")
            .arg(command)
            .output()
            .context("run password command")?;
        if !output.status.success() {
            bail!("password command failed");
        }
        return String::from_utf8(output.stdout)
            .context("password command output is not UTF-8")
            .map(|password| password.trim_end_matches(['\r', '\n']).to_string());
    }
    if options.prompt_password {
        eprint!("Joplin password: ");
        let mut password = String::new();
        std::io::stdin().read_line(&mut password)?;
        return Ok(password.trim_end_matches(['\r', '\n']).to_string());
    }
    bail!("one password source is required");
}
