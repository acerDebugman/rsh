use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::llm::{ChatRequest, LlmClient, LlmEvent, Message, Role, Usage};
use crate::prompt::{PromptContext, system_prompt};
use crate::session::{EventKind, SessionStore};
use crate::skills::SkillRegistry;
use crate::subagent::SubagentRunner;
use crate::tools::{Approver, SessionApprovals, ToolCtx, ToolOutput, ToolRegistry, UserIo};

use super::transcript::render_transcript;

/// Progress emitted while a turn runs, consumed by the CLI/TUI.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    Text(String),
    Reasoning(String),
    ToolStart {
        name: String,
        arguments: String,
    },
    ToolEnd {
        name: String,
        ok: bool,
        content: String,
    },
}

#[derive(Debug, Clone, Default)]
pub struct CompactOutcome {
    pub summary: String,
    /// Compaction spend is billed to the session even though it is not part of
    /// the turn budget.
    pub usage: Usage,
}

#[derive(Debug, Clone, Default)]
pub struct TurnOutcome {
    pub text: String,
    pub steps: usize,
    pub changed_files: Vec<PathBuf>,
    /// Only the API-reported usage counts. Local guesses are not spend.
    pub usage: Usage,
}

pub struct Agent {
    llm: Arc<dyn LlmClient>,
    tools: Arc<ToolRegistry>,
    config: Config,
    workspace: PathBuf,
    messages: Vec<Message>,
    session: Option<SessionStore>,
    skills: Option<Arc<SkillRegistry>>,
    user_io: Option<Arc<dyn UserIo>>,
    subagents: Option<Arc<dyn SubagentRunner>>,
    approver: Option<Arc<dyn Approver>>,
    approvals: Arc<SessionApprovals>,
}

impl Agent {
    #[must_use]
    pub fn new(
        llm: Arc<dyn LlmClient>,
        tools: Arc<ToolRegistry>,
        config: Config,
        workspace: PathBuf,
    ) -> Self {
        Self {
            llm,
            tools,
            config,
            workspace,
            messages: Vec::new(),
            session: None,
            skills: None,
            user_io: None,
            subagents: None,
            approver: None,
            approvals: Arc::new(SessionApprovals::default()),
        }
    }

    #[must_use]
    pub fn with_session(mut self, session: SessionStore) -> Self {
        self.session = Some(session);
        self
    }

    #[must_use]
    pub fn with_skills(mut self, skills: Arc<SkillRegistry>) -> Self {
        self.skills = Some(skills);
        self
    }

    #[must_use]
    pub fn with_user_io(mut self, user_io: Arc<dyn UserIo>) -> Self {
        self.user_io = Some(user_io);
        self
    }

    #[must_use]
    pub fn with_subagents(mut self, subagents: Arc<dyn SubagentRunner>) -> Self {
        self.subagents = Some(subagents);
        self
    }

    #[must_use]
    pub fn with_approver(mut self, approver: Option<Arc<dyn Approver>>) -> Self {
        self.approver = approver;
        self
    }

    /// One approval cache per process: "allow for this session" has to outlive
    /// a single tool call.
    #[must_use]
    pub fn with_approvals(mut self, approvals: Arc<SessionApprovals>) -> Self {
        self.approvals = approvals;
        self
    }

    #[must_use]
    pub fn with_messages(mut self, messages: Vec<Message>) -> Self {
        self.messages = messages;
        self
    }

    /// Replace older messages with a model-written summary, keeping the last
    /// two user turns. The system prompt is preserved.
    pub async fn compact(&mut self, instruction: Option<&str>) -> Result<CompactOutcome> {
        if self.messages.len() < 4 {
            bail!("nothing to compact yet");
        }
        let mut cut = None;
        let mut seen = 0;
        for (index, message) in self.messages.iter().enumerate().rev() {
            if message.role == Role::User {
                seen += 1;
                if seen == 2 {
                    cut = Some(index);
                    break;
                }
            }
        }
        let Some(cut) = cut else {
            bail!("nothing to compact yet");
        };
        if cut <= 1 {
            bail!("nothing to compact yet");
        }
        let transcript = render_transcript(&self.messages[1..cut]);
        let instruction = instruction
            .unwrap_or("Keep goals, constraints, decisions, changed files, and unfinished work.");
        let request = ChatRequest {
            model: self.config.model.clone(),
            messages: vec![
                Message::system("You compress coding conversations. Reply with the summary only."),
                Message::user(format!(
                    "Compress the conversation below into a summary for continuing the work. {instruction}\n\n{transcript}"
                )),
            ],
            tools: Vec::new(),
            max_tokens: self.config.max_tokens.min(4096),
            temperature: 0.0,
            send_reasoning: false,
        };
        let (tx, mut rx) = mpsc::channel(64);
        let drain = tokio::spawn(async move {
            let mut usage = Usage::default();
            while let Some(event) = rx.recv().await {
                if let LlmEvent::Usage(reported) = event {
                    usage = reported;
                }
            }
            usage
        });
        let outcome = self
            .llm
            .stream_chat(request, tx, CancellationToken::new())
            .await;
        let usage = drain.await.unwrap_or_default();
        let outcome = outcome?;
        let summary = outcome.text.trim().to_string();
        if summary.is_empty() {
            bail!("compaction returned an empty summary");
        }

        self.record(EventKind::Compaction {
            summary: summary.clone(),
            replaces_until: cut - 1,
        })?;
        let tail = self.messages.split_off(cut);
        self.messages.truncate(1);
        self.messages
            .push(Message::system(format!("[compaction summary]\n{summary}")));
        self.messages.extend(tail);
        Ok(CompactOutcome { summary, usage })
    }

    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Run one user turn to completion. Tool failures are returned to the
    /// model as `role=tool` messages; only transport/loop errors abort.
    pub async fn run_turn(
        &mut self,
        input: &str,
        events: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    ) -> Result<TurnOutcome> {
        if !matches!(self.messages.first(), Some(message) if message.role == Role::System) {
            self.messages
                .insert(0, Message::system(self.system_prompt()));
        }
        self.messages.push(Message::user(input));
        self.record(EventKind::UserMessage {
            text: input.to_string(),
        })?;

        let mut usage = Usage::default();
        let mut final_text = String::new();
        let mut changed_files: Vec<PathBuf> = Vec::new();
        for step in 0..self.config.max_steps {
            let request = ChatRequest {
                model: self.config.model.clone(),
                messages: self.messages.clone(),
                tools: self.tools.schemas(),
                max_tokens: self.config.max_tokens,
                temperature: self.config.temperature,
                send_reasoning: self.config.send_reasoning,
            };
            let (tx, rx) = mpsc::channel(256);
            let forwarder = spawn_forwarder(rx, events.clone());
            let result = self.llm.stream_chat(request, tx, cancel.clone()).await;
            let step_usage = forwarder.await.unwrap_or_default();
            usage.prompt_tokens += step_usage.prompt_tokens;
            usage.completion_tokens += step_usage.completion_tokens;
            usage.total_tokens += step_usage.total_tokens;
            let outcome = result?;

            if !outcome.text.is_empty() {
                final_text = outcome.text.clone();
            }
            let reasoning = if outcome.reasoning.is_empty() {
                None
            } else {
                Some(outcome.reasoning.clone())
            };
            self.messages.push(Message::assistant(
                outcome.text.clone(),
                reasoning.clone(),
                outcome.tool_calls.clone(),
            ));
            self.record(EventKind::AssistantMessage {
                text: outcome.text,
                reasoning,
                tool_calls: outcome.tool_calls.clone(),
            })?;

            if outcome.tool_calls.is_empty() {
                self.record(EventKind::TurnEnd {
                    reason: "completed".into(),
                })?;
                return Ok(TurnOutcome {
                    text: final_text,
                    steps: step + 1,
                    changed_files,
                    usage,
                });
            }

            for call in outcome.tool_calls {
                let _ = events
                    .send(AgentEvent::ToolStart {
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    })
                    .await;
                let raw = call.arguments.trim();
                let raw = if raw.is_empty() { "{}" } else { raw };
                let output = match serde_json::from_str::<Value>(raw) {
                    Ok(Value::Object(map)) => {
                        let ctx = self.tool_ctx(&cancel);
                        self.tools
                            .run(&call.name, Value::Object(map), &ctx)
                            .await
                            .unwrap_or_else(|error| ToolOutput::error(error.to_string()))
                    }
                    Ok(other) => ToolOutput::error(format!(
                        "tool arguments must be a JSON object, got {other}"
                    )),
                    Err(error) => ToolOutput::error(format!(
                        "invalid JSON arguments for {}: {error}",
                        call.name
                    )),
                };
                changed_files.extend(output.changed_files.iter().cloned());
                let _ = events
                    .send(AgentEvent::ToolEnd {
                        name: call.name.clone(),
                        ok: !output.is_error,
                        content: output.content.clone(),
                    })
                    .await;
                self.record(EventKind::ToolResult {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    args: raw.to_string(),
                    ok: !output.is_error,
                    content: output.content.clone(),
                })?;
                self.messages.push(Message::tool(call.id, output.content));
            }
        }

        self.record(EventKind::TurnEnd {
            reason: "max_steps".into(),
        })?;
        bail!("max steps ({}) exceeded", self.config.max_steps)
    }

    fn tool_ctx(&self, cancel: &CancellationToken) -> ToolCtx {
        ToolCtx {
            workspace: self.workspace.clone(),
            permission: self.config.permissions.mode,
            cancel: cancel.clone(),
            output_limit: self.config.tool_output_limit,
            user_io: self.user_io.clone(),
            skills: self.skills.clone(),
            subagents: self.subagents.clone(),
            approver: self.approver.clone(),
            approvals: Arc::clone(&self.approvals),
        }
    }

    fn system_prompt(&self) -> String {
        let context =
            PromptContext::new(&self.workspace, &self.config).with_skills(self.skills.as_deref());
        system_prompt(&context)
    }

    /// A failed append is a failed turn. Best-effort logging would mean the log
    /// silently disagrees with what the model actually saw.
    fn record(&mut self, kind: EventKind) -> Result<()> {
        let Some(session) = &mut self.session else {
            return Ok(());
        };
        session.append(kind).context("append session event")?;
        Ok(())
    }

    /// Tools currently exposed to the model. Used by the TUI status line.
    #[must_use]
    pub fn tool_names(&self) -> Vec<&'static str> {
        self.tools.names()
    }
}

/// Forward UI events and hand back the API-reported usage. Dropping `Usage`
/// would leave the caller with no way to count spend.
fn spawn_forwarder(
    mut rx: mpsc::Receiver<LlmEvent>,
    events: mpsc::Sender<AgentEvent>,
) -> tokio::task::JoinHandle<Usage> {
    tokio::spawn(async move {
        let mut usage = Usage::default();
        while let Some(event) = rx.recv().await {
            let mapped = match event {
                LlmEvent::Text(text) => Some(AgentEvent::Text(text)),
                LlmEvent::Reasoning(text) => Some(AgentEvent::Reasoning(text)),
                LlmEvent::ToolCall { .. } => None,
                LlmEvent::Usage(reported) => {
                    usage = reported;
                    None
                }
            };
            if let Some(mapped) = mapped {
                if events.send(mapped).await.is_err() {
                    break;
                }
            }
        }
        usage
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::llm::{StreamOutcome, ToolCall};
    use crate::tools::{Tool, ToolResult, function_schema};

    struct MockLlm {
        responses: Mutex<VecDeque<StreamOutcome>>,
    }

    impl MockLlm {
        fn new(responses: Vec<StreamOutcome>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
            }
        }
    }

    #[async_trait]
    impl LlmClient for MockLlm {
        async fn stream_chat(
            &self,
            _request: ChatRequest,
            events: mpsc::Sender<LlmEvent>,
            _cancel: CancellationToken,
        ) -> Result<StreamOutcome> {
            let outcome = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("mock response");
            if !outcome.text.is_empty() {
                let _ = events.send(LlmEvent::Text(outcome.text.clone())).await;
            }
            // Every response reports usage; the loop must not drop it.
            let _ = events
                .send(LlmEvent::Usage(Usage {
                    prompt_tokens: 20,
                    completion_tokens: 10,
                    total_tokens: 30,
                }))
                .await;
            Ok(outcome)
        }
    }

    struct StubTool {
        fail: bool,
    }

    #[async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &'static str {
            "stub"
        }

        fn description(&self) -> &'static str {
            "test tool"
        }

        fn schema(&self) -> Value {
            function_schema("stub", "test tool", json!({"type": "object"}))
        }

        async fn run(&self, _args: Value, _ctx: &ToolCtx) -> ToolResult<ToolOutput> {
            if self.fail {
                Ok(ToolOutput::error("stub failed"))
            } else {
                Ok(ToolOutput::ok("stub ok"))
            }
        }
    }

    fn workspace() -> PathBuf {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp");
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn agent(responses: Vec<StreamOutcome>, max_steps: usize, fail: bool) -> Agent {
        let mut config = Config::default();
        config.max_steps = max_steps;
        let mut tools = ToolRegistry::default();
        tools.register(StubTool { fail });
        Agent::new(
            Arc::new(MockLlm::new(responses)),
            Arc::new(tools),
            config,
            workspace(),
        )
    }

    fn text(value: &str) -> StreamOutcome {
        StreamOutcome {
            text: value.into(),
            ..StreamOutcome::default()
        }
    }

    fn call(id: &str, arguments: &str) -> StreamOutcome {
        StreamOutcome {
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: "stub".into(),
                arguments: arguments.into(),
            }],
            ..StreamOutcome::default()
        }
    }

    #[tokio::test]
    async fn text_only_turn_returns_text() {
        let mut agent = agent(vec![text("hello")], 5, false);
        let (tx, mut rx) = mpsc::channel(16);
        let outcome = agent
            .run_turn("hi", tx, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(outcome.text, "hello");
        assert_eq!(outcome.steps, 1);
        assert_eq!(outcome.usage.total_tokens, 30);
        assert_eq!(outcome.usage.prompt_tokens, 20);
        let mut texts = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::Text(text) = event {
                texts.push(text);
            }
        }
        assert_eq!(texts, vec!["hello"]);
    }

    #[tokio::test]
    async fn tool_call_then_text() {
        let mut agent = agent(vec![call("call_1", "{}"), text("done")], 5, false);
        let (tx, _rx) = mpsc::channel(16);
        let outcome = agent
            .run_turn("do it", tx, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(outcome.text, "done");
        assert_eq!(outcome.steps, 2);
        assert_eq!(outcome.usage.total_tokens, 60, "usage accumulates per step");
        let tool_message = agent
            .messages()
            .iter()
            .find(|message| message.tool_call_id.as_deref() == Some("call_1"))
            .expect("tool message");
        assert_eq!(tool_message.content, "stub ok");
    }

    #[tokio::test]
    async fn tool_failure_is_returned_to_the_model() {
        let mut agent = agent(vec![call("call_1", "{}"), text("recovered")], 5, true);
        let (tx, _rx) = mpsc::channel(16);
        let outcome = agent
            .run_turn("do it", tx, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(outcome.text, "recovered");
        let tool_message = agent
            .messages()
            .iter()
            .find(|message| message.tool_call_id.is_some())
            .expect("tool message");
        assert_eq!(tool_message.content, "stub failed");
    }

    #[tokio::test]
    async fn invalid_arguments_become_a_tool_error() {
        let mut agent = agent(vec![call("call_1", "{oops"), text("done")], 5, false);
        let (tx, _rx) = mpsc::channel(16);
        agent
            .run_turn("do it", tx, CancellationToken::new())
            .await
            .unwrap();
        let tool_message = agent
            .messages()
            .iter()
            .find(|message| message.tool_call_id.is_some())
            .expect("tool message");
        assert!(
            tool_message.content.contains("invalid JSON"),
            "{}",
            tool_message.content
        );
    }

    #[tokio::test]
    async fn compaction_replaces_old_messages_and_keeps_recent_turns() {
        let mut agent = agent(vec![text("SUMMARY")], 5, false);
        agent = agent.with_messages(vec![
            Message::system("sys"),
            Message::user("u1"),
            Message::assistant("a1", None, Vec::new()),
            Message::user("u2"),
            Message::assistant("a2", None, Vec::new()),
            Message::user("u3"),
            Message::assistant("a3", None, Vec::new()),
        ]);
        let outcome = agent.compact(None).await.unwrap();
        assert_eq!(outcome.summary, "SUMMARY");
        assert_eq!(agent.messages().len(), 6);
        assert_eq!(agent.messages()[0].content, "sys");
        assert!(agent.messages()[1].content.contains("SUMMARY"));
        assert_eq!(agent.messages()[2].content, "u2");
    }

    #[tokio::test]
    async fn max_steps_is_enforced() {
        let mut agent = agent(vec![call("call_1", "{}"), call("call_2", "{}")], 2, false);
        let (tx, _rx) = mpsc::channel(16);
        let error = agent
            .run_turn("loop", tx, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("max steps"), "{error}");
    }
}
