use crate::{config::BootstrapOptions, token_file::write_token};
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use std::io::Read;

#[derive(Debug, Serialize)]
struct BootstrapRequest<'a> {
    email: &'a str,
    password: &'a str,
    client_label: &'a str,
}

#[derive(Debug, Deserialize)]
struct BootstrapResponse {
    token: String,
}

pub async fn bootstrap(options: BootstrapOptions) -> anyhow::Result<()> {
    let password = read_password(&options)?;
    let server_url = options.client.server_url()?.join("/api/bootstrap/login")?;
    let response = reqwest::Client::new()
        .post(server_url)
        .json(&BootstrapRequest {
            email: &options.email,
            password: &password,
            client_label: &options.client_label,
        })
        .send()
        .await
        .context("send bootstrap request")?;

    if !response.status().is_success() {
        bail!("bootstrap failed");
    }

    let body = response.json::<BootstrapResponse>().await?;
    write_token(&options.client.token_file()?, &body.token)?;
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
        bail!("--password-command is not implemented yet");
    }
    if options.prompt_password {
        bail!("--prompt-password is not implemented yet");
    }
    bail!("one password source is required");
}
