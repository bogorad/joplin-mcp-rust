use crate::config::ClientConfig;
use anyhow::{Context, bail};
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use url::Url;

pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, Serialize)]
pub struct BootstrapRequest<'a> {
    pub email: &'a str,
    pub password: &'a str,
    pub client_label: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct BootstrapResponse {
    pub token: String,
}

#[derive(Debug, Deserialize)]
struct TokenCheckResponse {
    valid: bool,
}

#[derive(Debug, Clone)]
pub struct ClientApi {
    client: reqwest::Client,
    server_url: Url,
    origin: String,
    test_id: Option<String>,
}

impl ClientApi {
    pub fn new(config: &ClientConfig) -> anyhow::Result<Self> {
        config.validate_fingerprint_preflight()?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.http_timeout_seconds))
            .build()
            .context("build HTTP client")?;
        Ok(Self {
            client,
            origin: origin_header(&config.server_url()?),
            server_url: config.server_url()?,
            test_id: config.test_id.clone(),
        })
    }

    pub async fn bootstrap_login(
        &self,
        request: &BootstrapRequest<'_>,
    ) -> anyhow::Result<BootstrapResponse> {
        let response = self
            .with_common_headers(
                self.client
                    .post(self.server_url.join("/api/bootstrap/login")?),
            )
            .json(request)
            .send()
            .await
            .context("send bootstrap request")?;
        if !response.status().is_success() {
            bail!("bootstrap failed");
        }
        response
            .json::<BootstrapResponse>()
            .await
            .context("parse bootstrap response")
    }

    pub async fn check_token(&self, token: &str) -> anyhow::Result<bool> {
        let response = self
            .with_common_headers(
                self.client
                    .post(self.server_url.join("/api/token/check")?)
                    .bearer_auth(token),
            )
            .send()
            .await
            .context("send token check request")?;

        if response.status() == StatusCode::UNAUTHORIZED {
            return Ok(false);
        }
        if !response.status().is_success() {
            bail!("token check failed");
        }
        let body = response
            .json::<TokenCheckResponse>()
            .await
            .context("parse token check response")?;
        Ok(body.valid)
    }

    pub async fn index_status(&self, token: &str) -> anyhow::Result<Value> {
        let response = self
            .with_common_headers(
                self.client
                    .get(self.server_url.join("/api/index/status")?)
                    .bearer_auth(token),
            )
            .send()
            .await
            .context("send index status request")?;
        if !response.status().is_success() {
            bail!("index status failed");
        }
        response
            .json::<Value>()
            .await
            .context("parse index status response")
    }

    pub async fn status(&self, token: &str) -> anyhow::Result<Value> {
        let token_valid = self.check_token(token).await?;
        let index = if token_valid {
            self.index_status(token).await?
        } else {
            Value::Null
        };
        Ok(json!({
            "token_valid": token_valid,
            "index": index,
        }))
    }

    pub async fn revoke_token(&self, token: &str) -> anyhow::Result<()> {
        let response = self
            .with_common_headers(
                self.client
                    .post(self.server_url.join("/api/token/revoke")?)
                    .bearer_auth(token),
            )
            .send()
            .await
            .context("send token revoke request")?;
        if !response.status().is_success() {
            bail!("token revoke failed");
        }
        Ok(())
    }

    pub async fn mcp(&self, token: &str, body: Value) -> anyhow::Result<Option<Value>> {
        let response = self
            .with_common_headers(
                self.client
                    .post(self.server_url.join("/mcp")?)
                    .bearer_auth(token)
                    .header(header::ACCEPT, "application/json")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("MCP-Protocol-Version", MCP_PROTOCOL_VERSION),
            )
            .json(&body)
            .send()
            .await
            .context("send MCP request")?;
        if response.status() == StatusCode::ACCEPTED {
            return Ok(None);
        }
        if !response.status().is_success() {
            bail!("MCP request failed");
        }
        Ok(Some(
            response
                .json::<Value>()
                .await
                .context("parse MCP response")?,
        ))
    }

    fn with_common_headers(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let builder = builder.header(header::ORIGIN, &self.origin);
        match &self.test_id {
            Some(test_id) => builder.header("X-Test-Id", test_id),
            None => builder,
        }
    }
}

fn origin_header(url: &Url) -> String {
    let host = url.host_str().expect("parsed URL has host");
    match url.port() {
        Some(port) => format!("{}://{}:{}", url.scheme(), host, port),
        None => format!("{}://{}", url.scheme(), host),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ClientConfig, ClientLoggingConfig};
    use std::path::PathBuf;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::oneshot,
    };

    #[tokio::test]
    async fn token_check_forwards_authorization_and_test_id() {
        let (server_url, request) = one_request_server(r#"{"valid":true}"#).await;
        let api = ClientApi::new(&test_config(server_url)).expect("api");

        assert!(api.check_token("mcp_testtoken").await.expect("token check"));
        let request = request.await.expect("captured request").to_lowercase();
        assert!(request.contains("post /api/token/check "));
        assert!(request.contains("authorization: bearer mcp_testtoken"));
        assert!(request.contains("x-test-id: jp-mcp-test-client"));
    }

    #[tokio::test]
    async fn mcp_request_forwards_protocol_authorization_and_test_id() {
        let response = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let (server_url, request) = one_request_server(response).await;
        let api = ClientApi::new(&test_config(server_url)).expect("api");

        let body = api
            .mcp(
                "mcp_testtoken",
                json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
            )
            .await
            .expect("mcp")
            .expect("response body");
        assert_eq!(body["result"]["ok"], true);

        let request = request.await.expect("captured request").to_lowercase();
        assert!(request.contains("post /mcp "));
        assert!(request.contains("authorization: bearer mcp_testtoken"));
        assert!(request.contains("mcp-protocol-version: 2025-06-18"));
        assert!(request.contains("x-test-id: jp-mcp-test-client"));
    }

    fn test_config(server_url: Url) -> ClientConfig {
        ClientConfig {
            server_url: Some(server_url),
            url_file: None,
            token_file: Some(PathBuf::from("/tmp/token")),
            test_id: Some("jp-mcp-test-client".to_string()),
            server_fingerprint: None,
            http_timeout_seconds: 5,
            logging: ClientLoggingConfig::default(),
        }
    }

    async fn one_request_server(response_body: &'static str) -> (Url, oneshot::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let (send, recv) = oneshot::channel();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            let mut buffer = vec![0_u8; 8192];
            let read = stream.read(&mut buffer).await.expect("read request");
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let _ = send.send(request);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        (
            Url::parse(&format!("http://{addr}")).expect("server url"),
            recv,
        )
    }
}
