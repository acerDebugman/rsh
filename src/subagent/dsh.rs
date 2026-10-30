use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::Config;
use crate::error::ToolResult;

use super::{SubagentResult, SubagentRunner, acp};

/// Drives the dsh-tui automation CLI (`dst run`) or the official headless
/// profile. ACP is attempted by `run_acp` first; this type owns the fallback.
pub struct DshRunner {
    config: Config,
    workspace: PathBuf,
}

impl DshRunner {
    #[must_use]
    pub fn new(config: Config, workspace: PathBuf) -> Self {
        Self { config, workspace }
    }

    async fn run_cli(
        &self,
        prompt: &str,
        cwd: &Path,
        timeout_secs: u64,
    ) -> Result<SubagentResult, String> {
        let key = self.config.api_key().map_err(|error| error.to_string())?;
        let timeout_ms = timeout_secs.saturating_mul(1_000);
        let mut command = Command::new("dst");
        command
            .arg("run")
            .arg("--cwd")
            .arg(cwd)
            .arg("--provider")
            .arg("deepseek-official")
            .arg("--model")
            .arg(&self.config.model)
            .arg("--permission-mode")
            .arg("danger-full-access")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--timeout")
            .arg(timeout_ms.to_string())
            .arg(prompt)
            .current_dir(cwd)
            .env("DEEPSEEK_API_KEY", key)
            .env("DSH_PERMISSION_MODE", "danger-full-access")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let child = command
            .spawn()
            .map_err(|error| format!("spawn dst: {error}"))?;
        let output = tokio::time::timeout(
            Duration::from_millis(timeout_ms + 30_000),
            child.wait_with_output(),
        )
        .await
        .map_err(|_| "dst timed out".to_string())?
        .map_err(|error| format!("dst wait: {error}"))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut text = String::new();
        let mut failure = None;
        let mut session_id = None;
        for line in stdout.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            match value["type"].as_str() {
                Some("session") => {
                    session_id = value["sessionId"].as_str().map(str::to_string);
                }
                Some("result") => {
                    if let Some(value) = value["text"].as_str() {
                        text = value.to_string();
                    }
                }
                Some("error") => {
                    failure = Some(
                        value["message"]
                            .as_str()
                            .unwrap_or("dst reported an error")
                            .to_string(),
                    );
                }
                _ => {}
            }
        }
        if let Some(failure) = failure {
            return Ok(SubagentResult::error(failure, "dsh-cli"));
        }
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("dst exited {}: {stderr}", output.status));
        }
        Ok(SubagentResult::ok(text, Vec::new(), "dsh-cli", session_id))
    }

    async fn run_headless(
        &self,
        prompt: &str,
        cwd: &Path,
        timeout_secs: u64,
    ) -> Result<SubagentResult, String> {
        let key = self.config.api_key().map_err(|error| error.to_string())?;
        let timeout_ms = timeout_secs.saturating_mul(1_000);
        let mut command = Command::new("dsh");
        command
            .args(["--profile", "headless", "--", "--json", "-"])
            .current_dir(cwd)
            .env("DEEPSEEK_API_KEY", key)
            .env("DSH_PERMISSION_MODE", "danger-full-access")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command
            .spawn()
            .map_err(|error| format!("spawn dsh: {error}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(prompt.as_bytes())
                .await
                .map_err(|error| format!("write dsh stdin: {error}"))?;
        }
        let output = tokio::time::timeout(
            Duration::from_millis(timeout_ms + 30_000),
            child.wait_with_output(),
        )
        .await
        .map_err(|_| "dsh timed out".to_string())?
        .map_err(|error| format!("dsh wait: {error}"))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut text = String::new();
        let mut failure = None;
        let mut session_id = None;
        for line in stdout.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            match value["type"].as_str() {
                Some("session") => {
                    session_id = value["sessionId"].as_str().map(str::to_string);
                }
                Some("final") => {
                    if let Some(value) = value["text"].as_str() {
                        text = value.to_string();
                    }
                }
                Some("error") => {
                    failure = Some(
                        value["message"]
                            .as_str()
                            .unwrap_or("dsh reported an error")
                            .to_string(),
                    );
                }
                _ => {}
            }
        }
        if let Some(failure) = failure {
            return Ok(SubagentResult::error(failure, "dsh-headless"));
        }
        if !output.status.success() {
            return Err(format!("dsh exited {}", output.status));
        }
        Ok(SubagentResult::ok(
            text,
            Vec::new(),
            "dsh-headless",
            session_id,
        ))
    }
}

#[async_trait]
impl SubagentRunner for DshRunner {
    async fn run(
        &self,
        prompt: &str,
        _backend: Option<&str>,
        cwd: Option<&Path>,
        timeout_secs: Option<u64>,
    ) -> ToolResult<SubagentResult> {
        let cwd = cwd.map_or_else(|| self.workspace.clone(), Path::to_path_buf);
        let timeout_secs = timeout_secs.unwrap_or(600).clamp(10, 3_600);

        if std::env::var("RSH_DSH_TRANSPORT").ok().as_deref() != Some("cli") {
            match tokio::time::timeout(
                Duration::from_secs(timeout_secs),
                acp::run(
                    prompt,
                    &cwd,
                    &self.config,
                    Duration::from_secs(timeout_secs),
                ),
            )
            .await
            {
                Ok(Ok(result)) => return Ok(result),
                Ok(Err(error)) => {
                    eprintln!("dsh acp failed ({error}); falling back to CLI");
                }
                Err(_) => eprintln!("dsh acp timed out; falling back to CLI"),
            }
        }

        match self.run_cli(prompt, &cwd, timeout_secs).await {
            Ok(result) => Ok(result),
            Err(dst_error) => match self.run_headless(prompt, &cwd, timeout_secs).await {
                Ok(result) => Ok(result),
                Err(dsh_error) => Ok(SubagentResult::error(
                    format!("dsh backends failed: dst: {dst_error}; dsh: {dsh_error}"),
                    "dsh",
                )),
            },
        }
    }
}
