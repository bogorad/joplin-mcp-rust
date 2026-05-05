use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Notify, RwLock};

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

#[cfg(test)]
mod tests {
    use super::*;
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
