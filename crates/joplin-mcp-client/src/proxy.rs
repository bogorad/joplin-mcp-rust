use crate::config::ClientConfig;

pub async fn serve_stdio_proxy(config: ClientConfig) -> anyhow::Result<()> {
    let _token = config.read_token()?;
    let _server_url = config.server_url()?;
    Ok(())
}
