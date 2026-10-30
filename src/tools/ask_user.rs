use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::ToolResult;

use super::{AskOutcome, Tool, ToolCtx, ToolOutput, function_schema};

/// Ask the user a question. In a TUI session this blocks until the user
/// answers in the composer; headless runs report that no UI is available.
pub struct AskUserTool;

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &'static str {
        "ask_user"
    }

    fn description(&self) -> &'static str {
        "Ask the user a question and wait for an answer. Use this when a decision cannot be inferred."
    }

    fn schema(&self) -> Value {
        function_schema(
            "ask_user",
            self.description(),
            json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string"},
                    "options": {"type": "array", "items": {"type": "string"}, "description": "Optional choices to show"}
                },
                "required": ["question"],
                "additionalProperties": false
            }),
        )
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolResult<ToolOutput> {
        let question = args
            .get("question")
            .and_then(Value::as_str)
            .ok_or_else(|| crate::error::ToolError::InvalidArgs("question is required".into()))?;
        let options: Vec<String> = args
            .get("options")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let Some(user_io) = &ctx.user_io else {
            return Ok(ToolOutput::error(
                "ask_user requires an interactive TUI session",
            ));
        };
        match user_io.ask(question, &options).await {
            Ok(AskOutcome::Answered(answer)) => Ok(ToolOutput::ok(answer)),
            // Never an empty string: the model would read that as an answer.
            Ok(AskOutcome::Cancelled) => Ok(ToolOutput::error("user cancelled the question")),
            Err(error) => Ok(ToolOutput::error(error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::*;
    use crate::error::ToolError;
    use crate::tools::{AskOutcome, UserIo};

    struct MockUserIo {
        outcome: AskOutcome,
    }

    #[async_trait]
    impl UserIo for MockUserIo {
        async fn ask(&self, _question: &str, _options: &[String]) -> Result<AskOutcome, ToolError> {
            Ok(self.outcome.clone())
        }
    }

    fn ctx_with(outcome: AskOutcome) -> ToolCtx {
        ToolCtx::new(std::env::temp_dir()).with_user_io(Some(Arc::new(MockUserIo { outcome })))
    }

    #[tokio::test]
    async fn uses_the_interactive_bridge() {
        let ctx = ctx_with(AskOutcome::Answered("yes".into()));
        let output = AskUserTool
            .run(json!({"question": "continue?"}), &ctx)
            .await
            .unwrap();
        assert_eq!(output.content, "yes");
    }

    #[tokio::test]
    async fn cancellation_is_an_error_not_an_empty_answer() {
        let ctx = ctx_with(AskOutcome::Cancelled);
        let output = AskUserTool
            .run(json!({"question": "continue?"}), &ctx)
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("cancelled"));
        assert_ne!(output.content, "");
    }

    #[tokio::test]
    async fn headless_reports_missing_ui() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let output = AskUserTool
            .run(json!({"question": "continue?"}), &ctx)
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("interactive TUI"));
    }
}
