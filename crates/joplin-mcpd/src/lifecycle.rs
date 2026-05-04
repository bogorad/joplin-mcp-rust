use crate::config::ServerConfig;
use axum::{
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use ipnet::IpNet;
use serde::Serialize;
use std::{
    future::Future,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Notify, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteIp(pub IpAddr);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessStatus {
    Ready,
    NotReady,
    ShuttingDown,
}

impl ReadinessStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::NotReady => "not_ready",
            Self::ShuttingDown => "shutting_down",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Readiness {
    inner: Arc<RwLock<ReadinessStatus>>,
}

impl Readiness {
    pub fn new(status: ReadinessStatus) -> Self {
        Self {
            inner: Arc::new(RwLock::new(status)),
        }
    }

    pub async fn status(&self) -> ReadinessStatus {
        *self.inner.read().await
    }

    pub async fn set(&self, status: ReadinessStatus) {
        *self.inner.write().await = status;
    }
}

#[derive(Debug, Clone)]
pub struct ShutdownDrain {
    readiness: Readiness,
    notify: Arc<Notify>,
}

impl ShutdownDrain {
    pub fn new(readiness: Readiness) -> Self {
        Self {
            readiness,
            notify: Arc::new(Notify::new()),
        }
    }

    pub async fn begin(&self) {
        self.readiness.set(ReadinessStatus::ShuttingDown).await;
        self.notify.notify_waiters();
    }

    pub async fn wait(&self) {
        self.notify.notified().await;
    }

    pub fn wait_signal(&self) -> impl Future<Output = ()> + Send + 'static {
        let drain = self.clone();
        async move {
            drain.wait().await;
        }
    }
}

pub fn timeout_response() -> Response {
    (StatusCode::REQUEST_TIMEOUT, "request timed out").into_response()
}

pub fn should_log_slow_request(started: Instant, threshold: Duration) -> bool {
    started.elapsed() >= threshold
}

pub fn extract_client_ip(
    peer: SocketAddr,
    headers: &HeaderMap,
    config: &ServerConfig,
) -> Result<IpAddr, StatusCode> {
    if !ip_in_nets(peer.ip(), &config.trusted_proxies) {
        return Ok(peer.ip());
    }

    let Some(value) = headers.get(config.forwarded_header.as_str()) else {
        return Ok(peer.ip());
    };

    parse_forwarded_ip(value).ok_or(StatusCode::BAD_REQUEST)
}

fn parse_forwarded_ip(value: &HeaderValue) -> Option<IpAddr> {
    let value = value.to_str().ok()?;
    let first = value.split(',').next()?.trim();
    first.parse().ok()
}

fn ip_in_nets(ip: IpAddr, nets: &[IpNet]) -> bool {
    nets.iter().any(|net| net.contains(&ip))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;
    use axum::http::HeaderMap;
    use std::time::Duration;

    #[tokio::test]
    async fn readiness_state_changes() {
        let readiness = Readiness::new(ReadinessStatus::NotReady);
        assert_eq!(readiness.status().await, ReadinessStatus::NotReady);
        readiness.set(ReadinessStatus::Ready).await;
        assert_eq!(readiness.status().await, ReadinessStatus::Ready);
    }

    #[tokio::test]
    async fn shutdown_drain_marks_readiness_before_signal() {
        let readiness = Readiness::new(ReadinessStatus::Ready);
        let drain = ShutdownDrain::new(readiness.clone());

        drain.begin().await;

        assert_eq!(readiness.status().await, ReadinessStatus::ShuttingDown);
    }

    #[test]
    fn ignores_forwarded_header_without_trusted_proxy() {
        let config = ServerConfig::default();
        let peer: SocketAddr = "192.0.2.10:1234".parse().expect("peer");
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "198.51.100.1".parse().expect("header"));

        let ip = extract_client_ip(peer, &headers, &config).expect("client ip");

        assert_eq!(ip, peer.ip());
    }

    #[test]
    fn accepts_forwarded_header_from_trusted_proxy() {
        let config = ServerConfig {
            trusted_proxies: vec!["192.0.2.0/24".parse().expect("net")],
            ..ServerConfig::default()
        };
        let peer: SocketAddr = "192.0.2.10:1234".parse().expect("peer");
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "198.51.100.1, 203.0.113.2".parse().expect("header"),
        );

        let ip = extract_client_ip(peer, &headers, &config).expect("client ip");

        assert_eq!(ip, "198.51.100.1".parse::<IpAddr>().expect("ip"));
    }

    #[test]
    fn rejects_malformed_forwarded_header_from_trusted_proxy() {
        let config = ServerConfig {
            trusted_proxies: vec!["192.0.2.0/24".parse().expect("net")],
            ..ServerConfig::default()
        };
        let peer: SocketAddr = "192.0.2.10:1234".parse().expect("peer");
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "not-an-ip".parse().expect("header"));

        let status = extract_client_ip(peer, &headers, &config).expect_err("bad header");

        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn slow_request_threshold_is_inclusive() {
        let started = Instant::now() - Duration::from_secs(2);
        assert!(should_log_slow_request(started, Duration::from_secs(1)));
    }

    #[test]
    fn slow_request_threshold_ignores_fast_requests() {
        let started = Instant::now() - Duration::from_millis(100);
        assert!(!should_log_slow_request(
            started,
            Duration::from_millis(1000)
        ));
    }
}
