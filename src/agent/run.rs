use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, bail};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::llm::{ChatRequest, LlmClient, LlmEvent, Message};
use crate::session::{EventKind, SessionStore};
use crate::tools::{ToolCtx, ToolOutput, ToolRegistry};

const SYSTEM_PROMPT: &str = "\
You are rsh, a terminal coding agent working inside the user's workspace.\n\
Use the provided tools to inspect and modify files. Prefer reading before editing.\n\
Check the [exit code: N] marker on every bash result and investigate failures.\n\
Keep replies short and concrete. Do not invent file contents.";

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
pub struct TurnOutcome {
    pub text: String,
    pub steps: usize,
}

pub struct Agent {
    llm: Arc<dyn LlmClient>,
    tools: Arc<ToolRegistry>,
    config: Config,
    workspace: PathBuf,
    messages: Vec<Message>,
    session: Option<SessionStore>,
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
        }
    }

    #[must_use]
    pub fn with_session(mut self, session: SessionStore) -> Self {
        self.session = Some(session);
        self
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
        if self.messages.is_empty() {
            self.messages.push(Message::system(SYSTEM_PROMPT));
        }
        self.messages.push(Message::user(input));
        self.record(EventKind::UserMessage {
            text: input.to_string(),
        });

        let mut final_text = String::new();
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
            let _ = forwarder.await;
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
            });

            if outcome.tool_calls.is_empty() {
                self.record(EventKind::TurnEnd {
                    reason: "completed".into(),
                });
                return Ok(TurnOutcome {
                    text: final_text,
                    steps: step + 1,
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
                    ok: !output.is_error,
                    content: output.content.clone(),
                });
                self.messages.push(Message::tool(call.id, output.content));
            }
        }

        self.record(EventKind::TurnEnd {
            reason: "max_steps".into(),
        });
        bail!("max steps ({}) exceeded", self.config.max_steps)
    }

    fn tool_ctx(&self, cancel: &CancellationToken) -> ToolCtx {
        ToolCtx {
            workspace: self.workspace.clone(),
            permission: self.config.permissions.mode,
            cancel: cancel.clone(),
            output_limit: self.config.tool_output_limit,
        }
    }

    fn record(&mut self, kind: EventKind) {
        let Some(session) = &mut self.session else {
            return;
        };
        if let Err(error) = session.append(kind) {
            eprintln!("warning: failed to append session event: {error}");
        }
    }
}

fn spawn_forwarder(
    mut rx: mpsc::Receiver<LlmEvent>,
    events: mpsc::Sender<AgentEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            let mapped = match event {
                LlmEvent::Text(text) => Some(AgentEvent::Text(text)),
                LlmEvent::Reasoning(text) => Some(AgentEvent::Reasoning(text)),
                LlmEvent::ToolCall { .. } | LlmEvent::Usage(_) => None,
            };
            if let Some(mapped) = mapped {
                if events.send(mapped).await.is_err() {
                    break;
                }
            }
        }
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
