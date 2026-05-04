use crate::{api::ClientApi, config::ClientConfig};
use anyhow::{Context, bail};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

pub async fn serve_stdio_proxy(config: ClientConfig) -> anyhow::Result<()> {
    let token = config.read_token()?;
    let api = ClientApi::new(&config)?;
    proxy_stdio(tokio::io::stdin(), tokio::io::stdout(), api, token).await
}

pub async fn proxy_stdio<R, W>(
    input: R,
    mut output: W,
    api: ClientApi,
    token: String,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut input = BufReader::new(input);
    while let Some(body) = read_frame(&mut input).await? {
        let request =
            serde_json::from_slice::<Value>(&body).context("parse stdio JSON-RPC body")?;
        if let Some(response) = api.mcp(&token, request).await? {
            write_frame(&mut output, &serde_json::to_vec(&response)?).await?;
        }
    }
    output.flush().await?;
    Ok(())
}

async fn read_frame<R>(input: &mut BufReader<R>) -> anyhow::Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let read = input.read_line(&mut line).await?;
        if read == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .context("parse Content-Length")?,
            );
        }
    }

    let Some(content_length) = content_length else {
        bail!("stdio frame missing Content-Length");
    };
    let mut body = vec![0_u8; content_length];
    tokio::io::AsyncReadExt::read_exact(input, &mut body).await?;
    Ok(Some(body))
}

async fn write_frame<W>(output: &mut W, body: &[u8]) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    output
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    output.write_all(body).await?;
    output.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stdio_frame_round_trip_preserves_json_body() {
        let input = b"Content-Length: 36\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}";
        let mut reader = BufReader::new(&input[..]);
        let body = read_frame(&mut reader)
            .await
            .expect("read frame")
            .expect("frame exists");
        assert_eq!(
            serde_json::from_slice::<Value>(&body).expect("json"),
            serde_json::json!({"jsonrpc":"2.0","id":1,"result":{}})
        );

        let mut output = Vec::new();
        write_frame(&mut output, &body).await.expect("write frame");
        assert!(
            String::from_utf8(output)
                .expect("utf8")
                .starts_with("Content-Length: 36\r\n\r\n")
        );
    }
}
