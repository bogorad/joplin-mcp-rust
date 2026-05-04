use uuid::Uuid;

pub const TEST_ID_HEADER: &str = "x-test-id";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestContext {
    pub request_id: Uuid,
    pub test_id: Option<String>,
}

impl RequestContext {
    pub fn new(test_id: Option<String>) -> Self {
        Self {
            request_id: Uuid::new_v4(),
            test_id,
        }
    }
}

pub fn new_test_id() -> String {
    format!("jp-mcp-test-{}", Uuid::new_v4())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_test_id_has_required_prefix() {
        assert!(new_test_id().starts_with("jp-mcp-test-"));
    }
}
