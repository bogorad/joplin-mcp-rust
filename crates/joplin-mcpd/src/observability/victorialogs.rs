use anyhow::{Context, bail};
use reqwest::Url;
use std::{env, fmt, time::Duration};
use tokio::time::{Instant, sleep};

pub const LOCAL_VICTORIALOGS_URL: &str = "http://127.0.0.1:59428";
pub const LIVE_VICTORIALOGS_URL: &str = "http://victorialogs.lan:9428";
pub const OTLP_LOGS_PATH: &str = "/insert/opentelemetry/v1/logs";
pub const LOGSQL_QUERY_PATH: &str = "/select/logsql/query";

const DEFAULT_LOCAL_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_LIVE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct VictoriaLogsHarness {
    client: reqwest::Client,
    base_url: Url,
    timeout: Duration,
    initial_backoff: Duration,
    max_backoff: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedLog {
    text: String,
}

impl ExpectedLog {
    pub fn containing(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

impl VictoriaLogsHarness {
    pub fn local() -> anyhow::Result<Self> {
        Self::new(LOCAL_VICTORIALOGS_URL, DEFAULT_LOCAL_TIMEOUT)
    }

    pub fn live() -> anyhow::Result<Self> {
        Self::new(LIVE_VICTORIALOGS_URL, DEFAULT_LIVE_TIMEOUT)
    }

    pub fn from_env() -> anyhow::Result<Self> {
        if env::var("JP_MCP_LIVE_JOPLIN").as_deref() == Ok("1") {
            return Self::live();
        }
        Self::local()
    }

    pub fn new(base_url: impl AsRef<str>, timeout: Duration) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(DEFAULT_MAX_BACKOFF)
            .build()
            .context("build VictoriaLogs HTTP client")?;
        Self::with_client(client, base_url, timeout)
    }

    pub fn with_client(
        client: reqwest::Client,
        base_url: impl AsRef<str>,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        let base_url = Url::parse(base_url.as_ref()).context("parse VictoriaLogs base URL")?;
        Ok(Self {
            client,
            base_url,
            timeout,
            initial_backoff: DEFAULT_INITIAL_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
        })
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    pub fn query_url(&self) -> anyhow::Result<Url> {
        self.base_url
            .join(LOGSQL_QUERY_PATH.trim_start_matches('/'))
            .context("build VictoriaLogs LogSQL query URL")
    }

    pub fn otlp_logs_url(&self) -> anyhow::Result<Url> {
        self.base_url
            .join(OTLP_LOGS_PATH.trim_start_matches('/'))
            .context("build VictoriaLogs OTLP logs URL")
    }

    pub fn query_for_test_id(test_id: &str) -> String {
        format!(r#"{{test.id="{}"}}"#, escape_logsql_value(test_id))
    }

    pub async fn query_raw(&self, query: &str) -> anyhow::Result<String> {
        let response = self
            .client
            .post(self.query_url()?)
            .form(&[("query", query)])
            .send()
            .await
            .context("query VictoriaLogs")?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("read VictoriaLogs response")?;
        if !status.is_success() {
            bail!("VictoriaLogs query failed with status {status}: {body}");
        }
        Ok(body)
    }

    pub async fn poll_for_test_logs(
        &self,
        test_id: &str,
        expected: &[ExpectedLog],
    ) -> anyhow::Result<String> {
        let query = Self::query_for_test_id(test_id);
        self.poll_until_query_contains(&query, expected).await
    }

    pub async fn poll_until_query_contains(
        &self,
        query: &str,
        expected: &[ExpectedLog],
    ) -> anyhow::Result<String> {
        let deadline = Instant::now() + self.timeout;
        let mut backoff = self.initial_backoff;
        loop {
            let last_body = self.query_raw(query).await?;
            if missing_expected_logs(&last_body, expected).is_empty() {
                return Ok(last_body);
            }

            let now = Instant::now();
            if now >= deadline {
                let missing = missing_expected_logs(&last_body, expected);
                bail!(
                    "VictoriaLogs did not contain expected logs before timeout: {}. query={query}; last_response={last_body}",
                    MissingLogs(&missing)
                );
            }

            let remaining = deadline.saturating_duration_since(now);
            sleep(backoff.min(remaining)).await;
            backoff = (backoff * 2).min(self.max_backoff);
        }
    }
}

fn escape_logsql_value(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn missing_expected_logs<'a>(body: &str, expected: &'a [ExpectedLog]) -> Vec<&'a str> {
    expected
        .iter()
        .filter_map(|log| {
            if body.contains(&log.text) {
                None
            } else {
                Some(log.text.as_str())
            }
        })
        .collect()
}

struct MissingLogs<'a>(&'a [&'a str]);

impl fmt::Display for MissingLogs<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, missing) in self.0.iter().enumerate() {
            if index > 0 {
                formatter.write_str(", ")?;
            }
            write!(formatter, "{missing:?}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::oneshot,
    };

    fn live_joplin_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .expect("live Joplin env lock")
    }

    #[test]
    fn selects_local_endpoint_by_default() {
        let _guard = live_joplin_env_lock();
        unsafe {
            env::remove_var("JP_MCP_LIVE_JOPLIN");
        }

        let harness = VictoriaLogsHarness::from_env().expect("harness");

        assert_eq!(harness.base_url().as_str(), "http://127.0.0.1:59428/");
        assert_eq!(
            harness.otlp_logs_url().expect("otlp url").as_str(),
            "http://127.0.0.1:59428/insert/opentelemetry/v1/logs"
        );
        assert_eq!(
            harness.query_url().expect("query url").as_str(),
            "http://127.0.0.1:59428/select/logsql/query"
        );
    }

    #[test]
    fn selects_live_endpoint_for_real_joplin_tests() {
        let _guard = live_joplin_env_lock();
        unsafe {
            env::set_var("JP_MCP_LIVE_JOPLIN", "1");
        }

        let harness = VictoriaLogsHarness::from_env().expect("harness");
        assert_eq!(harness.base_url().as_str(), "http://victorialogs.lan:9428/");
        assert_eq!(
            harness.otlp_logs_url().expect("otlp url").as_str(),
            "http://victorialogs.lan:9428/insert/opentelemetry/v1/logs"
        );
        assert_eq!(
            harness.query_url().expect("query url").as_str(),
            "http://victorialogs.lan:9428/select/logsql/query"
        );

        unsafe {
            env::remove_var("JP_MCP_LIVE_JOPLIN");
        }
    }

    #[test]
    fn builds_test_id_query_with_escaping() {
        assert_eq!(
            VictoriaLogsHarness::query_for_test_id(r#"jp-mcp-test-a"b\c"#),
            r#"{test.id="jp-mcp-test-a\"b\\c"}"#
        );
    }

    #[tokio::test]
    async fn posts_logsql_query_form() {
        let (base_url, request) = one_response_server("[]").await;
        let harness = VictoriaLogsHarness::new(base_url, Duration::from_secs(1)).expect("harness");

        let body = harness
            .query_raw(r#"{test.id="jp-mcp-test-one"}"#)
            .await
            .expect("query");
        let request = request.await.expect("captured request").to_lowercase();

        assert_eq!(body, "[]");
        assert!(request.contains("post /select/logsql/query "));
        assert!(request.contains("content-type: application/x-www-form-urlencoded"));
        assert!(request.contains("query=%7btest.id%3d%22jp-mcp-test-one%22%7d"));
    }

    #[tokio::test]
    async fn polls_until_expected_logs_are_present() {
        let (base_url, requests) = sequenced_response_server(vec![
            "[]".to_string(),
            r#"[{"test.id":"jp-mcp-test-one","operation":"http_request","outcome":"completed"}]"#
                .to_string(),
        ])
        .await;
        let harness = VictoriaLogsHarness::new(base_url, Duration::from_secs(1)).expect("harness");

        let body = harness
            .poll_for_test_logs(
                "jp-mcp-test-one",
                &[
                    ExpectedLog::containing("http_request"),
                    ExpectedLog::containing("completed"),
                ],
            )
            .await
            .expect("logs found");

        assert!(body.contains("http_request"));
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn missing_logs_fail_even_when_query_succeeds() {
        let (base_url, _) = sequenced_response_server(vec!["[]".to_string(); 8]).await;
        let harness =
            VictoriaLogsHarness::new(base_url, Duration::from_millis(10)).expect("harness");

        let error = harness
            .poll_for_test_logs(
                "jp-mcp-test-one",
                &[ExpectedLog::containing("request completed")],
            )
            .await
            .expect_err("missing logs fail");
        let message = error.to_string();

        assert!(message.contains("request completed"));
        assert!(message.contains("jp-mcp-test-one"));
        assert!(message.contains("last_response=[]"));
    }

    async fn one_response_server(
        response_body: &'static str,
    ) -> (String, oneshot::Receiver<String>) {
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
            write_response(&mut stream, response_body).await;
        });
        (format!("http://{addr}"), recv)
    }

    async fn sequenced_response_server(responses: Vec<String>) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let requests = Arc::new(AtomicUsize::new(0));
        let requests_for_task = Arc::clone(&requests);
        tokio::spawn(async move {
            for response_body in responses {
                let (mut stream, _) = listener.accept().await.expect("accept request");
                let mut buffer = vec![0_u8; 8192];
                let _ = stream.read(&mut buffer).await.expect("read request");
                requests_for_task.fetch_add(1, Ordering::SeqCst);
                write_response(&mut stream, &response_body).await;
            }
        });
        (format!("http://{addr}"), requests)
    }

    async fn write_response(stream: &mut tokio::net::TcpStream, body: &str) {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    }
}
