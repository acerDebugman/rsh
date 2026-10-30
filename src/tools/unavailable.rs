use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::ToolResult;

use super::{Tool, ToolCtx, ToolOutput, function_schema};

/// Placeholder for tools whose milestone has not landed yet. The schema is
/// real so the model sees the final tool surface from M1 onward; calling it
/// returns an actionable error instead of a protocol error.
pub struct UnavailableTool {
    name: &'static str,
    description: &'static str,
    schema: Value,
    reason: &'static str,
}

impl UnavailableTool {
    #[must_use]
    pub fn subagent() -> Self {
        Self {
            name: "subagent",
            description: "Delegate one isolated task to a subagent.",
            schema: function_schema(
                "subagent",
                "Delegate one isolated task to a subagent.",
                json!({
                    "type": "object",
                    "properties": {
                        "prompt": {"type": "string"},
                        "backend": {"type": "string", "enum": ["native", "dsh"]},
                        "cwd": {"type": "string"},
                        "timeout_secs": {"type": "integer"}
                    },
                    "required": ["prompt"],
                    "additionalProperties": false
                }),
            ),
            reason: "subagent backends land in M4",
        }
    }
}

#[async_trait]
impl Tool for UnavailableTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    async fn run(&self, _args: Value, _ctx: &ToolCtx) -> ToolResult<ToolOutput> {
        Ok(ToolOutput::error(format!(
            "{} is not available in M1: {}",
            self.name, self.reason
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stubs_keep_the_final_tool_surface() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        for tool in [UnavailableTool::subagent()] {
            let output = tool.run(serde_json::json!({}), &ctx).await.unwrap();
            assert!(output.is_error);
            assert!(output.content.contains("not available in M1"));
        }
    }

    #[test]
    fn registry_exposes_nine_tools() {
        let registry = super::super::ToolRegistry::with_builtin_tools();
        let names = registry.names();
        assert_eq!(names.len(), 9, "{names:?}");
        for expected in [
            "bash",
            "read_file",
            "write_file",
            "edit_file",
            "glob",
            "grep",
            "ask_user",
            "skill",
            "subagent",
        ] {
            assert!(names.contains(&expected), "missing {expected}");
        }
        assert_eq!(tool_count(&registry), 9);
    }

    fn tool_count(registry: &super::super::ToolRegistry) -> usize {
        registry.names().len()
    }
}
