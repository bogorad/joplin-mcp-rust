use crate::{
    auth::tokens::{HmacKey, TokenAuthError, TokenRecord, TokenRepository, parse_bearer_token},
    contracts::MCP_PROTOCOL_VERSION,
    http::{ApiBackend, AppState},
    lifecycle::ReadinessStatus,
    mcp::tools::{
        ToolError, UserScope, get_changes_since_tool, get_note_excerpt_tool,
        get_note_resources_tool, get_note_tool, get_notebook_tree_tool, get_notes_by_tag_tool,
        get_recent_notes_tool, list_notebooks_tool, list_notes_tool, list_tags_tool,
        search_notes_tool, status_tool, tools_list_response,
    },
    observability::metrics,
};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::Utc;
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::time::timeout;
use tracing::Instrument;

#[derive(Debug, Clone)]
pub enum McpAuth {
    SyntaxOnly,
    Repository {
        repository: TokenRepository,
        hmac_keys: Arc<Vec<HmacKey>>,
    },
}

impl McpAuth {
    pub async fn authenticate(
        &self,
        headers: &HeaderMap,
    ) -> Result<Option<TokenRecord>, TokenAuthError> {
        let authorization = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok());

        match self {
            Self::SyntaxOnly => {
                parse_bearer_token(authorization)?;
                Ok(None)
            }
            Self::Repository {
                repository,
                hmac_keys,
            } => {
                let token = repository
                    .authenticate_bearer(authorization, hmac_keys, Utc::now())
                    .await?;
                Ok(Some(token))
            }
        }
    }
}

const JSONRPC_INVALID_PARAMS: i64 = -32602;
const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;
const JSONRPC_TOOL_ERROR: i64 = -32000;

pub async fn mcp_get() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
}

pub async fn mcp_delete() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
}

pub async fn mcp_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if state.readiness.status().await == ReadinessStatus::ShuttingDown {
        return (StatusCode::SERVICE_UNAVAILABLE, "server is shutting down").into_response();
    }

    if !accepts_json(&headers) {
        return StatusCode::NOT_ACCEPTABLE.into_response();
    }

    if !has_json_content_type(&headers) {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }

    if let Err(error) = validate_protocol_version(&headers, &body) {
        return error.into_response();
    }

    let token = match state.mcp_auth.authenticate(&headers).await {
        Ok(token) => token,
        Err(_) => {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };

    if is_json_rpc_notification_or_response(&body) {
        return StatusCode::ACCEPTED.into_response();
    }

    json_rpc_response(dispatch_json_rpc_request(&state, token.as_ref(), &body).await)
        .into_response()
}

async fn dispatch_json_rpc_request(
    state: &AppState,
    token: Option<&TokenRecord>,
    body: &Value,
) -> Value {
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = body.get("method").and_then(Value::as_str) else {
        return json_rpc_error(id, JSONRPC_INVALID_PARAMS, "missing JSON-RPC method", None);
    };

    match method {
        "initialize" => json_rpc_result(id, initialize_result()),
        "tools/list" => json_rpc_result(id, tools_list_response()),
        "tools/call" => match dispatch_tool_call(state, token, body).await {
            Ok(result) => json_rpc_result(id, tool_call_result(result)),
            Err(error) => json_rpc_error(
                id,
                JSONRPC_TOOL_ERROR,
                &error.message,
                Some(error.to_mcp_error()),
            ),
        },
        _ => json_rpc_error(id, JSONRPC_METHOD_NOT_FOUND, "method not found", None),
    }
}

async fn dispatch_tool_call(
    state: &AppState,
    token: Option<&TokenRecord>,
    body: &Value,
) -> Result<Value, ToolError> {
    let tool_name = tool_call_name(body).unwrap_or("unknown");
    let started = Instant::now();
    let result = timeout(
        Duration::from_secs(state.config.mcp.tool_timeout_seconds),
        call_tool(state, token, body)
            .instrument(tracing::info_span!("mcp.tool_call", tool = tool_name)),
    )
    .await;

    metrics::record_mcp_tool_duration(tool_name, started.elapsed());
    match result {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(error)) => {
            metrics::record_mcp_tool_error(tool_name, &error.code.to_string());
            Err(error)
        }
        Err(_) => {
            metrics::record_mcp_tool_error(tool_name, "internal");
            Err(ToolError::internal("MCP tool timed out", "timeout"))
        }
    }
}

fn tool_call_name(body: &Value) -> Option<&str> {
    body.get("params")
        .and_then(Value::as_object)
        .and_then(|params| params.get("name"))
        .and_then(Value::as_str)
}

async fn call_tool(
    state: &AppState,
    token: Option<&TokenRecord>,
    body: &Value,
) -> Result<Value, ToolError> {
    let token =
        token.ok_or_else(|| ToolError::validation("tools/call requires repository auth"))?;
    let params = body
        .get("params")
        .and_then(Value::as_object)
        .ok_or_else(|| ToolError::validation("tools/call params must be an object"))?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::validation("tools/call missing required field name"))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let ApiBackend::Repository {
        mcp_pool,
        active_hmac_key,
        ..
    } = &state.api_backend
    else {
        return Err(ToolError::validation(
            "tools/call backend is not configured",
        ));
    };
    let scope = UserScope::new(token.user_id);
    let cursor_key = active_hmac_key.bytes.as_slice();
    let max_response_bytes = state.config.mcp.max_response_bytes;

    match name {
        "status" => status_tool(mcp_pool, &scope).await,
        "list_notebooks" => list_notebooks_tool(mcp_pool, &scope).await,
        "list_tags" => list_tags_tool(mcp_pool, &scope).await,
        "list_notes" => {
            list_notes_tool(mcp_pool, &scope, &arguments, cursor_key, max_response_bytes).await
        }
        "search_notes" => {
            search_notes_tool(
                mcp_pool,
                &scope,
                &arguments,
                cursor_key,
                &state.config.index.text_search_config,
                max_response_bytes,
            )
            .await
        }
        "get_note" => {
            get_note_tool(mcp_pool, &scope, &arguments, cursor_key, max_response_bytes).await
        }
        "get_note_excerpt" => {
            get_note_excerpt_tool(mcp_pool, &scope, &arguments, max_response_bytes).await
        }
        "get_recent_notes" => {
            get_recent_notes_tool(mcp_pool, &scope, &arguments, cursor_key, max_response_bytes)
                .await
        }
        "get_notes_by_tag" => get_notes_by_tag_tool(mcp_pool, &scope, &arguments, cursor_key).await,
        "get_notebook_tree" => get_notebook_tree_tool(mcp_pool, &scope).await,
        "get_changes_since" => {
            get_changes_since_tool(mcp_pool, &scope, &arguments, cursor_key, max_response_bytes)
                .await
        }
        "get_note_resources" => {
            get_note_resources_tool(mcp_pool, &scope, &arguments, max_response_bytes).await
        }
        _ => Err(ToolError::validation("unknown tool")),
    }
}

fn tool_call_result(result: Value) -> Value {
    let text = serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_string());
    json!({
        "content": [
            {
                "type": "text",
                "text": text
            }
        ],
        "structuredContent": result,
        "isError": false
    })
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": "joplin-mcpd",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

fn json_rpc_result(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn json_rpc_error(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
            "data": data
        }
    })
}

fn json_rpc_response(body: Value) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/json")], Json(body))
}

fn validate_protocol_version(headers: &HeaderMap, body: &Value) -> Result<(), StatusCode> {
    match headers
        .get("mcp-protocol-version")
        .and_then(|value| value.to_str().ok())
    {
        Some(MCP_PROTOCOL_VERSION) => Ok(()),
        Some(_) => Err(StatusCode::BAD_REQUEST),
        None if is_initialize_request(body) => Ok(()),
        None => Err(StatusCode::BAD_REQUEST),
    }
}

fn is_initialize_request(body: &Value) -> bool {
    body.get("method").and_then(Value::as_str) == Some("initialize")
}

fn is_json_rpc_notification_or_response(body: &Value) -> bool {
    body.get("id").is_none() || body.get("method").is_none()
}

fn accepts_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|value| {
            value.split(',').any(|part| {
                let media_type = part.split(';').next().unwrap_or("").trim();
                media_type == "*/*"
                    || media_type == "application/*"
                    || media_type.eq_ignore_ascii_case("application/json")
            })
        })
}

fn has_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(';').next().is_some_and(|media_type| {
                media_type.trim().eq_ignore_ascii_case("application/json")
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    #[test]
    fn protocol_version_is_required_after_initialize() {
        let headers = HeaderMap::new();
        let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
        assert_eq!(
            validate_protocol_version(&headers, &request),
            Err(StatusCode::BAD_REQUEST)
        );
    }

    #[test]
    fn initialize_request_may_omit_protocol_version() {
        let headers = HeaderMap::new();
        let request = json!({"jsonrpc":"2.0","id":1,"method":"initialize"});
        validate_protocol_version(&headers, &request).expect("initialize accepted");
    }

    #[test]
    fn unsupported_protocol_version_is_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "mcp-protocol-version",
            HeaderValue::from_static("2024-11-05"),
        );
        let request = json!({"jsonrpc":"2.0","id":1,"method":"initialize"});
        assert_eq!(
            validate_protocol_version(&headers, &request),
            Err(StatusCode::BAD_REQUEST)
        );
    }

    #[test]
    fn notifications_and_responses_do_not_return_json_body() {
        assert!(is_json_rpc_notification_or_response(
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        ));
        assert!(is_json_rpc_notification_or_response(
            &json!({"jsonrpc":"2.0","id":1,"result":{}})
        ));
        assert!(!is_json_rpc_notification_or_response(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})
        ));
    }

    #[test]
    fn accept_header_must_allow_json() {
        let mut headers = HeaderMap::new();
        assert!(accepts_json(&headers));
        headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        assert!(accepts_json(&headers));
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        assert!(!accepts_json(&headers));
    }
}
