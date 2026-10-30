pub mod acp;
pub mod dsh;
pub mod native;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::ToolResult;
use crate::llm::LlmClient;
use crate::skills::SkillRegistry;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentResult {
    pub status: String,
    pub summary: String,
    pub files_changed: Vec<PathBuf>,
    pub backend: String,
    pub session_id: Option<String>,
}

impl SubagentResult {
    #[must_use]
    pub fn ok(
        summary: impl Into<String>,
        files_changed: Vec<PathBuf>,
        backend: impl Into<String>,
        session_id: Option<String>,
    ) -> Self {
        Self {
            status: "ok".into(),
            summary: summary.into(),
            files_changed,
            backend: backend.into(),
            session_id,
        }
    }

    #[must_use]
    pub fn error(summary: impl Into<String>, backend: impl Into<String>) -> Self {
        Self {
            status: "error".into(),
            summary: summary.into(),
            files_changed: Vec::new(),
            backend: backend.into(),
            session_id: None,
        }
    }

    /// Render the result as the tool message the main model sees.
    #[must_use]
    pub fn render(&self) -> String {
        let files = if self.files_changed.is_empty() {
            "(none)".to_string()
        } else {
            self.files_changed
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };
        format!(
            "status: {}\nbackend: {}\nsession: {}\nsummary:\n{}\nfiles_changed:\n{}",
            self.status,
            self.backend,
            self.session_id.as_deref().unwrap_or("-"),
            self.summary.trim(),
            files
        )
    }
}

#[async_trait]
pub trait SubagentRunner: Send + Sync {
    async fn run(
        &self,
        prompt: &str,
        backend: Option<&str>,
        cwd: Option<&Path>,
        timeout_secs: Option<u64>,
    ) -> ToolResult<SubagentResult>;
}

/// Selects the native or dsh backend per call.
pub struct Manager {
    native: native::NativeRunner,
    dsh: dsh::DshRunner,
}

impl Manager {
    #[must_use]
    pub fn new(
        llm: Arc<dyn LlmClient>,
        config: Config,
        skills: Arc<SkillRegistry>,
        workspace: PathBuf,
    ) -> Self {
        Self {
            native: native::NativeRunner::new(llm, config.clone(), skills, workspace.clone()),
            dsh: dsh::DshRunner::new(config, workspace),
        }
    }
}

#[async_trait]
impl SubagentRunner for Manager {
    async fn run(
        &self,
        prompt: &str,
        backend: Option<&str>,
        cwd: Option<&Path>,
        timeout_secs: Option<u64>,
    ) -> ToolResult<SubagentResult> {
        match backend.unwrap_or("native") {
            "dsh" => self.dsh.run(prompt, backend, cwd, timeout_secs).await,
            _ => self.native.run(prompt, backend, cwd, timeout_secs).await,
        }
    }
}
