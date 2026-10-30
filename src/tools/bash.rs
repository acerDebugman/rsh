use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::error::{ToolError, ToolResult};

use super::{Tool, ToolCtx, ToolOutput, function_schema};

/// `bash` keeps its tail: the real failure is the last thing a build prints.
pub const OUTPUT_LIMIT: usize = 4_000;

pub struct BashTool;

/// Binaries whose whole purpose is to change the filesystem.
const MUTATING_BINARIES: &[&str] = &[
    "rm", "rmdir", "mv", "cp", "mkdir", "touch", "tee", "truncate", "dd", "chmod", "chown",
    "chgrp", "ln", "install", "patch", "shred", "mkfifo", "mknod", "unlink", "rsync",
];

/// `git` subcommands that write to the worktree, the index, or a remote.
const MUTATING_GIT: &[&str] = &[
    "add",
    "am",
    "apply",
    "branch",
    "checkout",
    "cherry-pick",
    "clean",
    "clone",
    "commit",
    "config",
    "fetch",
    "format-patch",
    "gc",
    "init",
    "merge",
    "mv",
    "pull",
    "push",
    "rebase",
    "reset",
    "restore",
    "revert",
    "rm",
    "stash",
    "submodule",
    "switch",
    "tag",
    "update-ref",
];

/// Subcommands of the common package managers that touch the tree or a lockfile.
const MUTATING_PACKAGE_SUBCOMMANDS: &[&str] = &[
    "add",
    "remove",
    "rm",
    "install",
    "uninstall",
    "update",
    "upgrade",
    "publish",
    "fix",
    "fmt",
    "vendor",
    "clean",
    "prune",
    "link",
    "unlink",
    "new",
    "init",
    "generate",
];

/// Best-effort classifier: does this shell command line look like it writes?
///
/// This backs `--read-only`, which is an escape hatch, not a sandbox. A shell
/// can always obfuscate a write (`python -c`, `curl ... | sh`); the classifier
/// covers what a coding agent actually emits, and it is deliberately allowed
/// to produce false positives — refusing a read in read-only mode is
/// recoverable, running a write is not.
#[must_use]
pub fn mutating_reason(command: &str) -> Option<String> {
    mutating_reason_depth(command, 0)
}

fn mutating_reason_depth(command: &str, depth: usize) -> Option<String> {
    if depth > 3 {
        return None;
    }
    let words = tokenize(command);
    for (index, word) in words.iter().enumerate() {
        if word != ">" && word != ">>" {
            continue;
        }
        let target = words.get(index + 1).map_or("", String::as_str);
        if !target.is_empty() && target != "/dev/null" && !target.starts_with('&') {
            return Some(format!("redirects output to {target}"));
        }
    }
    for segment in words.split(|word| word == ";" || word == "&&" || word == "||" || word == "|") {
        if let Some(reason) = segment_reason(segment, depth) {
            return Some(reason);
        }
    }
    None
}

fn segment_reason(segment: &[String], depth: usize) -> Option<String> {
    // Skip env assignments and transparent launchers: `FOO=1 sudo -n rm x`.
    let mut index = 0;
    while let Some(word) = segment.get(index) {
        let launcher = matches!(
            word.as_str(),
            "sudo" | "env" | "command" | "nohup" | "time" | "exec" | "xargs" | "nice"
        );
        let assignment = word.contains('=') && !word.starts_with('-');
        if !launcher && !assignment {
            break;
        }
        index += 1;
    }
    let head = segment.get(index)?.as_str();
    let args = &segment[index + 1..];

    if matches!(head, "find" | "watch")
        && args
            .iter()
            .any(|word| matches!(word.as_str(), "-delete" | "-exec" | "-execdir"))
    {
        return Some(format!("{head} with an in-place action"));
    }
    if matches!(head, "sed" | "perl")
        && args
            .iter()
            .any(|word| word.starts_with("-i") || word == "--in-place")
    {
        return Some(format!("{head} in place"));
    }
    if head == "git" {
        if let Some(sub) = args.first() {
            if MUTATING_GIT.contains(&sub.as_str()) {
                return Some(format!("git {sub}"));
            }
        }
    }
    if matches!(
        head,
        "npm"
            | "pnpm"
            | "yarn"
            | "pip"
            | "pip3"
            | "cargo"
            | "rustup"
            | "go"
            | "brew"
            | "apt"
            | "apt-get"
    ) {
        if let Some(sub) = args.first() {
            if MUTATING_PACKAGE_SUBCOMMANDS.contains(&sub.as_str()) {
                return Some(format!("{head} {sub}"));
            }
        }
    }
    if MUTATING_BINARIES.contains(&head) {
        return Some(head.to_string());
    }
    if matches!(head, "sh" | "bash" | "zsh" | "dash") {
        for (position, word) in args.iter().enumerate() {
            if word.starts_with('-') && word.contains('c') {
                if let Some(inner) = args.get(position + 1) {
                    return mutating_reason_depth(inner, depth + 1)
                        .map(|reason| format!("{head} -c: {reason}"));
                }
            }
        }
    }
    None
}

/// Split into words; quotes are dropped but keep their spaces, so a quoted
/// shell snippet stays one word and can be re-scanned while a redirect inside
/// `grep "a > b"` is not mistaken for a write. `>`, `>>`, `;`, `&&`, `||` and
/// `|` come out as their own words.
fn tokenize(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (None, '\'' | '"') => quote = Some(ch),
            (Some(open), close) if open == close => quote = None,
            (None, '>' | '<') => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
                let mut operator = ch.to_string();
                if ch == '>' && chars.peek() == Some(&'>') {
                    chars.next();
                    operator.push('>');
                }
                words.push(operator);
            }
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            (None, ';' | '&' | '|') => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
                let mut operator = ch.to_string();
                while chars.peek() == Some(&ch) {
                    chars.next();
                    operator.push(ch);
                }
                words.push(operator);
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

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

    fn output_limit(&self, default_limit: usize) -> usize {
        default_limit.min(OUTPUT_LIMIT)
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

        // `--read-only` refuses anything that looks like a write; a false
        // positive costs a retry, a false negative costs the worktree.
        if let Some(reason) = mutating_reason(command) {
            if let Err(error) = ctx.ensure_writable("bash", &reason) {
                return Ok(ToolOutput::error(error.to_string()));
            }
        }
        // In `ask` mode every distinct command string is approved on its own.
        if let Err(error) = ctx.authorize("bash", command).await {
            return Ok(ToolOutput::error(error.to_string()));
        }

        let mut process = shell_command(command);
        process
            .current_dir(&ctx.workspace)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        detach_process_group(&mut process);

        let child = process.spawn().map_err(ToolError::Io)?;
        let pid = child.id();
        let wait = child.wait_with_output();

        let output = tokio::select! {
            () = ctx.cancel.cancelled() => {
                kill_process_group(pid);
                return Ok(ToolOutput::error("command cancelled"));
            }
            result = tokio::time::timeout(Duration::from_millis(timeout_ms), wait) => {
                match result {
                    Ok(Ok(output)) => output,
                    Ok(Err(error)) => return Err(ToolError::Io(error)),
                    Err(_) => {
                        // `sh -c` grandchildren (rustc, pytest workers) survive a
                        // plain child kill; kill the whole process group.
                        kill_process_group(pid);
                        return Ok(ToolOutput::error(format!(
                            "command timed out after {timeout_ms} ms (process group killed)"
                        )));
                    }
                }
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
        // Keep the tail: compiler and test failures live at the bottom.
        let text = keep_tail(&text, self.output_limit(ctx.output_limit));

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

#[cfg(unix)]
fn detach_process_group(process: &mut Command) {
    // SAFETY: `setsid` is async-signal-safe and runs between fork and exec,
    // where no allocator or lock may be touched.
    unsafe {
        process.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn detach_process_group(_process: &mut Command) {
    // TODO(windows): a Job Object is the equivalent of setsid + killpg here.
}

#[cfg(unix)]
fn kill_process_group(pid: Option<u32>) {
    let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) else {
        return;
    };
    // SAFETY: signalling a process group this process created via `setsid`.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: Option<u32>) {}

/// Keep the last `limit` characters, which is where compiler and test output
/// puts the failure. The marker is counted inside the limit.
#[must_use]
pub fn keep_tail(text: &str, limit: usize) -> String {
    let total = text.chars().count();
    if total <= limit {
        return text.to_string();
    }
    let marker = format!("[... {} chars elided ...]\n", total - limit);
    let keep = limit.saturating_sub(marker.chars().count());
    let tail: String = text.chars().skip(total - keep).collect();
    format!("{marker}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;

    use crate::config::PermissionMode;
    use crate::tools::{Approval, ApprovalRequest, Approver, SessionApprovals};

    fn workspace() -> std::path::PathBuf {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::Builder::new()
            .prefix("bash-")
            .tempdir_in(&base)
            .unwrap()
            .keep()
    }

    struct FixedApprover {
        answer: Approval,
        seen: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Approver for FixedApprover {
        async fn approve(&self, request: &ApprovalRequest) -> ToolResult<Approval> {
            self.seen.lock().unwrap().push(request.detail.clone());
            Ok(self.answer)
        }
    }

    fn asking(answer: Approval) -> (ToolCtx, Arc<FixedApprover>) {
        let approver = Arc::new(FixedApprover {
            answer,
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let ctx = ToolCtx::new(workspace())
            .with_permission(PermissionMode::Ask)
            .with_approver(Some(approver.clone()));
        (ctx, approver)
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

    #[tokio::test]
    async fn cancel_stops_the_command() {
        let ctx = ToolCtx::new(workspace());
        let cancel = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel.cancel();
        });
        let started = std::time::Instant::now();
        let output = BashTool
            .run(json!({"command": "sleep 30"}), &ctx)
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("cancelled"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn ask_mode_denies_before_running_anything() {
        let (ctx, approver) = asking(Approval::Deny);
        let output = BashTool
            .run(json!({"command": "touch denied.txt"}), &ctx)
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("denied"));
        assert_eq!(
            approver.seen.lock().unwrap().as_slice(),
            ["touch denied.txt"]
        );
        assert!(!ctx.workspace.join("denied.txt").exists());
    }

    #[tokio::test]
    async fn ask_mode_runs_an_approved_command() {
        let (ctx, _approver) = asking(Approval::AllowOnce);
        let output = BashTool
            .run(json!({"command": "printf approved"}), &ctx)
            .await
            .unwrap();
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("approved"));
    }

    #[tokio::test]
    async fn allow_session_covers_only_the_identical_command() {
        let (ctx, approver) = asking(Approval::AllowSession);
        BashTool
            .run(json!({"command": "printf a"}), &ctx)
            .await
            .unwrap();
        BashTool
            .run(json!({"command": "printf a"}), &ctx)
            .await
            .unwrap();
        BashTool
            .run(json!({"command": "printf b"}), &ctx)
            .await
            .unwrap();
        let seen = approver.seen.lock().unwrap().clone();
        assert_eq!(seen, vec!["printf a".to_string(), "printf b".to_string()]);
        assert_eq!(ctx.approvals.len(), 2);
    }

    #[tokio::test]
    async fn ask_mode_without_an_approver_refuses_to_run() {
        let ctx = ToolCtx::new(workspace()).with_permission(PermissionMode::Ask);
        let output = BashTool
            .run(json!({"command": "printf nope"}), &ctx)
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("--yolo"), "{}", output.content);
    }

    #[tokio::test]
    async fn full_access_skips_the_approver() {
        let approver = Arc::new(FixedApprover {
            answer: Approval::Deny,
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let ctx = ToolCtx::new(workspace()).with_approver(Some(approver.clone()));
        let output = BashTool
            .run(json!({"command": "printf open"}), &ctx)
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(approver.seen.lock().unwrap().is_empty());
    }

    #[test]
    fn tail_survives_truncation() {
        let text = format!("{}THE_FAILURE", "x".repeat(10_000));
        let kept = keep_tail(&text, 4_000);
        assert!(kept.chars().count() <= 4_000);
        assert!(kept.contains("THE_FAILURE"));
        assert!(kept.contains("chars elided"));
    }

    #[test]
    fn short_output_is_untouched() {
        assert_eq!(keep_tail("hello", 10), "hello");
    }

    #[test]
    fn mutating_commands_are_recognised() {
        for command in [
            "rm -rf build",
            "rm build/out",
            "mv a b",
            "printf x > out.txt",
            "printf x >> out.txt",
            "echo hi | tee log.txt",
            "sed -i 's/a/b/' f.txt",
            "git commit -m x",
            "git add .",
            "cargo add serde",
            "npm install left-pad",
            "find . -name '*.tmp' -delete",
            "FOO=1 sudo rm -rf /tmp/x",
            "sh -c 'rm -rf build'",
            "cargo test && rm -rf target",
            "mkdir -p out",
            "dd if=/dev/zero of=f bs=1 count=1",
        ] {
            assert!(
                mutating_reason(command).is_some(),
                "should be refused in read-only mode: {command}"
            );
        }
    }

    #[test]
    fn read_commands_are_left_alone() {
        for command in [
            "ls -la",
            "cat a.txt",
            "cargo test",
            "cargo build --release",
            "git status",
            "git log --oneline -5",
            "git diff",
            "rg --files",
            "grep -rn 'rm -rf' src/",
            "sed -n '1,5p' a.txt",
            "echo hello",
            "find . -name '*.rs'",
            "python3 --version",
            "cmd 2>/dev/null",
            "cmd >/dev/null 2>&1",
        ] {
            assert_eq!(
                mutating_reason(command),
                None,
                "should be allowed in read-only mode: {command}"
            );
        }
    }

    #[tokio::test]
    async fn read_only_refuses_writes_before_spawning_a_shell() {
        let workspace = workspace();
        let ctx = ToolCtx::new(workspace.clone()).with_permission(PermissionMode::ReadOnly);
        let output = BashTool
            .run(json!({"command": "touch touched.txt"}), &ctx)
            .await
            .unwrap();
        assert!(output.is_error, "{}", output.content);
        assert!(output.content.contains("read-only"));
        assert!(!workspace.join("touched.txt").exists());
    }

    #[tokio::test]
    async fn read_only_still_runs_reads() {
        let workspace = workspace();
        std::fs::write(workspace.join("a.txt"), "hello").unwrap();
        let ctx = ToolCtx::new(workspace).with_permission(PermissionMode::ReadOnly);
        let output = BashTool
            .run(json!({"command": "cat a.txt"}), &ctx)
            .await
            .unwrap();
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("hello"));
    }

    #[test]
    fn approvals_are_keyed_by_tool_and_detail() {
        let approvals = SessionApprovals::default();
        approvals.allow("bash", "ls");
        assert!(approvals.is_allowed("bash", "ls"));
        assert!(!approvals.is_allowed("bash", "ls "));
        assert!(!approvals.is_allowed("path", "ls"));
    }
}
