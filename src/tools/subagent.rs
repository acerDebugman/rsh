use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::{ToolError, ToolResult};

use super::{Tool, ToolCtx, ToolOutput, function_schema};

pub struct SubagentTool;

#[async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &'static str {
        "subagent"
    }

    fn description(&self) -> &'static str {
        "Delegate one isolated task to a subagent and return its summary. The subagent cannot see this conversation."
    }

    fn schema(&self) -> Value {
        function_schema(
            "subagent",
            self.description(),
            json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "Complete task for the subagent"},
                    "backend": {"type": "string", "enum": ["native", "dsh"], "description": "Default: native"},
                    "cwd": {"type": "string", "description": "Optional working directory"},
                    "timeout_secs": {"type": "integer", "description": "Default 600"}
                },
                "required": ["prompt"],
                "additionalProperties": false
            }),
        )
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolResult<ToolOutput> {
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("prompt is required".into()))?;
        let backend = args.get("backend").and_then(Value::as_str);
        let cwd = args.get("cwd").and_then(Value::as_str).map(PathBuf::from);
        let timeout_secs = args.get("timeout_secs").and_then(Value::as_u64);
        let Some(runner) = &ctx.subagents else {
            return Ok(ToolOutput::error("subagent backends are not configured"));
        };
        match runner
            .run(prompt, backend, cwd.as_deref(), timeout_secs)
            .await
        {
            Ok(result) => Ok(ToolOutput::ok(result.render())),
            Err(error) => Ok(ToolOutput::error(error.to_string())),
        }
    }
}
