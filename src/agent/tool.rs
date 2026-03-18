use serde_json::Value;
use super::types::{ToolResult, ToolSpec};

#[derive(Debug, Clone, PartialEq)]
pub enum ToolSafety {
    ReadOnly,
    Validated,
}

impl ToolSafety {
    pub fn requires_validation(&self) -> bool {
        matches!(self, Self::Validated)
    }
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> Value;
    fn safety(&self) -> ToolSafety;
    async fn execute(&self, args: Value) -> ToolResult;

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().into(),
            description: self.description().into(),
            parameters: self.parameters_schema(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct MockReadTool;

    #[async_trait::async_trait]
    impl Tool for MockReadTool {
        fn name(&self) -> &str {
            "mock_read"
        }

        fn description(&self) -> &str {
            "A mock read-only tool for testing"
        }

        fn parameters_schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The path to read"
                    }
                },
                "required": ["path"]
            })
        }

        fn safety(&self) -> ToolSafety {
            ToolSafety::ReadOnly
        }

        async fn execute(&self, _args: Value) -> ToolResult {
            ToolResult {
                success: true,
                output: "mock output".into(),
            }
        }
    }

    #[test]
    fn test_tool_spec_generation() {
        let tool = MockReadTool;
        let spec = tool.spec();

        assert_eq!(spec.name, "mock_read");
        assert_eq!(spec.description, "A mock read-only tool for testing");
        assert_eq!(spec.parameters["type"], "object");
        assert!(spec.parameters["properties"]["path"].is_object());
        assert_eq!(spec.parameters["required"][0], "path");
    }

    #[test]
    fn test_tool_safety_requires_validation() {
        assert!(!ToolSafety::ReadOnly.requires_validation());
        assert!(ToolSafety::Validated.requires_validation());
    }
}
