use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::{ToolError, ToolResult};

use super::{Tool, ToolCtx, ToolOutput, function_schema, resolve_path};

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Read a text file with line numbers. Supports offset and limit."
    }

    fn schema(&self) -> Value {
        function_schema(
            "read_file",
            self.description(),
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer", "description": "1-based first line"},
                    "limit": {"type": "integer", "description": "Maximum lines (default 2000)"}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        )
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolResult<ToolOutput> {
        let raw = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("path is required".into()))?;
        let offset = args
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(2_000)
            .clamp(1, 10_000) as usize;

        let path = resolve_path(ctx, raw)?;
        let bytes = tokio::fs::read(&path).await.map_err(ToolError::Io)?;
        if bytes.iter().take(8_192).any(|byte| *byte == 0) {
            return Ok(ToolOutput::error(format!(
                "{} looks like a binary file",
                path.display()
            )));
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        if total == 0 {
            return Ok(ToolOutput::ok(format!("(empty file: {})", path.display())));
        }
        let start = (offset - 1).min(total);
        let end = (start + limit).min(total);
        let mut output = String::new();
        for (index, line) in lines[start..end].iter().enumerate() {
            output.push_str(&format!("{}\t{}\n", start + index + 1, line));
        }
        if end < total {
            output.push_str(&format!("[{} more lines]\n", total - end));
        }
        Ok(ToolOutput::ok(output))
    }
}

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        "Create or overwrite a file, creating parent directories as needed."
    }

    fn schema(&self) -> Value {
        function_schema(
            "write_file",
            self.description(),
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        )
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolResult<ToolOutput> {
        let raw = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("path is required".into()))?;
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("content is required".into()))?;

        let path = resolve_path(ctx, raw)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(ToolError::Io)?;
        }
        tokio::fs::write(&path, content)
            .await
            .map_err(ToolError::Io)?;
        Ok(ToolOutput::ok(format!(
            "wrote {} bytes to {}",
            content.len(),
            path.display()
        ))
        .with_changed(path))
    }
}

pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn description(&self) -> &'static str {
        "Replace an exact string in a file. Fails when the string is missing or ambiguous."
    }

    fn schema(&self) -> Value {
        function_schema(
            "edit_file",
            self.description(),
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean", "default": false}
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        )
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolResult<ToolOutput> {
        let raw = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("path is required".into()))?;
        let old = args
            .get("old_string")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("old_string is required".into()))?;
        let new = args
            .get("new_string")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("new_string is required".into()))?;
        let replace_all = args
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if old.is_empty() {
            return Err(ToolError::InvalidArgs(
                "old_string must not be empty".into(),
            ));
        }

        let path = resolve_path(ctx, raw)?;
        let text = tokio::fs::read_to_string(&path)
            .await
            .map_err(ToolError::Io)?;
        let count = text.matches(old).count();
        if count == 0 {
            return Ok(ToolOutput::error(format!(
                "old_string not found in {}",
                path.display()
            )));
        }
        if count > 1 && !replace_all {
            return Ok(ToolOutput::error(format!(
                "old_string appears {count} times in {}; pass replace_all=true or add more context",
                path.display()
            )));
        }
        let updated = if replace_all {
            text.replace(old, new)
        } else {
            text.replacen(old, new, 1)
        };
        tokio::fs::write(&path, updated)
            .await
            .map_err(ToolError::Io)?;
        Ok(ToolOutput::ok(format!(
            "replaced {count} occurrence(s) in {}",
            path.display()
        ))
        .with_changed(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn workspace() -> PathBuf {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::Builder::new()
            .prefix("files-")
            .tempdir_in(&base)
            .unwrap()
            .keep()
    }

    #[tokio::test]
    async fn write_then_read_with_line_numbers() {
        let ctx = ToolCtx::new(workspace());
        let output = WriteFileTool
            .run(
                json!({"path": "notes/a.txt", "content": "one\ntwo\nthree\n"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!output.is_error);
        assert_eq!(output.changed_files.len(), 1);

        let read = ReadFileTool
            .run(
                json!({"path": "notes/a.txt", "offset": 2, "limit": 1}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(read.content.contains("2\ttwo"));
        assert!(!read.content.contains("1\tone"));
    }

    #[tokio::test]
    async fn edit_replaces_a_unique_match() {
        let ctx = ToolCtx::new(workspace());
        WriteFileTool
            .run(json!({"path": "a.txt", "content": "hello world\n"}), &ctx)
            .await
            .unwrap();
        let output = EditFileTool
            .run(
                json!({"path": "a.txt", "old_string": "world", "new_string": "rsh"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!output.is_error);
        let text = tokio::fs::read_to_string(ctx.workspace.join("a.txt"))
            .await
            .unwrap();
        assert_eq!(text, "hello rsh\n");
    }

    #[tokio::test]
    async fn edit_rejects_ambiguous_matches() {
        let ctx = ToolCtx::new(workspace());
        WriteFileTool
            .run(json!({"path": "a.txt", "content": "x\nx\n"}), &ctx)
            .await
            .unwrap();
        let output = EditFileTool
            .run(
                json!({"path": "a.txt", "old_string": "x", "new_string": "y"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("2 times"));
    }

    #[tokio::test]
    async fn edit_missing_match_is_an_error_result() {
        let ctx = ToolCtx::new(workspace());
        WriteFileTool
            .run(json!({"path": "a.txt", "content": "hello"}), &ctx)
            .await
            .unwrap();
        let output = EditFileTool
            .run(
                json!({"path": "a.txt", "old_string": "nope", "new_string": "x"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("not found"));
    }

    #[tokio::test]
    async fn binary_files_are_rejected() {
        let ctx = ToolCtx::new(workspace());
        tokio::fs::write(ctx.workspace.join("bin"), b"\x00\x01\x02")
            .await
            .unwrap();
        let output = ReadFileTool
            .run(json!({"path": "bin"}), &ctx)
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("binary"));
    }
}
