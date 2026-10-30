mod render;
mod terminal;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentEvent, CompactOutcome, TurnOutcome};
use crate::config::{Config, PermissionMode};
use crate::error::ToolError;
use crate::llm::{LlmClient, OpenAiClient};
use crate::session::{SessionStore, default_root, replay};
use crate::skills::SkillRegistry;
use crate::subagent::Manager;
use crate::tools::{
    Approval, ApprovalRequest, Approver, AskOutcome, BashTool, SessionApprovals, Tool, ToolCtx,
    ToolOutput, ToolRegistry, UserIo,
};

pub(crate) struct UiState {
    pub(crate) input: String,
    pub(crate) busy: bool,
    /// One turn waiting for the current one to finish. A second `Enter` while
    /// busy replaces it instead of silently dropping the text.
    pub(crate) queued: Option<String>,
    pub(crate) pending: Vec<String>,
    pub(crate) lines: Vec<String>,
    pub(crate) stream_buf: String,
    pub(crate) quit: bool,
    pub(crate) model: String,
    pub(crate) cwd: String,
    pub(crate) question: Option<PendingQuestion>,
    pub(crate) approval: Option<PendingApproval>,
    pub(crate) permission: PermissionMode,
    skills: Arc<SkillRegistry>,
    current_cancel: Option<CancellationToken>,
    approvals: Arc<SessionApprovals>,
    approver: Option<Arc<dyn Approver>>,
}

pub(crate) struct PendingQuestion {
    reply: oneshot::Sender<AskOutcome>,
}

pub(crate) struct PendingApproval {
    request: ApprovalRequest,
    reply: oneshot::Sender<Approval>,
}

impl UiState {
    fn new(model: String, cwd: String, skills: Arc<SkillRegistry>) -> Self {
        Self {
            input: String::new(),
            busy: false,
            queued: None,
            pending: Vec::new(),
            lines: Vec::new(),
            stream_buf: String::new(),
            quit: false,
            model,
            cwd,
            question: None,
            approval: None,
            permission: PermissionMode::DangerFullAccess,
            skills,
            current_cancel: None,
            approvals: Arc::new(SessionApprovals::default()),
            approver: None,
        }
    }

    /// Wire the same permission mode, approval cache, and approval surface the
    /// agent uses, so `!cmd` cannot bypass `ask` mode.
    fn with_access(
        mut self,
        permission: PermissionMode,
        approvals: Arc<SessionApprovals>,
        approver: Arc<dyn Approver>,
    ) -> Self {
        self.permission = permission;
        self.approvals = approvals;
        self.approver = Some(approver);
        self
    }

    fn push_line(&mut self, line: impl Into<String>) {
        self.pending.push(line.into());
    }
}

enum Command {
    Turn {
        input: String,
        cancel: CancellationToken,
        reply: oneshot::Sender<Result<TurnOutcome>>,
    },
    Compact {
        instruction: Option<String>,
        reply: oneshot::Sender<Result<CompactOutcome>>,
    },
}

enum UiMsg {
    Agent(AgentEvent),
    TurnDone(Result<TurnOutcome, String>),
    ShellDone(Result<ToolOutput, String>),
    CompactDone(Result<CompactOutcome, String>),
    Question {
        question: String,
        options: Vec<String>,
        reply: oneshot::Sender<AskOutcome>,
    },
    Approval {
        request: ApprovalRequest,
        reply: oneshot::Sender<Approval>,
    },
}

struct TuiUserIo {
    ui_tx: mpsc::UnboundedSender<UiMsg>,
}

#[async_trait]
impl UserIo for TuiUserIo {
    async fn ask(&self, question: &str, options: &[String]) -> Result<AskOutcome, ToolError> {
        let (reply, response) = oneshot::channel();
        self.ui_tx
            .send(UiMsg::Question {
                question: question.to_string(),
                options: options.to_vec(),
                reply,
            })
            .map_err(|_| ToolError::Other("UI is closed".into()))?;
        response
            .await
            .map_err(|_| ToolError::Other("UI is closed".into()))
    }
}

struct TuiApprover {
    ui_tx: mpsc::UnboundedSender<UiMsg>,
}

#[async_trait]
impl Approver for TuiApprover {
    async fn approve(&self, request: &ApprovalRequest) -> Result<Approval, ToolError> {
        let (reply, response) = oneshot::channel();
        self.ui_tx
            .send(UiMsg::Approval {
                request: request.clone(),
                reply,
            })
            .map_err(|_| ToolError::Other("UI is closed".into()))?;
        response
            .await
            .map_err(|_| ToolError::Other("approval was cancelled".into()))
    }
}

pub async fn run(config: Config, workspace: PathBuf, resume: Option<PathBuf>) -> Result<()> {
    let client: Arc<dyn LlmClient> = Arc::new(OpenAiClient::from_config(&config)?);
    let tools = Arc::new(ToolRegistry::with_builtin_tools());
    let skills = Arc::new(SkillRegistry::discover(&workspace));
    let manager = Arc::new(Manager::new(
        Arc::clone(&client),
        config.clone(),
        Arc::clone(&skills),
        workspace.clone(),
    ));
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
    let user_io = Arc::new(TuiUserIo {
        ui_tx: ui_tx.clone(),
    });
    let approver: Arc<dyn Approver> = Arc::new(TuiApprover {
        ui_tx: ui_tx.clone(),
    });
    let approvals = Arc::new(SessionApprovals::default());

    let mut agent = Agent::new(
        client,
        Arc::clone(&tools),
        config.clone(),
        workspace.clone(),
    )
    .with_skills(Arc::clone(&skills))
    .with_user_io(user_io)
    .with_subagents(manager)
    .with_approver(Some(Arc::clone(&approver)))
    .with_approvals(Arc::clone(&approvals));
    let mut resumed_session = None;
    match resume {
        Some(path) => {
            let events = SessionStore::load(&path)?;
            agent = agent.with_messages(replay(&events));
            match SessionStore::open(&path) {
                Ok(store) => {
                    resumed_session = Some(store.session_id().to_string());
                    agent = agent.with_session(store);
                }
                Err(error) => eprintln!("warning: session log disabled: {error}"),
            }
        }
        None => match SessionStore::create(&default_root(), &workspace, &config.model) {
            Ok(session) => agent = agent.with_session(session),
            Err(error) => eprintln!("warning: session log disabled: {error}"),
        },
    }

    let (command_tx, command_rx) = mpsc::unbounded_channel();
    tokio::spawn(agent_actor(agent, command_rx, ui_tx.clone()));

    terminal::install_panic_hook();
    let (mut terminal, mut mode) = terminal::init()?;
    let _guard = terminal::Guard;

    // Start reading keys only after the terminal is initialized: the inline
    // viewport's cursor-position query is answered on stdin, and a reader
    // running earlier would steal the CPR response.
    let (key_tx, mut key_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        loop {
            match crossterm::event::read() {
                Ok(event) => {
                    if key_tx.send(event).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    let mut state = UiState::new(
        config.model.clone(),
        workspace.display().to_string(),
        Arc::clone(&skills),
    )
    .with_access(config.permissions.mode, approvals, approver);
    if config.permissions.mode == PermissionMode::Ask {
        state
            .push_line("permission mode: ask - every bash command and escaped path needs approval");
    }
    if let Some(session) = resumed_session {
        state.push_line(format!("resumed session {session}"));
    }

    loop {
        if let Err(error) = terminal.draw(|frame| render::draw(frame, &state, mode)) {
            if mode == terminal::ScreenMode::Inline {
                eprintln!("tui: inline viewport failed ({error}); falling back to fullscreen");
                terminal = terminal::init_fullscreen()?;
                mode = terminal::ScreenMode::Fullscreen;
                continue;
            }
            return Err(error.into());
        }
        if let Err(error) = render::flush_pending(&mut terminal, &mut state, mode) {
            if mode == terminal::ScreenMode::Inline {
                eprintln!("tui: inline insert failed ({error}); falling back to fullscreen");
                terminal = terminal::init_fullscreen()?;
                mode = terminal::ScreenMode::Fullscreen;
                continue;
            }
            return Err(error);
        }
        if state.quit {
            break;
        }
        tokio::select! {
            Some(event) = key_rx.recv() => {
                handle_terminal_event(&mut state, event, &command_tx, &ui_tx, &workspace);
            }
            Some(message) = ui_rx.recv() => {
                handle_ui_message(&mut state, message, &command_tx, &ui_tx, &workspace)
            }
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }

    Ok(())
}

async fn agent_actor(
    mut agent: Agent,
    mut commands: mpsc::UnboundedReceiver<Command>,
    ui_tx: mpsc::UnboundedSender<UiMsg>,
) {
    while let Some(command) = commands.recv().await {
        match command {
            Command::Turn {
                input,
                cancel,
                reply,
            } => {
                let (event_tx, mut event_rx) = mpsc::channel(256);
                let ui = ui_tx.clone();
                let forwarder = tokio::spawn(async move {
                    while let Some(event) = event_rx.recv().await {
                        if ui.send(UiMsg::Agent(event)).is_err() {
                            break;
                        }
                    }
                });
                let result = agent.run_turn(&input, event_tx, cancel).await;
                let _ = forwarder.await;
                let _ = reply.send(result);
            }
            Command::Compact { instruction, reply } => {
                let result = agent.compact(instruction.as_deref()).await;
                let _ = reply.send(result);
            }
        }
    }
}

fn handle_terminal_event(
    state: &mut UiState,
    event: Event,
    command_tx: &mpsc::UnboundedSender<Command>,
    ui_tx: &mpsc::UnboundedSender<UiMsg>,
    workspace: &Path,
) {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            handle_key(state, key, command_tx, ui_tx, workspace);
        }
        Event::Paste(text) => {
            if !state.busy || state.question.is_some() {
                state
                    .input
                    .push_str(&text.replace("\r\n", "\n").replace('\r', ""));
            }
        }
        _ => {}
    }
}

fn handle_key(
    state: &mut UiState,
    key: KeyEvent,
    command_tx: &mpsc::UnboundedSender<Command>,
    ui_tx: &mpsc::UnboundedSender<UiMsg>,
    workspace: &Path,
) {
    if state.approval.is_some() {
        match key.code {
            KeyCode::Char('y') => resolve_approval(state, Approval::AllowOnce),
            KeyCode::Char('a') => resolve_approval(state, Approval::AllowSession),
            KeyCode::Char('n') | KeyCode::Esc => resolve_approval(state, Approval::Deny),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                resolve_approval(state, Approval::Deny);
            }
            _ => {}
        }
        return;
    }
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if state.question.is_some() {
                cancel_question(state);
            } else if state.busy {
                cancel_turn(state);
            } else if state.input.is_empty() {
                state.quit = true;
            } else {
                state.input.clear();
            }
        }
        KeyCode::Char('d')
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && state.input.is_empty()
                && !state.busy =>
        {
            state.quit = true;
        }
        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.input.push('\n');
        }
        KeyCode::Enter => {
            if state.question.is_some() {
                answer_question(state);
            } else if state.busy {
                queue_input(state);
            } else {
                submit(state, command_tx, ui_tx, workspace);
            }
        }
        KeyCode::Esc => {
            if state.question.is_some() {
                cancel_question(state);
            } else if state.busy {
                cancel_turn(state);
            } else {
                state.input.clear();
            }
        }
        KeyCode::Backspace => {
            state.input.pop();
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            state.input.push(character);
        }
        _ => {}
    }
}

fn submit(
    state: &mut UiState,
    command_tx: &mpsc::UnboundedSender<Command>,
    ui_tx: &mpsc::UnboundedSender<UiMsg>,
    workspace: &Path,
) {
    let input = std::mem::take(&mut state.input);
    dispatch_input(state, input, command_tx, ui_tx, workspace);
}

/// Route one composer payload: slash command, `!shell`, or a model turn.
fn dispatch_input(
    state: &mut UiState,
    input: String,
    command_tx: &mpsc::UnboundedSender<Command>,
    ui_tx: &mpsc::UnboundedSender<UiMsg>,
    workspace: &Path,
) {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return;
    }
    if let Some(command) = trimmed.strip_prefix('/') {
        if command == "compact" || command.starts_with("compact ") {
            let instruction = command.trim_start_matches("compact").trim();
            let instruction = if instruction.is_empty() {
                None
            } else {
                Some(instruction.to_string())
            };
            start_compact(state, instruction, command_tx, ui_tx);
            return;
        }
        if let Some(args) = command.strip_prefix("skill") {
            let args = args.trim_start();
            if args.is_empty() {
                state.push_line("usage: /skill <name> [task]");
                return;
            }
            let mut parts = args.splitn(2, char::is_whitespace);
            let name = parts.next().unwrap_or("");
            let task = parts.next().unwrap_or("").trim();
            match state.skills.get(name) {
                Some(skill) => {
                    let mut payload = skill.render();
                    if !task.is_empty() {
                        payload.push_str("\n\nUser task: ");
                        payload.push_str(task);
                    }
                    state.push_line(format!("› /skill {name}"));
                    start_turn(state, payload, command_tx, ui_tx);
                }
                None => state.push_line(format!("unknown skill: {name}")),
            }
            return;
        }
        handle_slash(state, command);
        return;
    }
    if let Some(command) = trimmed.strip_prefix('!') {
        run_shell(state, ui_tx, workspace, command.trim());
        return;
    }

    state.push_line(format!("› {input}"));
    start_turn(state, input, command_tx, ui_tx);
}

fn start_turn(
    state: &mut UiState,
    input: String,
    command_tx: &mpsc::UnboundedSender<Command>,
    ui_tx: &mpsc::UnboundedSender<UiMsg>,
) {
    state.busy = true;
    let cancel = CancellationToken::new();
    state.current_cancel = Some(cancel.clone());
    let (reply_tx, reply_rx) = oneshot::channel();
    let _ = command_tx.send(Command::Turn {
        input,
        cancel,
        reply: reply_tx,
    });
    let ui = ui_tx.clone();
    tokio::spawn(async move {
        let result = match reply_rx.await {
            Ok(result) => result.map_err(|error| error.to_string()),
            Err(error) => Err(format!("agent task dropped: {error}")),
        };
        let _ = ui.send(UiMsg::TurnDone(result));
    });
}

fn start_compact(
    state: &mut UiState,
    instruction: Option<String>,
    command_tx: &mpsc::UnboundedSender<Command>,
    ui_tx: &mpsc::UnboundedSender<UiMsg>,
) {
    state.push_line("… compacting");
    state.busy = true;
    let (reply_tx, reply_rx) = oneshot::channel();
    let _ = command_tx.send(Command::Compact {
        instruction,
        reply: reply_tx,
    });
    let ui = ui_tx.clone();
    tokio::spawn(async move {
        let result = match reply_rx.await {
            Ok(result) => result.map_err(|error| error.to_string()),
            Err(error) => Err(format!("agent task dropped: {error}")),
        };
        let _ = ui.send(UiMsg::CompactDone(result));
    });
}

fn answer_question(state: &mut UiState) {
    let answer = std::mem::take(&mut state.input);
    if let Some(question) = state.question.take() {
        state.push_line(format!("↳ {answer}"));
        let _ = question.reply.send(AskOutcome::Answered(answer));
    }
}

fn cancel_question(state: &mut UiState) {
    if let Some(question) = state.question.take() {
        // Never an empty answer: cancellation is its own outcome.
        let _ = question.reply.send(AskOutcome::Cancelled);
    }
}

fn resolve_approval(state: &mut UiState, decision: Approval) {
    if let Some(approval) = state.approval.take() {
        let label = match decision {
            Approval::AllowOnce => "allowed once",
            Approval::AllowSession => "allowed for this session",
            Approval::Deny => "denied",
        };
        state.push_line(format!("  {label}: {}", approval.request.detail));
        let _ = approval.reply.send(decision);
    }
}

fn start_queued(
    state: &mut UiState,
    command_tx: &mpsc::UnboundedSender<Command>,
    ui_tx: &mpsc::UnboundedSender<UiMsg>,
    workspace: &Path,
) {
    let Some(input) = state.queued.take() else {
        return;
    };
    dispatch_input(state, input, command_tx, ui_tx, workspace);
}

/// Busy + Enter: keep one turn on deck instead of dropping the text.
fn queue_input(state: &mut UiState) {
    let input = std::mem::take(&mut state.input);
    let preview = truncate(input.trim(), 80);
    if preview.is_empty() {
        return;
    }
    let replaced = state.queued.is_some();
    state.queued = Some(input);
    state.push_line(if replaced {
        format!("queued (replaced): {preview}")
    } else {
        format!("queued: {preview}")
    });
}

fn handle_slash(state: &mut UiState, command: &str) {
    match command.split_whitespace().next().unwrap_or("") {
        "quit" | "exit" => state.quit = true,
        "help" => {
            for line in [
                "/help        this list",
                "/model       show the active model",
                "/clear       start a fresh visual segment",
                "/quit        exit rsh",
                "!<command>   run a shell command directly",
            ] {
                state.push_line(line);
            }
        }
        "model" => {
            let model = state.model.clone();
            state.push_line(format!("model: {model}"));
        }
        "clear" => state.push_line("── new segment ──"),
        "" => {}
        other => state.push_line(format!("unknown command: /{other}")),
    }
}

fn run_shell(
    state: &mut UiState,
    ui_tx: &mpsc::UnboundedSender<UiMsg>,
    workspace: &Path,
    command: &str,
) {
    if command.is_empty() {
        state.push_line("usage: ! <shell command>");
        return;
    }
    state.busy = true;
    let ui = ui_tx.clone();
    let workspace = workspace.to_path_buf();
    let command = command.to_string();
    // `!cmd` is a bash call like any other: it goes through the same mode,
    // approval cache, and approval surface as the model's bash tool.
    let context = ToolCtx::new(workspace)
        .with_permission(state.permission)
        .with_approvals(Arc::clone(&state.approvals))
        .with_approver(state.approver.clone());
    tokio::spawn(async move {
        let result = BashTool
            .run(serde_json::json!({"command": command}), &context)
            .await
            .map_err(|error| error.to_string());
        let _ = ui.send(UiMsg::ShellDone(result));
    });
}

fn cancel_turn(state: &mut UiState) {
    if let Some(cancel) = &state.current_cancel {
        cancel.cancel();
    }
}

fn handle_ui_message(
    state: &mut UiState,
    message: UiMsg,
    command_tx: &mpsc::UnboundedSender<Command>,
    ui_tx: &mpsc::UnboundedSender<UiMsg>,
    workspace: &Path,
) {
    match message {
        UiMsg::Agent(AgentEvent::Text(text)) => {
            state.stream_buf.push_str(&text);
            drain_complete_lines(state);
        }
        UiMsg::Agent(AgentEvent::Reasoning(_)) => {}
        UiMsg::Agent(AgentEvent::ToolStart { name, arguments }) => {
            flush_stream(state);
            state.push_line(format!("⚙ {name} {}", truncate(&arguments, 120)));
        }
        UiMsg::Agent(AgentEvent::ToolEnd { name, ok, .. }) => {
            state.push_line(format!("{} {name}", if ok { "✓" } else { "✗" }));
        }
        UiMsg::Approval { request, reply } => {
            flush_stream(state);
            state.push_line(format!(
                "⚠ approve {}: {}",
                request.tool,
                truncate(&request.detail, 160)
            ));
            state.push_line("  y allow · a allow for session · n deny");
            state.approval = Some(PendingApproval { request, reply });
        }
        UiMsg::Question {
            question,
            options,
            reply,
        } => {
            state.push_line(format!("? {question}"));
            for (index, option) in options.iter().enumerate() {
                state.push_line(format!("  {}. {option}", index + 1));
            }
            state.question = Some(PendingQuestion { reply });
        }
        UiMsg::TurnDone(result) => {
            flush_stream(state);
            state.busy = false;
            state.current_cancel = None;
            state.question = None;
            if let Err(error) = result {
                state.push_line(format!("! {error}"));
            }
            start_queued(state, command_tx, ui_tx, workspace);
        }
        UiMsg::CompactDone(result) => {
            state.busy = false;
            match result {
                Ok(outcome) => state.push_line(format!(
                    "✓ compacted: {} · {} tokens",
                    outcome.summary, outcome.usage.total_tokens
                )),
                Err(error) => state.push_line(format!("! compact failed: {error}")),
            }
        }
        UiMsg::ShellDone(result) => {
            state.busy = false;
            match result {
                Ok(output) => {
                    for line in output.content.lines() {
                        state.push_line(line.to_string());
                    }
                }
                Err(error) => state.push_line(format!("! {error}")),
            }
            start_queued(state, command_tx, ui_tx, workspace);
        }
    }
}

fn drain_complete_lines(state: &mut UiState) {
    while let Some(position) = state.stream_buf.find('\n') {
        let line: String = state.stream_buf.drain(..=position).collect();
        state.pending.push(line.trim_end_matches('\n').to_string());
    }
}

fn flush_stream(state: &mut UiState) {
    if state.stream_buf.is_empty() {
        return;
    }
    let rest = std::mem::take(&mut state.stream_buf);
    for line in rest.split('\n') {
        state.pending.push(line.to_string());
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut shortened: String = text.chars().take(limit).collect();
    shortened.push('…');
    shortened
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DenyApprover;

    #[async_trait]
    impl Approver for DenyApprover {
        async fn approve(&self, _request: &ApprovalRequest) -> Result<Approval, ToolError> {
            Ok(Approval::Deny)
        }
    }

    fn test_state() -> UiState {
        UiState::new("m".into(), "cwd".into(), Arc::new(SkillRegistry::default()))
    }

    fn scratch() -> PathBuf {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::Builder::new()
            .prefix("tui-")
            .tempdir_in(&base)
            .unwrap()
            .keep()
    }

    #[tokio::test]
    async fn shell_shortcut_cannot_bypass_ask_mode() {
        let workspace = scratch();
        let mut state = test_state().with_access(
            PermissionMode::Ask,
            Arc::new(SessionApprovals::default()),
            Arc::new(DenyApprover),
        );
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        run_shell(&mut state, &ui_tx, &workspace, "touch bypass.txt");
        assert!(state.busy);

        // A denial is a tool error result, not a crash: the transcript should
        // say why and the command must not have run.
        match ui_rx.recv().await.expect("shell result") {
            UiMsg::ShellDone(Ok(output)) => {
                assert!(output.is_error, "{}", output.content);
                assert!(output.content.contains("denied"), "{}", output.content);
            }
            UiMsg::ShellDone(Err(error)) => panic!("unexpected transport error: {error}"),
            _ => panic!("unexpected message"),
        }
        assert!(!workspace.join("bypass.txt").exists());
        assert!(!state.approvals.is_allowed("bash", "touch bypass.txt"));
    }

    #[tokio::test]
    async fn the_tui_approver_asks_and_forwards_the_decision() {
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let approver = TuiApprover {
            ui_tx: ui_tx.clone(),
        };
        let request = ApprovalRequest {
            tool: "bash",
            detail: "rm -rf build".into(),
        };
        let asking = tokio::spawn(async move { approver.approve(&request).await });

        match ui_rx.recv().await.expect("approval request") {
            UiMsg::Approval { request, reply } => {
                assert_eq!(request.tool, "bash");
                assert_eq!(request.detail, "rm -rf build");
                let _ = reply.send(Approval::AllowSession);
            }
            _ => panic!("expected an approval request"),
        }
        assert_eq!(asking.await.unwrap().unwrap(), Approval::AllowSession);
    }

    #[tokio::test]
    async fn shell_shortcut_runs_when_full_access() {
        let workspace = scratch();
        let mut state = test_state();
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        run_shell(&mut state, &ui_tx, &workspace, "printf ok");
        match ui_rx.recv().await.expect("shell result") {
            UiMsg::ShellDone(Ok(output)) => assert!(output.content.contains("ok")),
            UiMsg::ShellDone(Err(error)) => panic!("unexpected error: {error}"),
            _ => panic!("expected a shell result with no approval step"),
        }
    }

    #[test]
    fn busy_enter_keeps_exactly_one_queued_turn() {
        let mut state = test_state();
        state.busy = true;
        state.input = "first".into();
        queue_input(&mut state);
        assert_eq!(state.queued.as_deref(), Some("first"));
        assert!(state.input.is_empty());

        state.input = "second".into();
        queue_input(&mut state);
        assert_eq!(state.queued.as_deref(), Some("second"));
        assert!(
            state
                .pending
                .iter()
                .any(|line| line.contains("queued (replaced): second"))
        );
    }

    #[test]
    fn empty_input_does_not_queue() {
        let mut state = test_state();
        state.busy = true;
        state.input = "   ".into();
        queue_input(&mut state);
        assert!(state.queued.is_none());
    }

    #[test]
    fn cancelling_a_question_reports_cancelled_not_an_empty_answer() {
        let mut state = test_state();
        let (reply, mut answer) = oneshot::channel();
        state.question = Some(PendingQuestion { reply });
        cancel_question(&mut state);
        assert_eq!(answer.try_recv().unwrap(), AskOutcome::Cancelled);
        assert!(state.question.is_none());
    }

    #[test]
    fn answering_a_question_reports_the_text() {
        let mut state = test_state();
        let (reply, mut answer) = oneshot::channel();
        state.question = Some(PendingQuestion { reply });
        state.input = "yes, do it".into();
        answer_question(&mut state);
        assert_eq!(
            answer.try_recv().unwrap(),
            AskOutcome::Answered("yes, do it".into())
        );
    }

    #[test]
    fn approval_keys_map_to_decisions() {
        for (key, expected) in [
            ('y', Approval::AllowOnce),
            ('a', Approval::AllowSession),
            ('n', Approval::Deny),
        ] {
            let mut state = test_state();
            let (reply, mut decision) = oneshot::channel();
            state.approval = Some(PendingApproval {
                request: ApprovalRequest {
                    tool: "bash",
                    detail: "ls".into(),
                },
                reply,
            });
            let (command_tx, _command_rx) = mpsc::unbounded_channel();
            let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE),
                &command_tx,
                &ui_tx,
                Path::new("."),
            );
            assert_eq!(decision.try_recv().unwrap(), expected);
            assert!(state.approval.is_none());
        }
    }

    #[test]
    fn approval_keys_do_not_leak_into_the_composer() {
        let mut state = test_state();
        let (reply, _decision) = oneshot::channel();
        state.approval = Some(PendingApproval {
            request: ApprovalRequest {
                tool: "bash",
                detail: "ls".into(),
            },
            reply,
        });
        let (command_tx, _command_rx) = mpsc::unbounded_channel();
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
            &command_tx,
            &ui_tx,
            Path::new("."),
        );
        assert!(state.input.is_empty());
    }

    #[test]
    fn truncate_keeps_short_text() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 3), "hel…");
    }

    #[test]
    fn slash_quit_sets_flag() {
        let mut state = UiState::new("m".into(), "cwd".into(), Arc::new(SkillRegistry::default()));
        handle_slash(&mut state, "quit");
        assert!(state.quit);
    }

    #[test]
    fn complete_lines_are_split_from_partial() {
        let mut state = UiState::new("m".into(), "cwd".into(), Arc::new(SkillRegistry::default()));
        state.stream_buf.push_str("one\ntwo");
        drain_complete_lines(&mut state);
        assert_eq!(state.pending, vec!["one"]);
        assert_eq!(state.stream_buf, "two");
        flush_stream(&mut state);
        assert_eq!(state.pending, vec!["one", "two"]);
    }
}
