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

use crate::agent::{Agent, AgentEvent, TurnOutcome};
use crate::config::Config;
use crate::error::ToolError;
use crate::llm::{LlmClient, OpenAiClient};
use crate::session::{SessionStore, default_root, replay};
use crate::skills::SkillRegistry;
use crate::subagent::Manager;
use crate::tools::{BashTool, Tool, ToolCtx, ToolOutput, ToolRegistry, UserIo};

pub(crate) struct UiState {
    pub(crate) input: String,
    pub(crate) busy: bool,
    pub(crate) pending: Vec<String>,
    pub(crate) lines: Vec<String>,
    pub(crate) stream_buf: String,
    pub(crate) quit: bool,
    pub(crate) model: String,
    pub(crate) cwd: String,
    pub(crate) question: Option<PendingQuestion>,
    skills: Arc<SkillRegistry>,
    current_cancel: Option<CancellationToken>,
}

pub(crate) struct PendingQuestion {
    reply: oneshot::Sender<String>,
}

impl UiState {
    fn new(model: String, cwd: String, skills: Arc<SkillRegistry>) -> Self {
        Self {
            input: String::new(),
            busy: false,
            pending: Vec::new(),
            lines: Vec::new(),
            stream_buf: String::new(),
            quit: false,
            model,
            cwd,
            question: None,
            skills,
            current_cancel: None,
        }
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
        reply: oneshot::Sender<Result<String>>,
    },
}

enum UiMsg {
    Agent(AgentEvent),
    TurnDone(Result<TurnOutcome, String>),
    ShellDone(Result<ToolOutput, String>),
    CompactDone(Result<String, String>),
    Question {
        question: String,
        options: Vec<String>,
        reply: oneshot::Sender<String>,
    },
}

struct TuiUserIo {
    ui_tx: mpsc::UnboundedSender<UiMsg>,
}

#[async_trait]
impl UserIo for TuiUserIo {
    async fn ask(&self, question: &str, options: &[String]) -> Result<String, ToolError> {
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
            .map_err(|_| ToolError::Other("question was cancelled".into()))
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

    let mut agent = Agent::new(
        client,
        Arc::clone(&tools),
        config.clone(),
        workspace.clone(),
    )
    .with_skills(Arc::clone(&skills))
    .with_user_io(user_io)
    .with_subagents(manager);
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

    terminal::install_panic_hook();
    let (mut terminal, mut mode) = terminal::init()?;
    let _guard = terminal::Guard;
    let mut state = UiState::new(
        config.model.clone(),
        workspace.display().to_string(),
        Arc::clone(&skills),
    );
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
            Some(message) = ui_rx.recv() => handle_ui_message(&mut state, message),
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
            } else if !state.busy {
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
        let _ = question.reply.send(answer);
    }
}

fn cancel_question(state: &mut UiState) {
    if let Some(question) = state.question.take() {
        let _ = question.reply.send(String::new());
    }
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
    tokio::spawn(async move {
        let context = ToolCtx::new(workspace);
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

fn handle_ui_message(state: &mut UiState, message: UiMsg) {
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
        }
        UiMsg::CompactDone(result) => {
            state.busy = false;
            match result {
                Ok(summary) => state.push_line(format!("✓ compacted: {summary}")),
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
