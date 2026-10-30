use std::path::Path;

use async_trait::async_trait;
use globset::Glob;
use serde_json::{Value, json};
use walkdir::{DirEntry, WalkDir};

use crate::error::{ToolError, ToolResult};

use super::{Tool, ToolCtx, ToolOutput, function_schema, resolve_path};

const MAX_GLOB_RESULTS: usize = 200;
const MAX_GREP_RESULTS: usize = 250;
const MAX_GREP_FILE_BYTES: u64 = 2 * 1024 * 1024;

fn is_ignored(entry: &DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    matches!(name.as_ref(), ".git" | "target" | "node_modules")
}

fn relative<'a>(entry: &'a DirEntry, workspace: &Path) -> &'a Path {
    entry.path().strip_prefix(workspace).unwrap_or(entry.path())
}

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "Find files by glob pattern, newest first."
    }

    fn schema(&self) -> Value {
        function_schema(
            "glob",
            self.description(),
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "e.g. **/*.rs"},
                    "path": {"type": "string", "description": "Directory to search (default: workspace)"}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        )
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolResult<ToolOutput> {
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("pattern is required".into()))?;
        let root = match args.get("path").and_then(Value::as_str) {
            Some(path) => resolve_path(ctx, path)?,
            None => ctx.workspace.clone(),
        };
        let matcher = Glob::new(pattern)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?
            .compile_matcher();

        let mut matches: Vec<(std::time::SystemTime, String)> = Vec::new();
        for entry in WalkDir::new(&root)
            .into_iter()
            .filter_entry(|entry| !is_ignored(entry))
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = relative(&entry, &ctx.workspace);
            if !matcher.is_match(path) {
                continue;
            }
            let modified = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .unwrap_or(std::time::UNIX_EPOCH);
            matches.push((modified, path.display().to_string()));
        }
        matches.sort_by(|left, right| right.0.cmp(&left.0));
        matches.truncate(MAX_GLOB_RESULTS);

        if matches.is_empty() {
            return Ok(ToolOutput::ok("(no matches)"));
        }
        let mut output = String::new();
        for (_, path) in matches {
            output.push_str(&path);
            output.push('\n');
        }
        Ok(ToolOutput::ok(output))
    }
}

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents with a regular expression, returning file:line matches."
    }

    fn schema(&self) -> Value {
        function_schema(
            "grep",
            self.description(),
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Rust regex"},
                    "path": {"type": "string", "description": "Directory to search (default: workspace)"},
                    "include": {"type": "string", "description": "Optional glob filter, e.g. **/*.rs"}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        )
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolResult<ToolOutput> {
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("pattern is required".into()))?;
        let regex = regex::Regex::new(pattern)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;
        let root = match args.get("path").and_then(Value::as_str) {
            Some(path) => resolve_path(ctx, path)?,
            None => ctx.workspace.clone(),
        };
        let include = args
            .get("include")
            .and_then(Value::as_str)
            .map(|pattern| {
                Glob::new(pattern)
                    .map(|glob| glob.compile_matcher())
                    .map_err(|error| ToolError::InvalidArgs(error.to_string()))
            })
            .transpose()?;

        let mut output = String::new();
        let mut hits = 0;
        'outer: for entry in WalkDir::new(&root)
            .into_iter()
            .filter_entry(|entry| !is_ignored(entry))
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = relative(&entry, &ctx.workspace);
            if let Some(include) = &include {
                if !include.is_match(path) {
                    continue;
                }
            }
            if entry.metadata().map(|meta| meta.len()).unwrap_or(0) > MAX_GREP_FILE_BYTES {
                continue;
            }
            let Ok(bytes) = std::fs::read(entry.path()) else {
                continue;
            };
            if bytes.iter().take(8_192).any(|byte| *byte == 0) {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);
            for (index, line) in text.lines().enumerate() {
                if regex.is_match(line) {
                    output.push_str(&format!(
                        "{}:{}:{}\n",
                        path.display(),
                        index + 1,
                        line.trim_end()
                    ));
                    hits += 1;
                    if hits >= MAX_GREP_RESULTS {
                        break 'outer;
                    }
                }
            }
        }

        if hits == 0 {
            Ok(ToolOutput::ok("(no matches)"))
        } else {
            Ok(ToolOutput::ok(output))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn workspace() -> PathBuf {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::Builder::new()
            .prefix("search-")
            .tempdir_in(&base)
            .unwrap()
            .keep();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn alpha() {}\n// needle\n").unwrap();
        std::fs::write(root.join("src/b.txt"), "needle here\n").unwrap();
        root
    }

    #[tokio::test]
    async fn glob_finds_matching_files() {
        let ctx = ToolCtx::new(workspace());
        let output = GlobTool
            .run(json!({"pattern": "**/*.rs"}), &ctx)
            .await
            .unwrap();
        assert!(output.content.contains("src/a.rs"), "{}", output.content);
        assert!(!output.content.contains("b.txt"));
    }

    #[tokio::test]
    async fn grep_reports_file_and_line() {
        let ctx = ToolCtx::new(workspace());
        let output = GrepTool
            .run(json!({"pattern": "needle", "include": "**/*.rs"}), &ctx)
            .await
            .unwrap();
        assert!(output.content.contains("src/a.rs:2"), "{}", output.content);
        assert!(!output.content.contains("b.txt"));
    }
}
