use crate::contracts::{READ_ONLY_MODE, UNSUPPORTED_SHARED_NOTEBOOKS};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "status",
            description: "Return JP-MCP read-only status for the authenticated user.",
            input_schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        },
        ToolDefinition {
            name: "search_notes",
            description: "Search indexed unencrypted notes for the authenticated user.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "minLength": 1, "maxLength": 1024},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20},
                    "cursor": {"type": "string"}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
    ]
}

pub fn status_response(user: &str, index_status: &str) -> Value {
    json!({
        "user": user,
        "mode": READ_ONLY_MODE,
        "index_status": index_status,
        "unencrypted_only": true,
        "shared_notebooks": UNSUPPORTED_SHARED_NOTEBOOKS
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_input_schema() {
        for tool in tool_definitions() {
            assert!(tool.input_schema.get("type").is_some(), "{}", tool.name);
        }
    }

    #[test]
    fn status_is_read_only_and_unencrypted_only() {
        let value = status_response("a***@example.com", "ready");
        assert_eq!(value["mode"], "read-only");
        assert_eq!(value["unencrypted_only"], true);
    }
}
