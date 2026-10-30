pub mod ask_user;
pub mod bash;
pub mod files;
pub mod search;
pub mod skill;
pub mod subagent;

pub use bash::BashTool;

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::config::PermissionMode;
use crate::error::ToolError;
pub use crate::error::ToolResult;
use crate::skills::SkillRegistry;
use crate::subagent::SubagentRunner;

pub const DEFAULT_OUTPUT_LIMIT: usize = 30_000;

pub struct ToolCtx {
    pub workspace: PathBuf,
    pub permission: PermissionMode,
    pub cancel: CancellationToken,
    pub output_limit: usize,
    pub user_io: Option<Arc<dyn UserIo>>,
    pub skills: Option<Arc<SkillRegistry>>,
    pub subagents: Option<Arc<dyn SubagentRunner>>,
}

impl ToolCtx {
    #[must_use]
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            permission: PermissionMode::DangerFullAccess,
            cancel: CancellationToken::new(),
            output_limit: DEFAULT_OUTPUT_LIMIT,
            user_io: None,
            skills: None,
            subagents: None,
        }
    }

    #[must_use]
    pub fn with_permission(mut self, permission: PermissionMode) -> Self {
        self.permission = permission;
        self
    }

    #[must_use]
    pub fn with_user_io(mut self, user_io: Option<Arc<dyn UserIo>>) -> Self {
        self.user_io = user_io;
        self
    }

    #[must_use]
    pub fn with_skills(mut self, skills: Option<Arc<SkillRegistry>>) -> Self {
        self.skills = skills;
        self
    }

    #[must_use]
    pub fn with_subagents(mut self, subagents: Option<Arc<dyn SubagentRunner>>) -> Self {
        self.subagents = subagents;
        self
    }
}

/// Bridge from `ask_user` to an interactive surface. The TUI implements this;
/// headless runs leave it unset and the tool reports that no UI is available.
#[async_trait]
pub trait UserIo: Send + Sync {
    async fn ask(&self, question: &str, options: &[String]) -> ToolResult<String>;
}

#[derive(Debug, Clone, Default)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub changed_files: Vec<PathBuf>,
}

impl ToolOutput {
    #[must_use]
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            changed_files: Vec::new(),
        }
    }

    #[must_use]
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            changed_files: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_changed(mut self, path: PathBuf) -> Self {
        self.changed_files.push(path);
        self
    }

    pub fn truncate(&mut self, limit: usize) {
        let total = self.content.chars().count();
        if total <= limit {
            return;
        }
        let kept: String = self.content.chars().take(limit).collect();
        self.content = format!("{kept}\n[truncated {} chars]", total - limit);
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn schema(&self) -> Value;
    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolResult<ToolOutput>;
}

#[must_use]
pub fn function_schema(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters,
        }
    })
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    #[must_use]
    pub fn with_builtin_tools() -> Self {
        Self::builtin(true)
    }

    /// Tools available inside a native subagent: everything except `subagent`
    /// (no recursion).
    #[must_use]
    pub fn native_subagent_tools() -> Self {
        Self::builtin(false)
    }

    fn builtin(include_subagent: bool) -> Self {
        let mut registry = Self::default();
        registry
            .register(bash::BashTool)
            .register(files::ReadFileTool)
            .register(files::WriteFileTool)
            .register(files::EditFileTool)
            .register(search::GlobTool)
            .register(search::GrepTool)
            .register(ask_user::AskUserTool)
            .register(skill::SkillTool);
        if include_subagent {
            registry.register(subagent::SubagentTool);
        }
        registry
    }

    pub fn register<T: Tool + 'static>(&mut self, tool: T) -> &mut Self {
        self.tools.push(Box::new(tool));
        self
    }

    #[must_use]
    pub fn schemas(&self) -> Vec<Value> {
        self.tools.iter().map(|tool| tool.schema()).collect()
    }

    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.tools.iter().map(|tool| tool.name()).collect()
    }

    pub async fn run(&self, name: &str, args: Value, ctx: &ToolCtx) -> ToolResult<ToolOutput> {
        let tool = self
            .tools
            .iter()
            .find(|tool| tool.name() == name)
            .ok_or_else(|| ToolError::Other(format!("unknown tool: {name}")))?;
        let mut output = tool.run(args, ctx).await?;
        output.truncate(ctx.output_limit);
        Ok(output)
    }
}

/// Resolve a user/model supplied path against the workspace and enforce the
/// permission boundary. Normalizes `..` without requiring the path to exist.
pub fn resolve_path(ctx: &ToolCtx, raw: &str) -> ToolResult<PathBuf> {
    if raw.trim().is_empty() {
        return Err(ToolError::InvalidArgs("path must not be empty".into()));
    }
    let path = Path::new(raw);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        ctx.workspace.join(path)
    };
    let normalized = normalize(&absolute);
    if ctx.permission != PermissionMode::DangerFullAccess && !normalized.starts_with(&ctx.workspace)
    {
        return Err(ToolError::PathEscape(normalized.display().to_string()));
    }
    Ok(normalized)
}

fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> PathBuf {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::Builder::new()
            .prefix("tools-")
            .tempdir_in(&base)
            .unwrap()
            .keep()
    }

    #[test]
    fn relative_paths_resolve_inside_workspace() {
        let root = workspace();
        let ctx = ToolCtx::new(root.clone());
        let path = resolve_path(&ctx, "src/main.rs").unwrap();
        assert_eq!(path, root.join("src/main.rs"));
    }

    #[test]
    fn parent_traversal_is_normalized() {
        let root = workspace();
        let ctx = ToolCtx::new(root.clone());
        let path = resolve_path(&ctx, "src/../a.txt").unwrap();
        assert_eq!(path, root.join("a.txt"));
    }

    #[test]
    fn restricted_mode_rejects_escape() {
        let root = workspace();
        let ctx = ToolCtx::new(root.clone()).with_permission(PermissionMode::WorkspaceWrite);
        let error = resolve_path(&ctx, "../outside.txt").unwrap_err();
        assert!(matches!(error, ToolError::PathEscape(_)));
    }

    #[test]
    fn full_access_allows_absolute_paths() {
        let root = workspace();
        let ctx = ToolCtx::new(root);
        let path = resolve_path(&ctx, "/tmp/example.txt").unwrap();
        assert_eq!(path, PathBuf::from("/tmp/example.txt"));
    }
}
