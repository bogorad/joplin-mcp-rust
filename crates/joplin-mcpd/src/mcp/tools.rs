use crate::contracts::{ErrorCode, READ_ONLY_MODE, UNSUPPORTED_SHARED_NOTEBOOKS};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

const CURSOR_PREFIX: &str = "jmc1";
const CURSOR_VERSION: u8 = 1;
const DEFAULT_RETRY_AFTER_SECONDS: u64 = 5;
const NOTE_PREVIEW_CHARS: usize = 200;
const ILIKE_FALLBACK_MAX_QUERY_CHARS: usize = 128;

#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserScope {
    user_id: Uuid,
}

impl UserScope {
    pub fn new(user_id: Uuid) -> Self {
        Self { user_id }
    }

    pub fn user_id(&self) -> Uuid {
        self.user_id
    }

    pub fn predicate(&self, bind_index: usize) -> String {
        format!("user_id = ${bind_index}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ToolError {
    pub fn validation(message: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::Validation,
            message: message.into(),
            data: None,
        }
    }

    pub fn index_not_ready(index_status: IndexStatus) -> Self {
        Self {
            code: ErrorCode::IndexNotReady,
            message: "index is not ready".to_string(),
            data: Some(json!({
                "index_status": index_status,
                "retry_after_seconds": DEFAULT_RETRY_AFTER_SECONDS
            })),
        }
    }

    pub fn response_too_large(max_response_bytes: usize, actual_response_bytes: usize) -> Self {
        Self {
            code: ErrorCode::Validation,
            message: "serialized response exceeds mcp.max_response_bytes".to_string(),
            data: Some(json!({
                "max_response_bytes": max_response_bytes,
                "actual_response_bytes": actual_response_bytes
            })),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::NotFound,
            message: message.into(),
            data: None,
        }
    }

    pub(crate) fn internal(message: impl Into<String>, error: impl std::fmt::Display) -> Self {
        Self {
            code: ErrorCode::Internal,
            message: message.into(),
            data: Some(json!({ "error": error.to_string() })),
        }
    }

    pub fn to_mcp_error(&self) -> Value {
        json!({
            "code": self.code,
            "message": self.message,
            "data": self.data
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexStatus {
    Ready,
    Stale,
    Rebuilding,
    Error,
    Missing,
}

impl IndexStatus {
    fn from_db(value: &str) -> Result<Self, ToolError> {
        match value {
            "ready" => Ok(Self::Ready),
            "stale" => Ok(Self::Stale),
            "rebuilding" => Ok(Self::Rebuilding),
            "error" => Ok(Self::Error),
            _ => Err(ToolError::validation(format!(
                "unknown index status {value}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexStateSnapshot {
    pub status: IndexStatus,
    pub last_full_rebuild_at: Option<DateTime<Utc>>,
    pub last_incremental_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexMetadata {
    pub index_status: IndexStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_indexed_at: Option<DateTime<Utc>>,
}

pub fn gate_index_state(snapshot: Option<&IndexStateSnapshot>) -> Result<IndexMetadata, ToolError> {
    let Some(snapshot) = snapshot else {
        return Err(ToolError::index_not_ready(IndexStatus::Missing));
    };

    match snapshot.status {
        IndexStatus::Ready | IndexStatus::Stale => Ok(IndexMetadata {
            index_status: snapshot.status,
            last_indexed_at: snapshot
                .last_incremental_at
                .or(snapshot.last_full_rebuild_at)
                .or(snapshot.updated_at),
        }),
        status => Err(ToolError::index_not_ready(status)),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncatedText {
    pub value: String,
    pub truncated: bool,
    pub next_offset: Option<usize>,
}

pub fn truncate_text_at_chars(text: &str, start_offset: usize, max_chars: usize) -> TruncatedText {
    let iter = text
        .char_indices()
        .filter(|(offset, _)| *offset >= start_offset);
    let mut end = start_offset;

    for (chars, (offset, ch)) in iter.enumerate() {
        if chars == max_chars {
            return TruncatedText {
                value: text[start_offset..end].to_string(),
                truncated: true,
                next_offset: Some(offset),
            };
        }
        end = offset + ch.len_utf8();
    }

    TruncatedText {
        value: text.get(start_offset..end).unwrap_or("").to_string(),
        truncated: false,
        next_offset: None,
    }
}

pub fn enforce_response_budget(
    value: Value,
    max_response_bytes: usize,
) -> Result<Value, ToolError> {
    let bytes = serde_json::to_vec(&value).map_err(|error| ToolError {
        code: ErrorCode::Internal,
        message: "failed to serialize MCP response".to_string(),
        data: Some(json!({ "error": error.to_string() })),
    })?;

    if bytes.len() > max_response_bytes {
        Err(ToolError::response_too_large(
            max_response_bytes,
            bytes.len(),
        ))
    } else {
        Ok(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorPayload {
    pub version: u8,
    pub tool: String,
    pub filter_hash: String,
    pub sort_keys: Vec<String>,
    pub last_values: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<BodyCursorPayload>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BodyCursorPayload {
    pub note_id: String,
    pub indexed_note_version: String,
    pub next_body_offset: usize,
}

pub fn filter_hash(filter: &Value) -> String {
    let bytes = serde_json::to_vec(filter).expect("serde_json::Value serializes");
    let digest = Sha256::digest(bytes);
    URL_SAFE_NO_PAD.encode(digest)
}

pub fn encode_cursor(payload: &CursorPayload, signing_key: &[u8]) -> Result<String, ToolError> {
    if payload.version != CURSOR_VERSION {
        return Err(ToolError::validation("unsupported cursor version"));
    }
    let body = serde_json::to_vec(payload).map_err(|error| ToolError {
        code: ErrorCode::Internal,
        message: "failed to encode cursor".to_string(),
        data: Some(json!({ "error": error.to_string() })),
    })?;
    let body = URL_SAFE_NO_PAD.encode(body);
    let signature = sign_cursor_body(&body, signing_key)?;
    Ok(format!("{CURSOR_PREFIX}.{body}.{signature}"))
}

pub fn decode_cursor(
    cursor: &str,
    signing_key: &[u8],
    expected_tool: &str,
    expected_filter_hash: &str,
) -> Result<CursorPayload, ToolError> {
    let mut parts = cursor.split('.');
    let prefix = parts
        .next()
        .ok_or_else(|| ToolError::validation("invalid cursor"))?;
    let body = parts
        .next()
        .ok_or_else(|| ToolError::validation("invalid cursor"))?;
    let signature = parts
        .next()
        .ok_or_else(|| ToolError::validation("invalid cursor"))?;
    if parts.next().is_some() || prefix != CURSOR_PREFIX {
        return Err(ToolError::validation("invalid cursor"));
    }

    let expected_signature = sign_cursor_body(body, signing_key)?;
    if signature != expected_signature {
        return Err(ToolError::validation("invalid cursor"));
    }

    let body = URL_SAFE_NO_PAD
        .decode(body)
        .map_err(|_| ToolError::validation("invalid cursor"))?;
    let payload: CursorPayload =
        serde_json::from_slice(&body).map_err(|_| ToolError::validation("invalid cursor"))?;

    if payload.version != CURSOR_VERSION {
        return Err(ToolError::validation("unsupported cursor version"));
    }
    if payload.tool != expected_tool {
        return Err(ToolError::validation("cursor does not match tool"));
    }
    if payload.filter_hash != expected_filter_hash {
        return Err(ToolError::validation("cursor does not match filters"));
    }
    Ok(payload)
}

fn sign_cursor_body(body: &str, signing_key: &[u8]) -> Result<String, ToolError> {
    let mut mac = HmacSha256::new_from_slice(signing_key)
        .map_err(|_| ToolError::validation("cursor signing key must be valid for HMAC-SHA256"))?;
    mac.update(body.as_bytes());
    Ok(URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

pub fn tools_list_response() -> Value {
    json!({ "tools": tool_definitions() })
}

pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "status",
            description: "Return JP-MCP read-only status for the authenticated user.",
            input_schema: empty_schema(),
        },
        ToolDefinition {
            name: "list_notebooks",
            description: "List indexed notebooks for the authenticated user.",
            input_schema: empty_schema(),
        },
        ToolDefinition {
            name: "list_notes",
            description: "List indexed unencrypted notes for the authenticated user.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "notebook_id": joplin_id_schema(),
                    "tag_ids": joplin_id_array_schema(),
                    "updated_after": unix_ms_schema(),
                    "updated_before": unix_ms_schema(),
                    "is_todo": {"type": "boolean"},
                    "limit": limit_schema(1, 100, 50),
                    "cursor": cursor_schema()
                },
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "search_notes",
            description: "Search indexed unencrypted notes for the authenticated user.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "minLength": 1, "maxLength": 1024},
                    "notebook_id": joplin_id_schema(),
                    "tag_ids": joplin_id_array_schema(),
                    "updated_after": unix_ms_schema(),
                    "updated_before": unix_ms_schema(),
                    "is_todo": {"type": "boolean"},
                    "limit": limit_schema(1, 100, 20),
                    "cursor": cursor_schema(),
                    "limit_body_chars": limit_schema(1, 8000, 8000)
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "get_note",
            description: "Return bounded note body content for one indexed note.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "note_id": joplin_id_schema(),
                    "body_cursor": cursor_schema(),
                    "max_body_chars": limit_schema(1, 8000, 8000)
                },
                "required": ["note_id"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "get_note_excerpt",
            description: "Return a bounded note excerpt for one indexed note.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "note_id": joplin_id_schema(),
                    "max_chars": limit_schema(1, 2000, 2000)
                },
                "required": ["note_id"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "get_recent_notes",
            description: "List recent indexed unencrypted notes for the authenticated user.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "limit": limit_schema(1, 100, 20),
                    "cursor": cursor_schema()
                },
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "list_tags",
            description: "List indexed tags for the authenticated user.",
            input_schema: empty_schema(),
        },
        ToolDefinition {
            name: "get_notes_by_tag",
            description: "List indexed notes associated with one tag.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tag_id": joplin_id_schema(),
                    "limit": limit_schema(1, 100, 50),
                    "cursor": cursor_schema()
                },
                "required": ["tag_id"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "get_notebook_tree",
            description: "Return indexed notebooks as a hierarchy.",
            input_schema: empty_schema(),
        },
        ToolDefinition {
            name: "get_changes_since",
            description: "List indexed notes changed since a unix millisecond timestamp.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "since": unix_ms_schema(),
                    "limit": limit_schema(1, 100, 100),
                    "cursor": cursor_schema()
                },
                "required": ["since"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "get_note_resources",
            description: "Return metadata for resources referenced by one indexed note.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "note_id": joplin_id_schema()
                },
                "required": ["note_id"],
                "additionalProperties": false
            }),
        },
    ]
}

pub fn validate_tool_input(tool_name: &str, input: &Value) -> Result<(), ToolError> {
    let tool = tool_definitions()
        .into_iter()
        .find(|tool| tool.name == tool_name)
        .ok_or_else(|| ToolError::validation("unknown tool"))?;
    validate_against_schema(tool.name, input, &tool.input_schema)
}

fn validate_against_schema(
    tool_name: &str,
    input: &Value,
    schema: &Value,
) -> Result<(), ToolError> {
    let object = input
        .as_object()
        .ok_or_else(|| ToolError::validation(format!("{tool_name} input must be an object")))?;
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| ToolError::validation(format!("{tool_name} schema has no properties")))?;

    reject_unknown_fields(tool_name, object, properties)?;
    require_fields(tool_name, object, schema)?;

    for (name, value) in object {
        let property_schema = properties
            .get(name)
            .ok_or_else(|| ToolError::validation(format!("{tool_name} unknown field {name}")))?;
        validate_property(tool_name, name, value, property_schema)?;
    }
    Ok(())
}

fn reject_unknown_fields(
    tool_name: &str,
    object: &Map<String, Value>,
    properties: &Map<String, Value>,
) -> Result<(), ToolError> {
    for name in object.keys() {
        if !properties.contains_key(name) {
            return Err(ToolError::validation(format!(
                "{tool_name} unknown field {name}"
            )));
        }
    }
    Ok(())
}

fn require_fields(
    tool_name: &str,
    object: &Map<String, Value>,
    schema: &Value,
) -> Result<(), ToolError> {
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str);
    for name in required {
        if !object.contains_key(name) {
            return Err(ToolError::validation(format!(
                "{tool_name} missing required field {name}"
            )));
        }
    }
    Ok(())
}

fn validate_property(
    tool_name: &str,
    name: &str,
    value: &Value,
    schema: &Value,
) -> Result<(), ToolError> {
    match schema.get("type").and_then(Value::as_str) {
        Some("string") => validate_string(tool_name, name, value, schema),
        Some("integer") => validate_integer(tool_name, name, value, schema),
        Some("boolean") => {
            if value.is_boolean() {
                Ok(())
            } else {
                Err(ToolError::validation(format!(
                    "{tool_name}.{name} must be a boolean"
                )))
            }
        }
        Some("array") => validate_array(tool_name, name, value, schema),
        _ => Err(ToolError::validation(format!(
            "{tool_name}.{name} has unsupported schema"
        ))),
    }
}

fn validate_string(
    tool_name: &str,
    name: &str,
    value: &Value,
    schema: &Value,
) -> Result<(), ToolError> {
    let Some(value) = value.as_str() else {
        return Err(ToolError::validation(format!(
            "{tool_name}.{name} must be a string"
        )));
    };
    if let Some(min) = schema.get("minLength").and_then(Value::as_u64)
        && value.chars().count() < min as usize
    {
        return Err(ToolError::validation(format!(
            "{tool_name}.{name} is shorter than minimum length"
        )));
    }
    if let Some(max) = schema.get("maxLength").and_then(Value::as_u64)
        && value.chars().count() > max as usize
    {
        return Err(ToolError::validation(format!(
            "{tool_name}.{name} exceeds maximum length"
        )));
    }
    if schema.get("pattern").and_then(Value::as_str) == Some("^[0-9A-Fa-f]{32}$")
        && !crate::contracts::is_joplin_id(value)
    {
        return Err(ToolError::validation(format!(
            "{tool_name}.{name} must be a 32-character Joplin ID"
        )));
    }
    Ok(())
}

fn validate_integer(
    tool_name: &str,
    name: &str,
    value: &Value,
    schema: &Value,
) -> Result<(), ToolError> {
    let Some(value) = value.as_i64() else {
        return Err(ToolError::validation(format!(
            "{tool_name}.{name} must be an integer"
        )));
    };
    if let Some(min) = schema.get("minimum").and_then(Value::as_i64)
        && value < min
    {
        return Err(ToolError::validation(format!(
            "{tool_name}.{name} is below minimum"
        )));
    }
    if let Some(max) = schema.get("maximum").and_then(Value::as_i64)
        && value > max
    {
        return Err(ToolError::validation(format!(
            "{tool_name}.{name} exceeds maximum"
        )));
    }
    Ok(())
}

fn validate_array(
    tool_name: &str,
    name: &str,
    value: &Value,
    schema: &Value,
) -> Result<(), ToolError> {
    let Some(values) = value.as_array() else {
        return Err(ToolError::validation(format!(
            "{tool_name}.{name} must be an array"
        )));
    };
    if let Some(max) = schema.get("maxItems").and_then(Value::as_u64)
        && values.len() > max as usize
    {
        return Err(ToolError::validation(format!(
            "{tool_name}.{name} has too many items"
        )));
    }
    let item_schema = schema
        .get("items")
        .ok_or_else(|| ToolError::validation(format!("{tool_name}.{name} has no item schema")))?;
    for value in values {
        validate_property(tool_name, name, value, item_schema)?;
    }
    Ok(())
}

pub fn status_response(user: &str, index: IndexMetadata) -> Value {
    json!({
        "user": user,
        "mode": READ_ONLY_MODE,
        "index_status": index.index_status,
        "last_indexed_at": index.last_indexed_at,
        "unencrypted_only": true,
        "shared_notebooks": UNSUPPORTED_SHARED_NOTEBOOKS
    })
}

pub async fn status_tool(pool: &PgPool, scope: &UserScope) -> Result<Value, ToolError> {
    let row = sqlx::query_as::<_, StatusRow>(
        r#"
        SELECT
            users.joplin_email,
            state.status,
            state.last_full_rebuild_at,
            state.last_incremental_at,
            state.updated_at
        FROM joplin_mcp.mcp_users users
        LEFT JOIN joplin_mcp.index_state state ON state.user_id = users.id
        WHERE users.id = $1
        "#,
    )
    .bind(scope.user_id())
    .fetch_optional(pool)
    .await
    .map_err(|error| ToolError::internal("failed to read status", error))?
    .ok_or_else(|| ToolError::not_found("user not found"))?;

    let joplin_email = row.joplin_email.clone();
    let index = row.index_metadata()?;
    Ok(status_response(&joplin_email, index))
}

pub async fn list_notebooks_tool(pool: &PgPool, scope: &UserScope) -> Result<Value, ToolError> {
    let index = ready_index_metadata(pool, scope).await?;
    let notebooks = fetch_notebook_rows(pool, scope).await?;
    Ok(json!({
        "index_status": index.index_status,
        "last_indexed_at": index.last_indexed_at,
        "notebooks": notebook_items(&notebooks)
    }))
}

pub async fn list_tags_tool(pool: &PgPool, scope: &UserScope) -> Result<Value, ToolError> {
    let index = ready_index_metadata(pool, scope).await?;
    let tags = sqlx::query_as::<_, TagRow>(
        r#"
        SELECT
            tags.joplin_id,
            tags.title,
            COUNT(notes.joplin_id)::bigint AS note_count
        FROM joplin_mcp.tags_index tags
        LEFT JOIN joplin_mcp.note_tags_index edges
            ON edges.user_id = tags.user_id
            AND edges.tag_joplin_id = tags.joplin_id
        LEFT JOIN joplin_mcp.notes_index notes
            ON notes.user_id = tags.user_id
            AND notes.joplin_id = edges.note_joplin_id
        WHERE tags.user_id = $1
        GROUP BY tags.joplin_id, tags.title
        ORDER BY tags.title ASC, tags.joplin_id ASC
        "#,
    )
    .bind(scope.user_id())
    .fetch_all(pool)
    .await
    .map_err(|error| ToolError::internal("failed to list tags", error))?;

    Ok(json!({
        "index_status": index.index_status,
        "last_indexed_at": index.last_indexed_at,
        "tags": tags
    }))
}

pub async fn get_notebook_tree_tool(pool: &PgPool, scope: &UserScope) -> Result<Value, ToolError> {
    let index = ready_index_metadata(pool, scope).await?;
    let notebooks = fetch_notebook_rows(pool, scope).await?;
    Ok(json!({
        "index_status": index.index_status,
        "last_indexed_at": index.last_indexed_at,
        "notebooks": notebook_tree(&notebooks)
    }))
}

pub async fn get_notes_by_tag_tool(
    pool: &PgPool,
    scope: &UserScope,
    input: &Value,
    cursor_signing_key: &[u8],
) -> Result<Value, ToolError> {
    validate_tool_input("get_notes_by_tag", input)?;
    let index = ready_index_metadata(pool, scope).await?;
    let tag_id = input
        .get("tag_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::validation("get_notes_by_tag missing required field tag_id"))?;
    let limit = input.get("limit").and_then(Value::as_u64).unwrap_or(50) as i64;
    ensure_tag_visible(pool, scope, tag_id).await?;

    let filter = json!({ "tag_id": tag_id });
    let filter_hash = filter_hash(&filter);
    let cursor = input
        .get("cursor")
        .and_then(Value::as_str)
        .map(|cursor| decode_cursor(cursor, cursor_signing_key, "get_notes_by_tag", &filter_hash))
        .transpose()?;
    let after = cursor.as_ref().map(tag_cursor_values).transpose()?;
    let rows = fetch_notes_by_tag_rows(pool, scope, tag_id, limit + 1, after).await?;
    let has_more = rows.len() as i64 > limit;
    let notes: Vec<NoteSummary> = rows.into_iter().take(limit as usize).collect();
    let next_cursor = if has_more {
        notes
            .last()
            .map(|note| encode_notes_by_tag_cursor(note, &filter_hash, cursor_signing_key))
            .transpose()?
    } else {
        None
    };

    Ok(json!({
        "index_status": index.index_status,
        "last_indexed_at": index.last_indexed_at,
        "tag_id": tag_id,
        "notes": notes,
        "next_cursor": next_cursor
    }))
}

pub async fn list_notes_tool(
    pool: &PgPool,
    scope: &UserScope,
    input: &Value,
    cursor_signing_key: &[u8],
    max_response_bytes: usize,
) -> Result<Value, ToolError> {
    validate_tool_input("list_notes", input)?;
    let index = ready_index_metadata(pool, scope).await?;
    let request = NoteListRequest::from_input(input, 50);
    let filter_hash = filter_hash(&request.filter_value());
    let cursor = input
        .get("cursor")
        .and_then(Value::as_str)
        .map(|cursor| decode_cursor(cursor, cursor_signing_key, "list_notes", &filter_hash))
        .transpose()?;
    let after = cursor
        .as_ref()
        .map(updated_desc_cursor_values)
        .transpose()?;
    let rows = fetch_note_list_rows(pool, scope, &request, request.limit + 1, after).await?;
    let has_more = rows.len() as i64 > request.limit;
    let notes: Vec<NoteListItem> = rows
        .into_iter()
        .take(request.limit as usize)
        .map(NoteListItem::from)
        .collect();
    let next_cursor = if has_more {
        notes
            .last()
            .map(|note| {
                encode_updated_desc_cursor(
                    "list_notes",
                    note.updated_time.unwrap_or(0),
                    &note.joplin_id,
                    &filter_hash,
                    cursor_signing_key,
                )
            })
            .transpose()?
    } else {
        None
    };

    enforce_response_budget(
        json!({
            "index_status": index.index_status,
            "last_indexed_at": index.last_indexed_at,
            "notes": notes,
            "next_cursor": next_cursor
        }),
        max_response_bytes,
    )
}

pub async fn get_recent_notes_tool(
    pool: &PgPool,
    scope: &UserScope,
    input: &Value,
    cursor_signing_key: &[u8],
    max_response_bytes: usize,
) -> Result<Value, ToolError> {
    validate_tool_input("get_recent_notes", input)?;
    let index = ready_index_metadata(pool, scope).await?;
    let request = NoteListRequest::recent(input);
    let filter_hash = filter_hash(&request.filter_value());
    let cursor = input
        .get("cursor")
        .and_then(Value::as_str)
        .map(|cursor| decode_cursor(cursor, cursor_signing_key, "get_recent_notes", &filter_hash))
        .transpose()?;
    let after = cursor
        .as_ref()
        .map(updated_desc_cursor_values)
        .transpose()?;
    let rows = fetch_note_list_rows(pool, scope, &request, request.limit + 1, after).await?;
    let has_more = rows.len() as i64 > request.limit;
    let notes: Vec<NoteListItem> = rows
        .into_iter()
        .take(request.limit as usize)
        .map(NoteListItem::from)
        .collect();
    let next_cursor = if has_more {
        notes
            .last()
            .map(|note| {
                encode_updated_desc_cursor(
                    "get_recent_notes",
                    note.updated_time.unwrap_or(0),
                    &note.joplin_id,
                    &filter_hash,
                    cursor_signing_key,
                )
            })
            .transpose()?
    } else {
        None
    };

    enforce_response_budget(
        json!({
            "index_status": index.index_status,
            "last_indexed_at": index.last_indexed_at,
            "notes": notes,
            "next_cursor": next_cursor
        }),
        max_response_bytes,
    )
}

pub async fn search_notes_tool(
    pool: &PgPool,
    scope: &UserScope,
    input: &Value,
    cursor_signing_key: &[u8],
    text_search_config: &str,
    max_response_bytes: usize,
) -> Result<Value, ToolError> {
    validate_tool_input("search_notes", input)?;
    let index = ready_index_metadata(pool, scope).await?;
    let request = SearchNotesRequest::from_input(input);
    let filter_hash = filter_hash(&request.filter_value());
    let cursor = input
        .get("cursor")
        .and_then(Value::as_str)
        .map(|cursor| decode_cursor(cursor, cursor_signing_key, "search_notes", &filter_hash))
        .transpose()?;
    let after = cursor.as_ref().map(search_cursor_values).transpose()?;
    let mut rows = fetch_search_note_rows(
        pool,
        scope,
        &request,
        text_search_config,
        request.limit + 1,
        after.clone(),
        SearchMode::FullText,
    )
    .await?;
    let fallback_used =
        rows.is_empty() && request.query.chars().count() <= ILIKE_FALLBACK_MAX_QUERY_CHARS;
    if fallback_used {
        rows = fetch_search_note_rows(
            pool,
            scope,
            &request,
            text_search_config,
            request.limit + 1,
            after,
            SearchMode::IlikeFallback,
        )
        .await?;
    }
    let has_more = rows.len() as i64 > request.limit;
    let notes: Vec<SearchNoteItem> = rows
        .into_iter()
        .take(request.limit as usize)
        .map(|row| SearchNoteItem::from_row(row, request.limit_body_chars))
        .collect();
    let next_cursor = if has_more {
        notes
            .last()
            .map(|note| encode_search_cursor(note, &filter_hash, cursor_signing_key))
            .transpose()?
    } else {
        None
    };

    enforce_response_budget(
        json!({
            "index_status": index.index_status,
            "last_indexed_at": index.last_indexed_at,
            "fallback": if fallback_used { Some("ilike") } else { None },
            "notes": notes,
            "next_cursor": next_cursor
        }),
        max_response_bytes,
    )
}

pub async fn get_note_tool(
    pool: &PgPool,
    scope: &UserScope,
    input: &Value,
    cursor_signing_key: &[u8],
    max_response_bytes: usize,
) -> Result<Value, ToolError> {
    validate_tool_input("get_note", input)?;
    let index = ready_index_metadata(pool, scope).await?;
    let note_id = input
        .get("note_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::validation("get_note missing required field note_id"))?;
    let max_body_chars = input
        .get("max_body_chars")
        .and_then(Value::as_u64)
        .unwrap_or(8000) as usize;
    let row = fetch_note_body_row(pool, scope, note_id).await?;
    let indexed_note_version = row.indexed_note_version();
    let start_offset = input
        .get("body_cursor")
        .and_then(Value::as_str)
        .map(|cursor| {
            decode_body_cursor(cursor, cursor_signing_key, note_id, &indexed_note_version)
        })
        .transpose()?
        .unwrap_or(0);
    let body_text = row.body_text.clone().unwrap_or_default();
    let body = truncate_text_at_chars(&body_text, start_offset, max_body_chars);
    let next_cursor = if body.truncated {
        body.next_offset
            .map(|offset| {
                encode_body_cursor(note_id, &indexed_note_version, offset, cursor_signing_key)
            })
            .transpose()?
    } else {
        None
    };

    enforce_response_budget(
        json!({
            "index_status": index.index_status,
            "last_indexed_at": index.last_indexed_at,
            "id": row.joplin_id,
            "title": row.title,
            "notebook_id": row.notebook_id,
            "updated_time": row.updated_time,
            "body": body.value,
            "resource_ref_count": row.resource_refs.len(),
            "truncated": body.truncated,
            "next_cursor": next_cursor,
        }),
        max_response_bytes,
    )
}

pub async fn get_note_excerpt_tool(
    pool: &PgPool,
    scope: &UserScope,
    input: &Value,
    max_response_bytes: usize,
) -> Result<Value, ToolError> {
    validate_tool_input("get_note_excerpt", input)?;
    let index = ready_index_metadata(pool, scope).await?;
    let note_id = input
        .get("note_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::validation("get_note_excerpt missing required field note_id"))?;
    let max_chars = input
        .get("max_chars")
        .and_then(Value::as_u64)
        .unwrap_or(2000) as usize;
    let row = fetch_note_body_row(pool, scope, note_id).await?;
    let body_text = row.body_text.clone().unwrap_or_default();
    let excerpt = truncate_text_at_chars(&body_text, 0, max_chars);

    enforce_response_budget(
        json!({
            "index_status": index.index_status,
            "last_indexed_at": index.last_indexed_at,
            "id": row.joplin_id,
            "title": row.title,
            "notebook_id": row.notebook_id,
            "updated_time": row.updated_time,
            "excerpt": excerpt.value,
            "truncated": excerpt.truncated,
        }),
        max_response_bytes,
    )
}

pub async fn get_changes_since_tool(
    pool: &PgPool,
    scope: &UserScope,
    input: &Value,
    cursor_signing_key: &[u8],
    max_response_bytes: usize,
) -> Result<Value, ToolError> {
    validate_tool_input("get_changes_since", input)?;
    let index = ready_index_metadata(pool, scope).await?;
    let since = input
        .get("since")
        .and_then(Value::as_i64)
        .ok_or_else(|| ToolError::validation("get_changes_since missing required field since"))?;
    let limit = input.get("limit").and_then(Value::as_u64).unwrap_or(100) as i64;

    let filter = json!({ "since": since });
    let filter_hash = filter_hash(&filter);
    let cursor = input
        .get("cursor")
        .and_then(Value::as_str)
        .map(|cursor| {
            decode_cursor(
                cursor,
                cursor_signing_key,
                "get_changes_since",
                &filter_hash,
            )
        })
        .transpose()?;
    let after = cursor.as_ref().map(changes_cursor_values).transpose()?;
    let rows = fetch_changes_since_rows(pool, scope, since, limit + 1, after).await?;
    let has_more = rows.len() as i64 > limit;
    let notes: Vec<NoteChange> = rows.into_iter().take(limit as usize).collect();
    let next_cursor = if has_more {
        notes
            .last()
            .map(|note| encode_changes_since_cursor(note, &filter_hash, cursor_signing_key))
            .transpose()?
    } else {
        None
    };

    enforce_response_budget(
        json!({
            "index_status": index.index_status,
            "last_indexed_at": index.last_indexed_at,
            "since": since,
            "notes": notes,
            "next_cursor": next_cursor
        }),
        max_response_bytes,
    )
}

pub async fn get_note_resources_tool(
    pool: &PgPool,
    scope: &UserScope,
    input: &Value,
    max_response_bytes: usize,
) -> Result<Value, ToolError> {
    validate_tool_input("get_note_resources", input)?;
    let index = ready_index_metadata(pool, scope).await?;
    let note_id = input
        .get("note_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ToolError::validation("get_note_resources missing required field note_id")
        })?;
    let resource_refs = fetch_note_resource_refs(pool, scope, note_id).await?;
    let rows = fetch_resource_rows(pool, scope, &resource_refs).await?;
    let resources = resource_items(&resource_refs, rows);

    enforce_response_budget(
        json!({
            "index_status": index.index_status,
            "last_indexed_at": index.last_indexed_at,
            "note_id": note_id,
            "binary_download_supported": false,
            "binary_download": "unsupported_in_v1",
            "resources": resources
        }),
        max_response_bytes,
    )
}

async fn ready_index_metadata(
    pool: &PgPool,
    scope: &UserScope,
) -> Result<IndexMetadata, ToolError> {
    let snapshot = sqlx::query_as::<_, IndexStateRow>(
        r#"
        SELECT status, last_full_rebuild_at, last_incremental_at, updated_at
        FROM joplin_mcp.index_state
        WHERE user_id = $1
        "#,
    )
    .bind(scope.user_id())
    .fetch_optional(pool)
    .await
    .map_err(|error| ToolError::internal("failed to read index state", error))?
    .map(IndexStateRow::snapshot)
    .transpose()?;

    gate_index_state(snapshot.as_ref())
}

async fn fetch_notebook_rows(
    pool: &PgPool,
    scope: &UserScope,
) -> Result<Vec<NotebookRow>, ToolError> {
    sqlx::query_as::<_, NotebookRow>(
        r#"
        SELECT
            notebooks.joplin_id,
            notebooks.parent_joplin_id,
            notebooks.title,
            COUNT(notes.joplin_id)::bigint AS note_count
        FROM joplin_mcp.notebooks_index notebooks
        LEFT JOIN joplin_mcp.notes_index notes
            ON notes.user_id = notebooks.user_id
            AND notes.parent_joplin_id = notebooks.joplin_id
        WHERE notebooks.user_id = $1
        GROUP BY notebooks.joplin_id, notebooks.parent_joplin_id, notebooks.title
        ORDER BY notebooks.title ASC, notebooks.joplin_id ASC
        "#,
    )
    .bind(scope.user_id())
    .fetch_all(pool)
    .await
    .map_err(|error| ToolError::internal("failed to list notebooks", error))
}

async fn ensure_tag_visible(
    pool: &PgPool,
    scope: &UserScope,
    tag_id: &str,
) -> Result<(), ToolError> {
    let exists = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM joplin_mcp.tags_index
            WHERE user_id = $1 AND joplin_id = $2
        )
        "#,
    )
    .bind(scope.user_id())
    .bind(tag_id)
    .fetch_one(pool)
    .await
    .map_err(|error| ToolError::internal("failed to read tag", error))?;

    if exists {
        Ok(())
    } else {
        Err(ToolError::not_found("tag not found"))
    }
}

async fn fetch_notes_by_tag_rows(
    pool: &PgPool,
    scope: &UserScope,
    tag_id: &str,
    limit: i64,
    after: Option<(i64, String)>,
) -> Result<Vec<NoteSummary>, ToolError> {
    let rows = sqlx::query_as::<_, NoteSummary>(
        r#"
        SELECT
            notes.joplin_id,
            notes.title,
            notes.parent_joplin_id AS notebook_id,
            notes.updated_time
        FROM joplin_mcp.note_tags_index edges
        JOIN joplin_mcp.notes_index notes
            ON notes.user_id = edges.user_id
            AND notes.joplin_id = edges.note_joplin_id
        WHERE edges.user_id = $1
            AND edges.tag_joplin_id = $2
            AND (
                $3::bigint IS NULL
                OR COALESCE(notes.updated_time, 0) < $3
                OR (COALESCE(notes.updated_time, 0) = $3 AND notes.joplin_id < $4)
            )
        ORDER BY COALESCE(notes.updated_time, 0) DESC, notes.joplin_id DESC
        LIMIT $5
        "#,
    )
    .bind(scope.user_id())
    .bind(tag_id)
    .bind(after.as_ref().map(|(updated_time, _)| *updated_time))
    .bind(after.as_ref().map(|(_, joplin_id)| joplin_id.as_str()))
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|error| ToolError::internal("failed to list notes by tag", error))?;

    Ok(rows)
}

async fn fetch_note_list_rows(
    pool: &PgPool,
    scope: &UserScope,
    request: &NoteListRequest,
    limit: i64,
    after: Option<(i64, String)>,
) -> Result<Vec<NoteListRow>, ToolError> {
    sqlx::query_as::<_, NoteListRow>(
        r#"
        SELECT
            notes.joplin_id,
            notes.title,
            notes.parent_joplin_id AS notebook_id,
            notes.updated_time,
            notes.body_text
        FROM joplin_mcp.notes_index notes
        WHERE notes.user_id = $1
            AND notes.deleted_time IS NULL
            AND ($2::text IS NULL OR notes.parent_joplin_id = $2)
            AND ($3::bigint IS NULL OR notes.updated_time >= $3)
            AND ($4::bigint IS NULL OR notes.updated_time <= $4)
            AND ($5::boolean IS NULL OR notes.is_todo = $5)
            AND (
                cardinality($6::text[]) = 0
                OR NOT EXISTS (
                    SELECT 1
                    FROM unnest($6::text[]) AS required(tag_id)
                    WHERE NOT EXISTS (
                        SELECT 1
                        FROM joplin_mcp.note_tags_index edges
                        WHERE edges.user_id = notes.user_id
                            AND edges.note_joplin_id = notes.joplin_id
                            AND edges.tag_joplin_id = required.tag_id
                    )
                )
            )
            AND (
                $7::bigint IS NULL
                OR COALESCE(notes.updated_time, 0) < $7
                OR (COALESCE(notes.updated_time, 0) = $7 AND notes.joplin_id < $8)
            )
        ORDER BY COALESCE(notes.updated_time, 0) DESC, notes.joplin_id DESC
        LIMIT $9
        "#,
    )
    .bind(scope.user_id())
    .bind(request.notebook_id.as_deref())
    .bind(request.updated_after)
    .bind(request.updated_before)
    .bind(request.is_todo)
    .bind(&request.tag_ids)
    .bind(after.as_ref().map(|(updated_time, _)| *updated_time))
    .bind(after.as_ref().map(|(_, joplin_id)| joplin_id.as_str()))
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|error| ToolError::internal("failed to list notes", error))
}

async fn fetch_search_note_rows(
    pool: &PgPool,
    scope: &UserScope,
    request: &SearchNotesRequest,
    text_search_config: &str,
    limit: i64,
    after: Option<(f32, i64, String)>,
    mode: SearchMode,
) -> Result<Vec<SearchNoteRow>, ToolError> {
    let query = match mode {
        SearchMode::FullText => {
            r#"
            SELECT
                notes.joplin_id,
                notes.title,
                notes.parent_joplin_id AS notebook_id,
                notes.updated_time,
                notes.body_text,
                ts_rank(notes.search_vector, plainto_tsquery($2::regconfig, $3)) AS rank
            FROM joplin_mcp.notes_index notes
            WHERE notes.user_id = $1
                AND notes.deleted_time IS NULL
                AND notes.search_vector @@ plainto_tsquery($2::regconfig, $3)
                AND ($4::text IS NULL OR notes.parent_joplin_id = $4)
                AND ($5::bigint IS NULL OR notes.updated_time >= $5)
                AND ($6::bigint IS NULL OR notes.updated_time <= $6)
                AND ($7::boolean IS NULL OR notes.is_todo = $7)
                AND (
                    cardinality($8::text[]) = 0
                    OR NOT EXISTS (
                        SELECT 1
                        FROM unnest($8::text[]) AS required(tag_id)
                        WHERE NOT EXISTS (
                            SELECT 1
                            FROM joplin_mcp.note_tags_index edges
                            WHERE edges.user_id = notes.user_id
                                AND edges.note_joplin_id = notes.joplin_id
                                AND edges.tag_joplin_id = required.tag_id
                        )
                    )
                )
                AND (
                    $9::real IS NULL
                    OR ts_rank(notes.search_vector, plainto_tsquery($2::regconfig, $3)) < $9
                    OR (
                        ts_rank(notes.search_vector, plainto_tsquery($2::regconfig, $3)) = $9
                        AND COALESCE(notes.updated_time, 0) < $10
                    )
                    OR (
                        ts_rank(notes.search_vector, plainto_tsquery($2::regconfig, $3)) = $9
                        AND COALESCE(notes.updated_time, 0) = $10
                        AND notes.joplin_id < $11
                    )
                )
            ORDER BY rank DESC, COALESCE(notes.updated_time, 0) DESC, notes.joplin_id DESC
            LIMIT $12
            "#
        }
        SearchMode::IlikeFallback => {
            r#"
            SELECT
                notes.joplin_id,
                notes.title,
                notes.parent_joplin_id AS notebook_id,
                notes.updated_time,
                notes.body_text,
                0::real AS rank
            FROM joplin_mcp.notes_index notes
            WHERE notes.user_id = $1
                AND notes.deleted_time IS NULL
                AND (notes.title ILIKE '%' || $3 || '%' OR notes.body_text ILIKE '%' || $3 || '%')
                AND ($4::text IS NULL OR notes.parent_joplin_id = $4)
                AND ($5::bigint IS NULL OR notes.updated_time >= $5)
                AND ($6::bigint IS NULL OR notes.updated_time <= $6)
                AND ($7::boolean IS NULL OR notes.is_todo = $7)
                AND (
                    cardinality($8::text[]) = 0
                    OR NOT EXISTS (
                        SELECT 1
                        FROM unnest($8::text[]) AS required(tag_id)
                        WHERE NOT EXISTS (
                            SELECT 1
                            FROM joplin_mcp.note_tags_index edges
                            WHERE edges.user_id = notes.user_id
                                AND edges.note_joplin_id = notes.joplin_id
                                AND edges.tag_joplin_id = required.tag_id
                        )
                    )
                )
                AND (
                    $9::real IS NULL
                    OR 0::real < $9
                    OR (0::real = $9 AND COALESCE(notes.updated_time, 0) < $10)
                    OR (0::real = $9 AND COALESCE(notes.updated_time, 0) = $10 AND notes.joplin_id < $11)
                )
            ORDER BY rank DESC, COALESCE(notes.updated_time, 0) DESC, notes.joplin_id DESC
            LIMIT $12
            "#
        }
    };

    sqlx::query_as::<_, SearchNoteRow>(query)
        .bind(scope.user_id())
        .bind(text_search_config)
        .bind(&request.query)
        .bind(request.notebook_id.as_deref())
        .bind(request.updated_after)
        .bind(request.updated_before)
        .bind(request.is_todo)
        .bind(&request.tag_ids)
        .bind(after.as_ref().map(|(rank, _, _)| *rank))
        .bind(after.as_ref().map(|(_, updated_time, _)| *updated_time))
        .bind(after.as_ref().map(|(_, _, joplin_id)| joplin_id.as_str()))
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(|error| ToolError::internal("failed to search notes", error))
}

async fn fetch_note_body_row(
    pool: &PgPool,
    scope: &UserScope,
    note_id: &str,
) -> Result<NoteBodyRow, ToolError> {
    sqlx::query_as::<_, NoteBodyRow>(
        r#"
        SELECT
            notes.joplin_id,
            notes.title,
            notes.parent_joplin_id AS notebook_id,
            notes.updated_time,
            notes.body_text,
            notes.resource_refs
        FROM joplin_mcp.notes_index notes
        WHERE notes.user_id = $1
            AND notes.joplin_id = $2
            AND notes.deleted_time IS NULL
        "#,
    )
    .bind(scope.user_id())
    .bind(note_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| ToolError::internal("failed to read note", error))?
    .ok_or_else(|| ToolError::not_found("note not found"))
}

async fn fetch_changes_since_rows(
    pool: &PgPool,
    scope: &UserScope,
    since: i64,
    limit: i64,
    after: Option<(i64, String)>,
) -> Result<Vec<NoteChange>, ToolError> {
    sqlx::query_as::<_, NoteChange>(
        r#"
        SELECT
            notes.joplin_id,
            notes.title,
            notes.parent_joplin_id AS notebook_id,
            notes.updated_time
        FROM joplin_mcp.notes_index notes
        WHERE notes.user_id = $1
            AND notes.deleted_time IS NULL
            AND notes.updated_time > $2
            AND (
                $3::bigint IS NULL
                OR notes.updated_time > $3
                OR (notes.updated_time = $3 AND notes.joplin_id > $4)
            )
        ORDER BY notes.updated_time ASC, notes.joplin_id ASC
        LIMIT $5
        "#,
    )
    .bind(scope.user_id())
    .bind(since)
    .bind(after.as_ref().map(|(updated_time, _)| *updated_time))
    .bind(after.as_ref().map(|(_, joplin_id)| joplin_id.as_str()))
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|error| ToolError::internal("failed to get changes since timestamp", error))
}

async fn fetch_note_resource_refs(
    pool: &PgPool,
    scope: &UserScope,
    note_id: &str,
) -> Result<Vec<String>, ToolError> {
    sqlx::query_scalar::<_, Vec<String>>(
        r#"
        SELECT resource_refs
        FROM joplin_mcp.notes_index
        WHERE user_id = $1
            AND joplin_id = $2
            AND deleted_time IS NULL
        "#,
    )
    .bind(scope.user_id())
    .bind(note_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| ToolError::internal("failed to read note resources", error))?
    .ok_or_else(|| ToolError::not_found("note not found"))
}

async fn fetch_resource_rows(
    pool: &PgPool,
    scope: &UserScope,
    resource_refs: &[String],
) -> Result<Vec<ResourceRow>, ToolError> {
    if resource_refs.is_empty() {
        return Ok(Vec::new());
    }

    sqlx::query_as::<_, ResourceRow>(
        r#"
        SELECT
            joplin_id,
            title,
            mime,
            size_bytes,
            file_extension,
            updated_time
        FROM joplin_mcp.resources_index
        WHERE user_id = $1
            AND joplin_id = ANY($2)
        "#,
    )
    .bind(scope.user_id())
    .bind(resource_refs)
    .fetch_all(pool)
    .await
    .map_err(|error| ToolError::internal("failed to read resource metadata", error))
}

fn tag_cursor_values(payload: &CursorPayload) -> Result<(i64, String), ToolError> {
    if payload.sort_keys != ["updated_time", "joplin_id"] || payload.last_values.len() != 2 {
        return Err(ToolError::validation("invalid get_notes_by_tag cursor"));
    }
    let updated_time = payload.last_values[0]
        .as_i64()
        .ok_or_else(|| ToolError::validation("invalid get_notes_by_tag cursor updated_time"))?;
    let joplin_id = payload.last_values[1]
        .as_str()
        .ok_or_else(|| ToolError::validation("invalid get_notes_by_tag cursor joplin_id"))?;
    Ok((updated_time, joplin_id.to_string()))
}

fn encode_notes_by_tag_cursor(
    note: &NoteSummary,
    filter_hash: &str,
    signing_key: &[u8],
) -> Result<String, ToolError> {
    encode_cursor(
        &CursorPayload {
            version: CURSOR_VERSION,
            tool: "get_notes_by_tag".to_string(),
            filter_hash: filter_hash.to_string(),
            sort_keys: vec!["updated_time".to_string(), "joplin_id".to_string()],
            last_values: vec![json!(note.updated_time.unwrap_or(0)), json!(note.joplin_id)],
            body: None,
        },
        signing_key,
    )
}

fn updated_desc_cursor_values(payload: &CursorPayload) -> Result<(i64, String), ToolError> {
    if payload.sort_keys != ["updated_time", "joplin_id"] || payload.last_values.len() != 2 {
        return Err(ToolError::validation("invalid note list cursor"));
    }
    let updated_time = payload.last_values[0]
        .as_i64()
        .ok_or_else(|| ToolError::validation("invalid note list cursor updated_time"))?;
    let joplin_id = payload.last_values[1]
        .as_str()
        .ok_or_else(|| ToolError::validation("invalid note list cursor joplin_id"))?;
    Ok((updated_time, joplin_id.to_string()))
}

fn encode_updated_desc_cursor(
    tool: &str,
    updated_time: i64,
    joplin_id: &str,
    filter_hash: &str,
    signing_key: &[u8],
) -> Result<String, ToolError> {
    encode_cursor(
        &CursorPayload {
            version: CURSOR_VERSION,
            tool: tool.to_string(),
            filter_hash: filter_hash.to_string(),
            sort_keys: vec!["updated_time".to_string(), "joplin_id".to_string()],
            last_values: vec![json!(updated_time), json!(joplin_id)],
            body: None,
        },
        signing_key,
    )
}

fn search_cursor_values(payload: &CursorPayload) -> Result<(f32, i64, String), ToolError> {
    if payload.sort_keys != ["rank", "updated_time", "joplin_id"] || payload.last_values.len() != 3
    {
        return Err(ToolError::validation("invalid search_notes cursor"));
    }
    let rank = payload.last_values[0]
        .as_f64()
        .ok_or_else(|| ToolError::validation("invalid search_notes cursor rank"))?
        as f32;
    let updated_time = payload.last_values[1]
        .as_i64()
        .ok_or_else(|| ToolError::validation("invalid search_notes cursor updated_time"))?;
    let joplin_id = payload.last_values[2]
        .as_str()
        .ok_or_else(|| ToolError::validation("invalid search_notes cursor joplin_id"))?;
    Ok((rank, updated_time, joplin_id.to_string()))
}

fn encode_search_cursor(
    note: &SearchNoteItem,
    filter_hash: &str,
    signing_key: &[u8],
) -> Result<String, ToolError> {
    encode_cursor(
        &CursorPayload {
            version: CURSOR_VERSION,
            tool: "search_notes".to_string(),
            filter_hash: filter_hash.to_string(),
            sort_keys: vec![
                "rank".to_string(),
                "updated_time".to_string(),
                "joplin_id".to_string(),
            ],
            last_values: vec![
                json!(note.rank),
                json!(note.updated_time.unwrap_or(0)),
                json!(note.joplin_id),
            ],
            body: None,
        },
        signing_key,
    )
}

fn decode_body_cursor(
    cursor: &str,
    signing_key: &[u8],
    note_id: &str,
    indexed_note_version: &str,
) -> Result<usize, ToolError> {
    let filter_hash = filter_hash(&json!({ "note_id": note_id }));
    let payload = decode_cursor(cursor, signing_key, "get_note", &filter_hash)?;
    let body = payload
        .body
        .ok_or_else(|| ToolError::validation("invalid get_note body cursor"))?;
    if body.note_id != note_id {
        return Err(ToolError::validation("body cursor does not match note"));
    }
    if body.indexed_note_version != indexed_note_version {
        return Err(ToolError::validation("body cursor is stale"));
    }
    Ok(body.next_body_offset)
}

fn encode_body_cursor(
    note_id: &str,
    indexed_note_version: &str,
    next_body_offset: usize,
    signing_key: &[u8],
) -> Result<String, ToolError> {
    encode_cursor(
        &CursorPayload {
            version: CURSOR_VERSION,
            tool: "get_note".to_string(),
            filter_hash: filter_hash(&json!({ "note_id": note_id })),
            sort_keys: Vec::new(),
            last_values: Vec::new(),
            body: Some(BodyCursorPayload {
                note_id: note_id.to_string(),
                indexed_note_version: indexed_note_version.to_string(),
                next_body_offset,
            }),
        },
        signing_key,
    )
}

fn changes_cursor_values(payload: &CursorPayload) -> Result<(i64, String), ToolError> {
    if payload.sort_keys != ["updated_time", "joplin_id"] || payload.last_values.len() != 2 {
        return Err(ToolError::validation("invalid get_changes_since cursor"));
    }
    let updated_time = payload.last_values[0]
        .as_i64()
        .ok_or_else(|| ToolError::validation("invalid get_changes_since cursor updated_time"))?;
    let joplin_id = payload.last_values[1]
        .as_str()
        .ok_or_else(|| ToolError::validation("invalid get_changes_since cursor joplin_id"))?;
    Ok((updated_time, joplin_id.to_string()))
}

fn encode_changes_since_cursor(
    note: &NoteChange,
    filter_hash: &str,
    signing_key: &[u8],
) -> Result<String, ToolError> {
    encode_cursor(
        &CursorPayload {
            version: CURSOR_VERSION,
            tool: "get_changes_since".to_string(),
            filter_hash: filter_hash.to_string(),
            sort_keys: vec!["updated_time".to_string(), "joplin_id".to_string()],
            last_values: vec![json!(note.updated_time), json!(note.joplin_id)],
            body: None,
        },
        signing_key,
    )
}

fn resource_items(resource_refs: &[String], rows: Vec<ResourceRow>) -> Vec<ResourceMetadata> {
    let mut rows_by_id: BTreeMap<String, ResourceRow> = rows
        .into_iter()
        .map(|row| (row.joplin_id.clone(), row))
        .collect();

    resource_refs
        .iter()
        .filter_map(|resource_id| rows_by_id.remove(resource_id))
        .map(ResourceMetadata::from)
        .collect()
}

fn notebook_items(rows: &[NotebookRow]) -> Vec<NotebookItem> {
    rows.iter()
        .map(|row| NotebookItem {
            id: row.joplin_id.clone(),
            title: row.title.clone(),
            parent_id: row.parent_joplin_id.clone(),
            note_count: row.note_count,
        })
        .collect()
}

fn notebook_tree(rows: &[NotebookRow]) -> Vec<NotebookTreeNode> {
    let ids: BTreeSet<&str> = rows.iter().map(|row| row.joplin_id.as_str()).collect();
    let children = rows.iter().fold(
        BTreeMap::<Option<&str>, Vec<&NotebookRow>>::new(),
        |mut map, row| {
            let parent = row
                .parent_joplin_id
                .as_deref()
                .filter(|parent| ids.contains(parent));
            map.entry(parent).or_default().push(row);
            map
        },
    );

    build_notebook_tree(None, &children)
}

fn build_notebook_tree(
    parent_id: Option<&str>,
    children: &BTreeMap<Option<&str>, Vec<&NotebookRow>>,
) -> Vec<NotebookTreeNode> {
    children
        .get(&parent_id)
        .into_iter()
        .flatten()
        .map(|row| NotebookTreeNode {
            id: row.joplin_id.clone(),
            title: row.title.clone(),
            note_count: row.note_count,
            children: build_notebook_tree(Some(&row.joplin_id), children),
        })
        .collect()
}

#[derive(Debug, FromRow)]
struct StatusRow {
    joplin_email: String,
    status: Option<String>,
    last_full_rebuild_at: Option<DateTime<Utc>>,
    last_incremental_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
}

impl StatusRow {
    fn index_metadata(&self) -> Result<IndexMetadata, ToolError> {
        let Some(status) = self.status.as_deref() else {
            return Ok(IndexMetadata {
                index_status: IndexStatus::Missing,
                last_indexed_at: None,
            });
        };
        let snapshot = IndexStateSnapshot {
            status: IndexStatus::from_db(status)?,
            last_full_rebuild_at: self.last_full_rebuild_at,
            last_incremental_at: self.last_incremental_at,
            updated_at: self.updated_at,
        };
        Ok(IndexMetadata {
            index_status: snapshot.status,
            last_indexed_at: snapshot
                .last_incremental_at
                .or(snapshot.last_full_rebuild_at)
                .or(snapshot.updated_at),
        })
    }
}

#[derive(Debug, FromRow)]
struct IndexStateRow {
    status: String,
    last_full_rebuild_at: Option<DateTime<Utc>>,
    last_incremental_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
}

impl IndexStateRow {
    fn snapshot(self) -> Result<IndexStateSnapshot, ToolError> {
        Ok(IndexStateSnapshot {
            status: IndexStatus::from_db(&self.status)?,
            last_full_rebuild_at: self.last_full_rebuild_at,
            last_incremental_at: self.last_incremental_at,
            updated_at: self.updated_at,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
struct NotebookRow {
    joplin_id: String,
    parent_joplin_id: Option<String>,
    title: String,
    note_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct NotebookItem {
    id: String,
    title: String,
    parent_id: Option<String>,
    note_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct NotebookTreeNode {
    id: String,
    title: String,
    note_count: i64,
    children: Vec<NotebookTreeNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow)]
struct TagRow {
    #[serde(rename = "id")]
    joplin_id: String,
    title: String,
    note_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow)]
struct NoteSummary {
    #[serde(rename = "id")]
    joplin_id: String,
    title: String,
    notebook_id: Option<String>,
    updated_time: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NoteListRequest {
    notebook_id: Option<String>,
    tag_ids: Vec<String>,
    updated_after: Option<i64>,
    updated_before: Option<i64>,
    is_todo: Option<bool>,
    limit: i64,
}

impl NoteListRequest {
    fn from_input(input: &Value, default_limit: i64) -> Self {
        Self {
            notebook_id: input
                .get("notebook_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            tag_ids: input
                .get("tag_ids")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            updated_after: input.get("updated_after").and_then(Value::as_i64),
            updated_before: input.get("updated_before").and_then(Value::as_i64),
            is_todo: input.get("is_todo").and_then(Value::as_bool),
            limit: input
                .get("limit")
                .and_then(Value::as_i64)
                .unwrap_or(default_limit),
        }
    }

    fn recent(input: &Value) -> Self {
        Self {
            notebook_id: None,
            tag_ids: Vec::new(),
            updated_after: None,
            updated_before: None,
            is_todo: None,
            limit: input.get("limit").and_then(Value::as_i64).unwrap_or(20),
        }
    }

    fn filter_value(&self) -> Value {
        json!({
            "notebook_id": self.notebook_id,
            "tag_ids": self.tag_ids,
            "updated_after": self.updated_after,
            "updated_before": self.updated_before,
            "is_todo": self.is_todo,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
struct NoteListRow {
    joplin_id: String,
    title: String,
    notebook_id: Option<String>,
    updated_time: Option<i64>,
    body_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct NoteListItem {
    #[serde(rename = "id")]
    joplin_id: String,
    title: String,
    notebook_id: Option<String>,
    updated_time: Option<i64>,
    preview: String,
}

impl From<NoteListRow> for NoteListItem {
    fn from(row: NoteListRow) -> Self {
        let body_text = row.body_text.unwrap_or_default();
        Self {
            joplin_id: row.joplin_id,
            title: row.title,
            notebook_id: row.notebook_id,
            updated_time: row.updated_time,
            preview: truncate_text_at_chars(&body_text, 0, NOTE_PREVIEW_CHARS).value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchNotesRequest {
    query: String,
    notebook_id: Option<String>,
    tag_ids: Vec<String>,
    updated_after: Option<i64>,
    updated_before: Option<i64>,
    is_todo: Option<bool>,
    limit: i64,
    limit_body_chars: usize,
}

impl SearchNotesRequest {
    fn from_input(input: &Value) -> Self {
        Self {
            query: input
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            notebook_id: input
                .get("notebook_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            tag_ids: input
                .get("tag_ids")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            updated_after: input.get("updated_after").and_then(Value::as_i64),
            updated_before: input.get("updated_before").and_then(Value::as_i64),
            is_todo: input.get("is_todo").and_then(Value::as_bool),
            limit: input.get("limit").and_then(Value::as_i64).unwrap_or(20),
            limit_body_chars: input
                .get("limit_body_chars")
                .and_then(Value::as_u64)
                .unwrap_or(8000) as usize,
        }
    }

    fn filter_value(&self) -> Value {
        json!({
            "query": self.query,
            "notebook_id": self.notebook_id,
            "tag_ids": self.tag_ids,
            "updated_after": self.updated_after,
            "updated_before": self.updated_before,
            "is_todo": self.is_todo,
            "limit_body_chars": self.limit_body_chars,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchMode {
    FullText,
    IlikeFallback,
}

#[derive(Debug, Clone, PartialEq, FromRow)]
struct SearchNoteRow {
    joplin_id: String,
    title: String,
    notebook_id: Option<String>,
    updated_time: Option<i64>,
    body_text: Option<String>,
    rank: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct SearchNoteItem {
    #[serde(rename = "id")]
    joplin_id: String,
    title: String,
    notebook_id: Option<String>,
    updated_time: Option<i64>,
    rank: f32,
    preview: String,
    truncated: bool,
}

impl SearchNoteItem {
    fn from_row(row: SearchNoteRow, limit_body_chars: usize) -> Self {
        let body_text = row.body_text.unwrap_or_default();
        let preview = truncate_text_at_chars(&body_text, 0, limit_body_chars);
        Self {
            joplin_id: row.joplin_id,
            title: row.title,
            notebook_id: row.notebook_id,
            updated_time: row.updated_time,
            rank: row.rank,
            preview: preview.value,
            truncated: preview.truncated,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
struct NoteBodyRow {
    joplin_id: String,
    title: String,
    notebook_id: Option<String>,
    updated_time: Option<i64>,
    body_text: Option<String>,
    resource_refs: Vec<String>,
}

impl NoteBodyRow {
    fn indexed_note_version(&self) -> String {
        format!("{}:{}", self.updated_time.unwrap_or(0), self.joplin_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow)]
struct NoteChange {
    #[serde(rename = "id")]
    joplin_id: String,
    title: String,
    notebook_id: Option<String>,
    updated_time: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
struct ResourceRow {
    joplin_id: String,
    title: String,
    mime: Option<String>,
    size_bytes: Option<i64>,
    file_extension: Option<String>,
    updated_time: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ResourceMetadata {
    #[serde(rename = "id")]
    joplin_id: String,
    title: String,
    mime: Option<String>,
    size_bytes: Option<i64>,
    file_extension: Option<String>,
    updated_time: Option<i64>,
}

impl From<ResourceRow> for ResourceMetadata {
    fn from(row: ResourceRow) -> Self {
        Self {
            joplin_id: row.joplin_id,
            title: row.title,
            mime: row.mime,
            size_bytes: row.size_bytes,
            file_extension: row.file_extension,
            updated_time: row.updated_time,
        }
    }
}

fn empty_schema() -> Value {
    json!({"type": "object", "properties": {}, "additionalProperties": false})
}

fn joplin_id_schema() -> Value {
    json!({"type": "string", "pattern": "^[0-9A-Fa-f]{32}$"})
}

fn joplin_id_array_schema() -> Value {
    json!({"type": "array", "items": joplin_id_schema(), "maxItems": 20})
}

fn unix_ms_schema() -> Value {
    json!({"type": "integer", "minimum": 0})
}

fn limit_schema(minimum: u64, maximum: u64, default: u64) -> Value {
    json!({"type": "integer", "minimum": minimum, "maximum": maximum, "default": default})
}

fn cursor_schema() -> Value {
    json!({"type": "string", "minLength": 1, "maxLength": 4096})
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_ID: &str = "0123456789abcdef0123456789abcdef";
    const CURSOR_KEY: &[u8] = b"cursor signing key for tests";

    #[test]
    fn every_tool_has_input_schema_with_closed_inputs() {
        for tool in tool_definitions() {
            assert!(tool.input_schema.get("type").is_some(), "{}", tool.name);
            assert_eq!(
                tool.input_schema.get("additionalProperties"),
                Some(&Value::Bool(false)),
                "{}",
                tool.name
            );
        }
    }

    #[test]
    fn tools_list_advertises_registry_schemas() {
        let response = tools_list_response();
        let tools = response["tools"].as_array().expect("tools array");
        assert!(tools.iter().any(|tool| tool["name"] == "search_notes"));
        assert!(
            tools
                .iter()
                .all(|tool| tool.get("inputSchema").is_some_and(Value::is_object))
        );
    }

    #[test]
    fn unknown_input_fields_fail_closed() {
        let error = validate_tool_input("search_notes", &json!({"query": "hello", "extra": true}))
            .expect_err("unknown field rejected");
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.message.contains("unknown field extra"));
    }

    #[test]
    fn required_and_limited_inputs_are_validated() {
        validate_tool_input(
            "search_notes",
            &json!({"query": "hello", "limit": 100, "tag_ids": [VALID_ID]}),
        )
        .expect("valid search input");

        let missing =
            validate_tool_input("search_notes", &json!({})).expect_err("query is required");
        assert!(missing.message.contains("missing required field query"));

        let too_large =
            validate_tool_input("search_notes", &json!({"query": "hello", "limit": 101}))
                .expect_err("limit cap enforced");
        assert!(too_large.message.contains("exceeds maximum"));

        let bad_id = validate_tool_input("get_note", &json!({"note_id": "not-a-joplin-id"}))
            .expect_err("joplin id enforced");
        assert!(bad_id.message.contains("32-character Joplin ID"));
    }

    #[test]
    fn response_budget_accepts_small_and_rejects_oversized() {
        enforce_response_budget(json!({"ok": true}), 32).expect("small response accepted");
        let error = enforce_response_budget(json!({"body": "too large"}), 8)
            .expect_err("large response rejected");
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.data.expect("data")["actual_response_bytes"].as_u64() > Some(8));
    }

    #[test]
    fn truncation_reports_next_byte_offset_at_char_boundary() {
        let truncated = truncate_text_at_chars("aébc", 0, 2);
        assert_eq!(truncated.value, "aé");
        assert!(truncated.truncated);
        assert_eq!(truncated.next_offset, Some(3));

        let resumed = truncate_text_at_chars("aébc", 3, 10);
        assert_eq!(resumed.value, "bc");
        assert!(!resumed.truncated);
    }

    #[test]
    fn index_gate_rejects_missing_or_rebuilding_and_allows_stale_metadata() {
        let missing = gate_index_state(None).expect_err("missing index rejected");
        assert_eq!(missing.code, ErrorCode::IndexNotReady);
        assert_eq!(missing.data.expect("data")["index_status"], "missing");

        let rebuilding = IndexStateSnapshot {
            status: IndexStatus::Rebuilding,
            last_full_rebuild_at: None,
            last_incremental_at: None,
            updated_at: None,
        };
        assert_eq!(
            gate_index_state(Some(&rebuilding))
                .expect_err("rebuilding rejected")
                .code,
            ErrorCode::IndexNotReady
        );

        let indexed_at = Utc::now();
        let stale = IndexStateSnapshot {
            status: IndexStatus::Stale,
            last_full_rebuild_at: None,
            last_incremental_at: Some(indexed_at),
            updated_at: None,
        };
        let metadata = gate_index_state(Some(&stale)).expect("stale is servable");
        assert_eq!(metadata.index_status, IndexStatus::Stale);
        assert_eq!(metadata.last_indexed_at, Some(indexed_at));
    }

    #[test]
    fn cursors_are_signed_and_bound_to_tool_and_filters() {
        let filters = filter_hash(&json!({"query": "hello"}));
        let payload = CursorPayload {
            version: CURSOR_VERSION,
            tool: "search_notes".to_string(),
            filter_hash: filters.clone(),
            sort_keys: vec![
                "rank".to_string(),
                "updated_time".to_string(),
                "joplin_id".to_string(),
            ],
            last_values: vec![json!(0.75), json!(1000), json!(VALID_ID)],
            body: None,
        };
        let cursor = encode_cursor(&payload, CURSOR_KEY).expect("cursor encoded");
        let decoded =
            decode_cursor(&cursor, CURSOR_KEY, "search_notes", &filters).expect("cursor decoded");
        assert_eq!(decoded, payload);

        let wrong_tool = decode_cursor(&cursor, CURSOR_KEY, "list_notes", &filters)
            .expect_err("wrong tool rejected");
        assert!(wrong_tool.message.contains("tool"));

        let wrong_filter = decode_cursor(&cursor, CURSOR_KEY, "search_notes", "different")
            .expect_err("wrong filter rejected");
        assert!(wrong_filter.message.contains("filters"));

        let invalid = decode_cursor("not-a-cursor", CURSOR_KEY, "search_notes", &filters)
            .expect_err("invalid cursor rejected");
        assert_eq!(invalid.code, ErrorCode::Validation);
    }

    #[test]
    fn body_cursor_payload_round_trips() {
        let filters = filter_hash(&json!({"note_id": VALID_ID}));
        let payload = CursorPayload {
            version: CURSOR_VERSION,
            tool: "get_note".to_string(),
            filter_hash: filters.clone(),
            sort_keys: Vec::new(),
            last_values: Vec::new(),
            body: Some(BodyCursorPayload {
                note_id: VALID_ID.to_string(),
                indexed_note_version: "indexed-at-1000".to_string(),
                next_body_offset: 128,
            }),
        };
        let cursor = encode_cursor(&payload, CURSOR_KEY).expect("cursor encoded");
        let decoded =
            decode_cursor(&cursor, CURSOR_KEY, "get_note", &filters).expect("cursor decoded");
        assert_eq!(decoded.body.expect("body").next_body_offset, 128);
    }

    #[test]
    fn user_scope_exposes_user_id_predicate() {
        let user_id = Uuid::new_v4();
        let scope = UserScope::new(user_id);
        assert_eq!(scope.user_id(), user_id);
        assert_eq!(scope.predicate(1), "user_id = $1");
    }

    #[test]
    fn status_is_read_only_and_unencrypted_only() {
        let value = status_response(
            "a***@example.com",
            IndexMetadata {
                index_status: IndexStatus::Ready,
                last_indexed_at: None,
            },
        );
        assert_eq!(value["mode"], "read-only");
        assert_eq!(value["unencrypted_only"], true);
        assert_eq!(value["index_status"], "ready");
    }

    #[test]
    fn notebook_items_keep_parent_ids_and_counts() {
        let rows = vec![
            NotebookRow {
                joplin_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
                parent_joplin_id: Some(VALID_ID.to_string()),
                title: "Child".to_string(),
                note_count: 2,
            },
            NotebookRow {
                joplin_id: VALID_ID.to_string(),
                parent_joplin_id: None,
                title: "Root".to_string(),
                note_count: 1,
            },
        ];

        let items = notebook_items(&rows);
        assert_eq!(items[0].id, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        assert_eq!(items[0].parent_id.as_deref(), Some(VALID_ID));
        assert_eq!(items[0].note_count, 2);
    }

    #[test]
    fn notebook_tree_nests_children_and_promotes_missing_parents() {
        let rows = vec![
            NotebookRow {
                joplin_id: VALID_ID.to_string(),
                parent_joplin_id: None,
                title: "Root".to_string(),
                note_count: 1,
            },
            NotebookRow {
                joplin_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
                parent_joplin_id: Some(VALID_ID.to_string()),
                title: "Child".to_string(),
                note_count: 2,
            },
            NotebookRow {
                joplin_id: "cccccccccccccccccccccccccccccccc".to_string(),
                parent_joplin_id: Some("dddddddddddddddddddddddddddddddd".to_string()),
                title: "Orphan".to_string(),
                note_count: 3,
            },
        ];

        let tree = notebook_tree(&rows);
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].id, VALID_ID);
        assert_eq!(tree[0].children[0].id, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        assert_eq!(tree[1].id, "cccccccccccccccccccccccccccccccc");
    }

    #[test]
    fn get_notes_by_tag_cursor_uses_updated_time_desc_and_joplin_id_desc_keys() {
        let note = NoteSummary {
            joplin_id: VALID_ID.to_string(),
            title: "Tagged".to_string(),
            notebook_id: None,
            updated_time: Some(1234),
        };
        let filters = filter_hash(&json!({"tag_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}));
        let cursor =
            encode_notes_by_tag_cursor(&note, &filters, CURSOR_KEY).expect("cursor encoded");
        let payload = decode_cursor(&cursor, CURSOR_KEY, "get_notes_by_tag", &filters)
            .expect("cursor decoded");

        assert_eq!(payload.sort_keys, ["updated_time", "joplin_id"]);
        assert_eq!(
            tag_cursor_values(&payload).expect("tag cursor values"),
            (1234, VALID_ID.to_string())
        );
    }

    #[test]
    fn get_changes_since_cursor_uses_updated_time_asc_and_joplin_id_asc_keys() {
        let note = NoteChange {
            joplin_id: VALID_ID.to_string(),
            title: "Changed".to_string(),
            notebook_id: None,
            updated_time: 1234,
        };
        let filters = filter_hash(&json!({"since": 1000}));
        let cursor =
            encode_changes_since_cursor(&note, &filters, CURSOR_KEY).expect("cursor encoded");
        let payload = decode_cursor(&cursor, CURSOR_KEY, "get_changes_since", &filters)
            .expect("cursor decoded");

        assert_eq!(payload.sort_keys, ["updated_time", "joplin_id"]);
        assert_eq!(
            changes_cursor_values(&payload).expect("changes cursor values"),
            (1234, VALID_ID.to_string())
        );
    }

    #[test]
    fn resource_items_preserve_note_reference_order_and_metadata_only() {
        let refs = vec![
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
            VALID_ID.to_string(),
        ];
        let rows = vec![
            ResourceRow {
                joplin_id: VALID_ID.to_string(),
                title: "Diagram".to_string(),
                mime: Some("image/png".to_string()),
                size_bytes: Some(42),
                file_extension: Some("png".to_string()),
                updated_time: Some(2000),
            },
            ResourceRow {
                joplin_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
                title: "Attachment".to_string(),
                mime: Some("application/pdf".to_string()),
                size_bytes: Some(100),
                file_extension: Some("pdf".to_string()),
                updated_time: Some(1000),
            },
        ];

        let resources = resource_items(&refs, rows);
        let value = json!({
            "binary_download_supported": false,
            "binary_download": "unsupported_in_v1",
            "resources": resources
        });

        assert_eq!(value["resources"][0]["id"], refs[0]);
        assert_eq!(value["resources"][1]["id"], refs[1]);
        assert_eq!(value["resources"][0]["mime"], "application/pdf");
        assert!(value.to_string().find("base64").is_none());
        assert!(value.to_string().find("content").is_none());
    }

    #[test]
    fn status_row_reports_missing_index_without_gating_error() {
        let row = StatusRow {
            joplin_email: "user@example.com".to_string(),
            status: None,
            last_full_rebuild_at: None,
            last_incremental_at: None,
            updated_at: None,
        };
        let metadata = row.index_metadata().expect("metadata");

        assert_eq!(metadata.index_status, IndexStatus::Missing);
        assert_eq!(metadata.last_indexed_at, None);
    }
}
