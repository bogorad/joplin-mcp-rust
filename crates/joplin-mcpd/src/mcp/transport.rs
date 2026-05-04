use crate::{contracts::MCP_PROTOCOL_VERSION, http::AppState};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

pub async fn mcp_get() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
}

pub async fn mcp_delete() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
}

pub async fn mcp_post(
    State(_state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Some(version) = headers.get("mcp-protocol-version")
        && version != MCP_PROTOCOL_VERSION
    {
        return (StatusCode::BAD_REQUEST, "unsupported MCP protocol version").into_response();
    }

    if headers.get("authorization").is_none() {
        return (StatusCode::UNAUTHORIZED, "missing bearer token").into_response();
    }

    if body.get("method").is_none() {
        return StatusCode::ACCEPTED.into_response();
    }

    Json(json!({
        "jsonrpc": "2.0",
        "id": body.get("id").cloned().unwrap_or(Value::Null),
        "result": {
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "serverInfo": {
                "name": "joplin-mcpd",
                "version": env!("CARGO_PKG_VERSION")
            }
        }
    }))
    .into_response()
}
