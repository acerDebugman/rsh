use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::error::{ToolError, ToolResult};

use super::{Tool, ToolCtx, ToolOutput, function_schema};

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        "Run a shell command in the workspace and return stdout, stderr, and the exit code."
    }

    fn schema(&self) -> Value {
        function_schema(
            "bash",
            self.description(),
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command to run"},
                    "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds (default 120000)"}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        )
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolResult<ToolOutput> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("command is required".into()))?;
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(120_000)
            .clamp(1_000, 600_000);

        let mut process = shell_command(command);
        process
            .current_dir(&ctx.workspace)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let child = process.spawn().map_err(ToolError::Io)?;
        let output =
            match tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait_with_output())
                .await
            {
                Ok(Ok(output)) => output,
                Ok(Err(error)) => return Err(ToolError::Io(error)),
                Err(_) => {
                    return Ok(ToolOutput::error(format!(
                        "command timed out after {timeout_ms} ms"
                    )));
                }
            };

        let code = output.status.code().unwrap_or(-1);
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&stderr);
        }
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("[exit code: {code}]"));

        if output.status.success() {
            Ok(ToolOutput::ok(text))
        } else {
            Ok(ToolOutput::error(text))
        }
    }
}

#[cfg(not(windows))]
fn shell_command(command: &str) -> Command {
    let mut process = Command::new("sh");
    process.arg("-c").arg(command);
    process
}

#[cfg(windows)]
fn shell_command(command: &str) -> Command {
    let mut process = Command::new("cmd");
    process.arg("/C").arg(command);
    process
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn workspace() -> std::path::PathBuf {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::Builder::new()
            .prefix("bash-")
            .tempdir_in(&base)
            .unwrap()
            .keep()
    }

    #[tokio::test]
    async fn reports_stdout_and_exit_code() {
        let ctx = ToolCtx::new(workspace());
        let output = BashTool
            .run(json!({"command": "printf hello"}), &ctx)
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("hello"));
        assert!(output.content.contains("[exit code: 0]"));
    }

    #[tokio::test]
    async fn failing_command_is_an_error_result() {
        let ctx = ToolCtx::new(workspace());
        let output = BashTool
            .run(json!({"command": "exit 7"}), &ctx)
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("[exit code: 7]"));
    }

    #[tokio::test]
    async fn timeout_returns_an_error_result() {
        let ctx = ToolCtx::new(workspace());
        let output = BashTool
            .run(json!({"command": "sleep 5", "timeout_ms": 1000}), &ctx)
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("timed out"));
    }
}
