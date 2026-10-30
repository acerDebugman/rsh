pub mod ask_user;
pub mod bash;
pub mod files;
pub mod search;
pub mod skill;
pub mod subagent;

pub use bash::BashTool;

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

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
    pub approver: Option<Arc<dyn Approver>>,
    pub approvals: Arc<SessionApprovals>,
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
            approver: None,
            approvals: Arc::new(SessionApprovals::default()),
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

    #[must_use]
    pub fn with_approver(mut self, approver: Option<Arc<dyn Approver>>) -> Self {
        self.approver = approver;
        self
    }

    #[must_use]
    pub fn with_approvals(mut self, approvals: Arc<SessionApprovals>) -> Self {
        self.approvals = approvals;
        self
    }

    /// Refuse a side-effecting action in `read-only` mode.
    ///
    /// This is deliberately *not* part of [`Self::authorize`]: reads still go
    /// through `resolve_path_checked`, which authorizes cross-workspace access,
    /// and read-only must never turn a read into an error.
    pub fn ensure_writable(&self, tool: &'static str, detail: &str) -> ToolResult<()> {
        if self.permission == PermissionMode::ReadOnly {
            return Err(ToolError::Denied(format!(
                "read-only mode: {tool} refused ({detail})"
            )));
        }
        Ok(())
    }

    /// Gate a side-effecting action through the permission mode.
    ///
    /// `danger-full-access` returns immediately, and so does `read-only`:
    /// that mode has nothing to ask about, because the writes it cares about
    /// were already refused by [`Self::ensure_writable`]. Only `ask` consults
    /// the session approval cache and then the interactive approver; with no
    /// approver (headless) it fails instead of silently running the action.
    pub async fn authorize(&self, tool: &'static str, detail: &str) -> ToolResult<()> {
        if matches!(
            self.permission,
            PermissionMode::DangerFullAccess | PermissionMode::ReadOnly
        ) {
            return Ok(());
        }
        if self.approvals.is_allowed(tool, detail) {
            return Ok(());
        }
        let Some(approver) = &self.approver else {
            return Err(ToolError::Denied(format!(
                "{tool} needs approval, but this run has no approval surface; \
                 rerun interactively or with --yolo"
            )));
        };
        let request = ApprovalRequest {
            tool,
            detail: detail.to_string(),
        };
        match approver.approve(&request).await? {
            Approval::AllowOnce => Ok(()),
            Approval::AllowSession => {
                self.approvals.allow(tool, detail);
                Ok(())
            }
            Approval::Deny => Err(ToolError::Denied(format!("user denied {tool}: {detail}"))),
        }
    }
}

/// A side-effecting action waiting for a user decision in `ask` mode.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub tool: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    AllowOnce,
    AllowSession,
    Deny,
}

/// Bridge from `ask` mode to an interactive surface.
#[async_trait]
pub trait Approver: Send + Sync {
    async fn approve(&self, request: &ApprovalRequest) -> ToolResult<Approval>;
}

/// "Allow for this session" memory. Keys on the exact action string so one
/// approved command never blanket-allows a different one.
#[derive(Debug, Default)]
pub struct SessionApprovals {
    allowed: Mutex<HashSet<String>>,
}

impl SessionApprovals {
    fn key(tool: &str, detail: &str) -> String {
        format!("{tool}\u{1}{detail}")
    }

    pub fn allow(&self, tool: &str, detail: &str) {
        if let Ok(mut allowed) = self.allowed.lock() {
            allowed.insert(Self::key(tool, detail));
        }
    }

    #[must_use]
    pub fn is_allowed(&self, tool: &str, detail: &str) -> bool {
        self.allowed
            .lock()
            .map(|allowed| allowed.contains(&Self::key(tool, detail)))
            .unwrap_or(false)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.allowed
            .lock()
            .map(|allowed| allowed.len())
            .unwrap_or(0)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The answer to an `ask_user` question. Cancellation is its own outcome: an
/// empty string would read to the model as "the user said nothing", which is a
/// different and much worse thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskOutcome {
    Answered(String),
    Cancelled,
}

/// Bridge from `ask_user` to an interactive surface. The TUI implements this;
/// headless runs leave it unset and the tool reports that no UI is available.
#[async_trait]
pub trait UserIo: Send + Sync {
    async fn ask(&self, question: &str, options: &[String]) -> ToolResult<AskOutcome>;
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

    /// Per-tool output ceiling. `bash` overrides this and truncates its own
    /// result so the tail (where the real error lives) survives.
    fn output_limit(&self, default_limit: usize) -> usize {
        default_limit
    }

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

    /// Tools available inside a native subagent: no `subagent` (no recursion)
    /// and no `ask_user` (a subagent must never interrupt the user).
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
            .register(skill::SkillTool);
        if include_subagent {
            registry
                .register(ask_user::AskUserTool)
                .register(subagent::SubagentTool);
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
        output.truncate(tool.output_limit(ctx.output_limit));
        Ok(output)
    }
}

/// Resolve a path for a tool that changes the filesystem. In `ask` mode any
/// path outside the workspace waits for approval; denial is a tool error the
/// model can read.
pub async fn resolve_path_checked(ctx: &ToolCtx, raw: &str) -> ToolResult<PathBuf> {
    let normalized = resolve_path(ctx, raw)?;
    if ctx.permission != PermissionMode::DangerFullAccess && !normalized.starts_with(&ctx.workspace)
    {
        ctx.authorize("path", &normalized.display().to_string())
            .await?;
    }
    Ok(normalized)
}

/// Normalize a user/model supplied path against the workspace without
/// requiring it to exist. Pure: the permission decision lives in
/// [`resolve_path_checked`], so there is exactly one place that decides.
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
    Ok(normalize(&absolute))
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

    #[tokio::test]
    async fn ask_mode_cannot_escape_without_approval() {
        let root = workspace();
        let ctx = ToolCtx::new(root.clone()).with_permission(PermissionMode::Ask);
        let error = resolve_path_checked(&ctx, "../outside.txt")
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Denied(_)), "{error}");
    }

    #[test]
    fn normalization_does_not_decide_permissions() {
        let root = workspace();
        let ctx = ToolCtx::new(root.clone()).with_permission(PermissionMode::Ask);
        assert_eq!(
            resolve_path(&ctx, "../outside.txt").unwrap(),
            root.parent().unwrap().join("outside.txt")
        );
    }

    #[test]
    fn full_access_allows_absolute_paths() {
        let root = workspace();
        let ctx = ToolCtx::new(root);
        let path = resolve_path(&ctx, "/tmp/example.txt").unwrap();
        assert_eq!(path, PathBuf::from("/tmp/example.txt"));
    }
}
