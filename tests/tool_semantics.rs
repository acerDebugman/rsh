//! One case per builtin tool, deterministic, in a throwaway workspace.
//!
//! Group 1 of the four groups that define "done" for this slice: every tool
//! gets at least one real filesystem exercise, plus the `--read-only` refusal
//! that the permission decision introduced.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use rsh::config::PermissionMode;
use rsh::error::ToolResult;
use rsh::skills::SkillRegistry;
use rsh::subagent::{SubagentResult, SubagentRunner};
use rsh::tools::{ToolCtx, ToolOutput, ToolRegistry};

fn workspace() -> PathBuf {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp");
    std::fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("tool-semantics-")
        .tempdir_in(&base)
        .unwrap()
        .keep()
}

fn full_access(workspace: &Path) -> ToolCtx {
    ToolCtx::new(workspace.to_path_buf())
}

fn read_only(workspace: &Path) -> ToolCtx {
    ToolCtx::new(workspace.to_path_buf()).with_permission(PermissionMode::ReadOnly)
}

async fn run(ctx: &ToolCtx, name: &str, args: serde_json::Value) -> ToolOutput {
    ToolRegistry::with_builtin_tools()
        .run(name, args, ctx)
        .await
        .unwrap_or_else(|error| ToolOutput::error(error.to_string()))
}

#[tokio::test]
async fn read_file_numbers_lines_and_reports_the_remainder() {
    let dir = workspace();
    std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let ctx = full_access(&dir);
    let output = run(&ctx, "read_file", json!({"path": "a.txt", "limit": 2})).await;
    assert!(!output.is_error, "{}", output.content);
    assert!(output.content.contains("1\tone"));
    assert!(output.content.contains("2\ttwo"));
    assert!(output.content.contains("[1 more lines]"));
}

#[tokio::test]
async fn write_file_creates_parents() {
    let dir = workspace();
    let ctx = full_access(&dir);
    let output = run(
        &ctx,
        "write_file",
        json!({"path": "deep/nested/a.txt", "content": "hi"}),
    )
    .await;
    assert!(!output.is_error, "{}", output.content);
    assert_eq!(
        std::fs::read_to_string(dir.join("deep/nested/a.txt")).unwrap(),
        "hi"
    );
    assert_eq!(output.changed_files, vec![dir.join("deep/nested/a.txt")]);
}

#[tokio::test]
async fn edit_file_replaces_and_reports_ambiguity() {
    let dir = workspace();
    std::fs::write(dir.join("a.txt"), "x\nx\ny\n").unwrap();
    let ctx = full_access(&dir);
    let ambiguous = run(
        &ctx,
        "edit_file",
        json!({"path": "a.txt", "old_string": "x", "new_string": "z"}),
    )
    .await;
    assert!(ambiguous.is_error);
    let ok = run(
        &ctx,
        "edit_file",
        json!({"path": "a.txt", "old_string": "x\ny", "new_string": "done"}),
    )
    .await;
    assert!(!ok.is_error, "{}", ok.content);
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "x\ndone\n"
    );
}

#[tokio::test]
async fn bash_reports_output_and_exit_code() {
    let dir = workspace();
    let ctx = full_access(&dir);
    let ok = run(&ctx, "bash", json!({"command": "printf hello"})).await;
    assert!(ok.content.contains("hello"));
    assert!(ok.content.contains("[exit code: 0]"));
    let bad = run(&ctx, "bash", json!({"command": "exit 3"})).await;
    assert!(bad.is_error);
    assert!(bad.content.contains("[exit code: 3]"));
}

#[tokio::test]
async fn glob_and_grep_walk_the_workspace() {
    let dir = workspace();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/a.rs"), "fn main() {}\n").unwrap();
    std::fs::write(dir.join("src/b.txt"), "nothing\n").unwrap();
    let ctx = full_access(&dir);
    let globbed = run(&ctx, "glob", json!({"pattern": "**/*.rs"})).await;
    assert!(globbed.content.contains("a.rs"), "{}", globbed.content);
    assert!(!globbed.content.contains("b.txt"));
    let grepped = run(&ctx, "grep", json!({"pattern": "fn main"})).await;
    assert!(grepped.content.contains("a.rs:1"), "{}", grepped.content);
}

#[tokio::test]
async fn skill_loads_from_the_project_directory() {
    let dir = workspace();
    let skill_dir = dir.join(".agents/skills/demo");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), "# demo\n\nDo the demo thing.\n").unwrap();
    let skills = Arc::new(SkillRegistry::discover(&dir));
    let ctx = full_access(&dir).with_skills(Some(skills));
    let output = run(&ctx, "skill", json!({"name": "demo"})).await;
    assert!(!output.is_error, "{}", output.content);
    assert!(output.content.contains("Do the demo thing."));
}

#[tokio::test]
async fn ask_user_without_a_surface_says_so_instead_of_hanging() {
    let dir = workspace();
    let ctx = full_access(&dir);
    let output = run(&ctx, "ask_user", json!({"question": "which?"})).await;
    assert!(output.is_error);
    assert!(output.content.contains("interactive"));
}

struct FixedRunner;

#[async_trait]
impl SubagentRunner for FixedRunner {
    async fn run(
        &self,
        prompt: &str,
        backend: Option<&str>,
        _cwd: Option<&Path>,
        _timeout_secs: Option<u64>,
    ) -> ToolResult<SubagentResult> {
        Ok(SubagentResult::ok(
            format!("handled: {prompt}"),
            Vec::new(),
            backend.unwrap_or("native"),
            None,
        ))
    }
}

#[tokio::test]
async fn subagent_renders_backend_and_summary() {
    let dir = workspace();
    let ctx = full_access(&dir).with_subagents(Some(Arc::new(FixedRunner)));
    let output = run(
        &ctx,
        "subagent",
        json!({"prompt": "find the bug", "backend": "dsh"}),
    )
    .await;
    assert!(!output.is_error, "{}", output.content);
    assert!(output.content.contains("backend: dsh"));
    assert!(output.content.contains("handled: find the bug"));
}

#[tokio::test]
async fn subagent_without_a_backend_is_an_error_result_not_a_panic() {
    let dir = workspace();
    let ctx = full_access(&dir);
    let output = run(&ctx, "subagent", json!({"prompt": "x"})).await;
    assert!(output.is_error);
    assert!(output.content.contains("not configured"));
}

#[tokio::test]
async fn read_only_refuses_the_two_writing_tools_and_keeps_reads_working() {
    let dir = workspace();
    std::fs::write(dir.join("a.txt"), "original\n").unwrap();
    let ctx = read_only(&dir);

    let write = run(
        &ctx,
        "write_file",
        json!({"path": "new.txt", "content": "x"}),
    )
    .await;
    assert!(write.is_error);
    assert!(write.content.contains("read-only"));
    assert!(!dir.join("new.txt").exists());

    let edit = run(
        &ctx,
        "edit_file",
        json!({"path": "a.txt", "old_string": "original", "new_string": "changed"}),
    )
    .await;
    assert!(edit.is_error);
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "original\n"
    );

    let read = run(&ctx, "read_file", json!({"path": "a.txt"})).await;
    assert!(!read.is_error, "{}", read.content);
    assert!(read.content.contains("original"));
}

#[tokio::test]
async fn read_only_refuses_a_write_hidden_in_a_shell_command() {
    let dir = workspace();
    let ctx = read_only(&dir);
    let output = run(&ctx, "bash", json!({"command": "touch created.txt"})).await;
    assert!(output.is_error);
    assert!(output.content.contains("read-only"));
    assert!(!dir.join("created.txt").exists());

    let listed = run(&ctx, "bash", json!({"command": "ls"})).await;
    assert!(!listed.is_error, "{}", listed.content);
}
