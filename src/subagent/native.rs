use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::Agent;
use crate::config::Config;
use crate::error::ToolResult;
use crate::llm::LlmClient;
use crate::skills::SkillRegistry;
use crate::tools::ToolRegistry;

use super::{SubagentResult, SubagentRunner};

pub struct NativeRunner {
    llm: Arc<dyn LlmClient>,
    config: Config,
    skills: Arc<SkillRegistry>,
    workspace: PathBuf,
}

impl NativeRunner {
    #[must_use]
    pub fn new(
        llm: Arc<dyn LlmClient>,
        config: Config,
        skills: Arc<SkillRegistry>,
        workspace: PathBuf,
    ) -> Self {
        Self {
            llm,
            config,
            skills,
            workspace,
        }
    }
}

#[async_trait]
impl SubagentRunner for NativeRunner {
    async fn run(
        &self,
        prompt: &str,
        _backend: Option<&str>,
        cwd: Option<&Path>,
        timeout_secs: Option<u64>,
    ) -> ToolResult<SubagentResult> {
        let workspace = cwd.map_or_else(|| self.workspace.clone(), Path::to_path_buf);
        let tools = Arc::new(ToolRegistry::native_subagent_tools());
        let mut agent = Agent::new(Arc::clone(&self.llm), tools, self.config.clone(), workspace)
            .with_skills(Arc::clone(&self.skills));

        let (tx, mut rx) = mpsc::channel(256);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let timeout = Duration::from_secs(timeout_secs.unwrap_or(600).clamp(10, 3_600));
        let result = tokio::time::timeout(
            timeout,
            agent.run_turn(prompt, tx, CancellationToken::new()),
        )
        .await;
        let _ = drain.await;

        Ok(match result {
            Ok(Ok(outcome)) => {
                SubagentResult::ok(outcome.text, outcome.changed_files, "native", None)
            }
            Ok(Err(error)) => SubagentResult::error(error.to_string(), "native"),
            Err(_) => SubagentResult::error(
                format!("subagent timed out after {}s", timeout.as_secs()),
                "native",
            ),
        })
    }
}
