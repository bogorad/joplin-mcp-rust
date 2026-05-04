use crate::{
    config::Config,
    contracts::MCP_PROTOCOL_VERSION,
    mcp::transport::{mcp_delete, mcp_get, mcp_post},
};
use axum::{Json, Router, routing::get};
use serde::Serialize;
use std::net::SocketAddr;
use tokio::net::TcpListener;

#[derive(Debug, Clone)]
pub struct AppState {
    pub config: Config,
}

pub fn router(config: Config) -> Router {
    let state = AppState { config };
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/mcp", get(mcp_get).post(mcp_post).delete(mcp_delete))
        .with_state(state)
}

pub async fn serve(config: Config) -> anyhow::Result<()> {
    let listen = config.server.listen;
    let app = router(config);
    let listener = TcpListener::bind(listen).await?;
    tracing::info!(operation = "serve", %listen, "joplin-mcpd listening");
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
}

async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: "joplin-mcpd",
    })
}

#[derive(Debug, Serialize)]
struct ReadyResponse {
    status: &'static str,
    mcp_protocol_version: &'static str,
}

async fn readyz() -> Json<ReadyResponse> {
    Json(ReadyResponse {
        status: "ready",
        mcp_protocol_version: MCP_PROTOCOL_VERSION,
    })
}

pub fn is_loopback_address(addr: SocketAddr) -> bool {
    addr.ip().is_loopback()
}
