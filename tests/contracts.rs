//! Deterministic contracts: no network, no model, no terminal.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use rsh::config::PermissionMode;
use rsh::error::{ToolError, ToolResult};
use rsh::tools::{
    Approval, ApprovalRequest, Approver, AskOutcome, SessionApprovals, Tool, ToolCtx, ToolRegistry,
    UserIo,
};

fn workspace() -> std::path::PathBuf {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp");
    std::fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("contracts-")
        .tempdir_in(&base)
        .unwrap()
        .keep()
}

struct FixedApprover(Approval);

#[async_trait]
impl Approver for FixedApprover {
    async fn approve(&self, _request: &ApprovalRequest) -> ToolResult<Approval> {
        Ok(self.0)
    }
}

struct NoUserIo;

#[async_trait]
impl UserIo for NoUserIo {
    async fn ask(&self, _question: &str, _options: &[String]) -> ToolResult<AskOutcome> {
        Ok(AskOutcome::Cancelled)
    }
}

#[test]
fn the_tool_set_is_closed_at_nine() {
    let mut names = ToolRegistry::with_builtin_tools().names();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "ask_user",
            "bash",
            "edit_file",
            "glob",
            "grep",
            "read_file",
            "skill",
            "subagent",
            "write_file",
        ]
    );
}

#[test]
fn native_subagents_cannot_recurse_or_ask() {
    let names = ToolRegistry::native_subagent_tools().names();
    assert!(!names.contains(&"subagent"));
    assert!(!names.contains(&"ask_user"));
    assert!(names.contains(&"bash"));
    assert_eq!(names.len(), 7);
}

#[tokio::test]
async fn unknown_tools_are_rejected_not_ignored() {
    let registry = ToolRegistry::with_builtin_tools();
    let ctx = ToolCtx::new(workspace());
    let error = registry
        .run("nope", json!({}), &ctx)
        .await
        .expect_err("unknown tool");
    assert!(matches!(error, ToolError::Other(_)));
    assert!(error.to_string().contains("unknown tool"));
}

#[tokio::test]
async fn every_declared_tool_has_a_function_schema() {
    let registry = ToolRegistry::with_builtin_tools();
    for (name, schema) in registry.names().into_iter().zip(registry.schemas()) {
        assert_eq!(schema["type"], "function");
        assert_eq!(schema["function"]["name"], name);
        assert!(schema["function"]["description"].is_string());
    }
}

#[tokio::test]
async fn ask_mode_prompts_before_touching_a_path_outside_the_workspace() {
    let workspace = workspace();
    let approvals = Arc::new(SessionApprovals::default());
    let ctx = ToolCtx::new(workspace.clone())
        .with_permission(PermissionMode::Ask)
        .with_approvals(Arc::clone(&approvals))
        .with_approver(Some(Arc::new(FixedApprover(Approval::Deny))));

    let error = ToolRegistry::with_builtin_tools()
        .run(
            "write_file",
            json!({"path": "../escape.txt", "content": "x"}),
            &ctx,
        )
        .await
        .expect_err("denied escape");
    assert!(error.to_string().contains("denied"), "{error}");
    assert!(!workspace.parent().unwrap().join("escape.txt").exists());

    let approvals = approvals.len();
    assert_eq!(approvals, 0, "a denial must not be cached as an approval");
}

#[tokio::test]
async fn ask_mode_allows_an_approved_escape_and_remembers_it() {
    let workspace = workspace();
    let ctx = ToolCtx::new(workspace.clone())
        .with_permission(PermissionMode::Ask)
        .with_approver(Some(Arc::new(FixedApprover(Approval::AllowSession))));
    let target = workspace.parent().unwrap().join("allowed-escape.txt");
    let _ = std::fs::remove_file(&target);

    let output = registry_run(
        &ctx,
        "write_file",
        json!({"path": "../allowed-escape.txt", "content": "ok"}),
    )
    .await;
    assert!(!output.is_error, "{}", output.content);
    assert!(target.exists());
    assert_eq!(ctx.approvals.len(), 1);
    let _ = std::fs::remove_file(&target);
}

#[tokio::test]
async fn ask_user_cancellation_is_not_an_answer() {
    let ctx = ToolCtx::new(workspace()).with_user_io(Some(Arc::new(NoUserIo)));
    let output = registry_run(&ctx, "ask_user", json!({"question": "which one?"})).await;
    assert!(output.is_error);
    assert!(output.content.contains("cancelled"));
}

async fn registry_run(
    ctx: &ToolCtx,
    name: &str,
    args: serde_json::Value,
) -> rsh::tools::ToolOutput {
    ToolRegistry::with_builtin_tools()
        .run(name, args, ctx)
        .await
        .expect("tool dispatch")
}

#[test]
fn ask_mode_is_not_a_thing_you_can_silently_configure_away() {
    assert_eq!(PermissionMode::parse("ask"), Some(PermissionMode::Ask));
    assert_eq!(PermissionMode::parse("workspace-write"), None);
    assert_eq!(PermissionMode::default(), PermissionMode::DangerFullAccess);
}

#[tokio::test]
async fn bash_truncates_its_tail_not_its_head() {
    let ctx = ToolCtx::new(workspace()).with_permission(PermissionMode::DangerFullAccess);
    let output = registry_run(
        &ctx,
        "bash",
        json!({"command": "seq 1 20000 | sed -n '1,20000p'; printf 'TAIL_MARKER\\n'"}),
    )
    .await;
    assert!(output.content.contains("TAIL_MARKER"));
    assert!(output.content.chars().count() <= rsh::tools::bash::OUTPUT_LIMIT);
}

#[test]
fn the_default_mode_is_full_access() {
    let ctx = ToolCtx::new(workspace());
    assert_eq!(ctx.permission, PermissionMode::DangerFullAccess);
}

#[allow(dead_code)]
fn assert_tool_object(_tool: &dyn Tool) {}
