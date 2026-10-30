mod render;
mod terminal;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentEvent, TurnOutcome};
use crate::config::Config;
use crate::llm::OpenAiClient;
use crate::session::SessionStore;
use crate::tools::{BashTool, Tool, ToolCtx, ToolOutput, ToolRegistry};

pub(crate) struct UiState {
    pub(crate) input: String,
    pub(crate) busy: bool,
    pub(crate) pending: Vec<String>,
    pub(crate) lines: Vec<String>,
    pub(crate) stream_buf: String,
    pub(crate) quit: bool,
    pub(crate) model: String,
    pub(crate) cwd: String,
    current_cancel: Option<CancellationToken>,
}

impl UiState {
    fn new(model: String, cwd: String) -> Self {
        Self {
            input: String::new(),
            busy: false,
            pending: Vec::new(),
            lines: Vec::new(),
            stream_buf: String::new(),
            quit: false,
            model,
            cwd,
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
}

enum UiMsg {
    Agent(AgentEvent),
    TurnDone(Result<TurnOutcome, String>),
    ShellDone(Result<ToolOutput, String>),
}

pub async fn run(config: Config, workspace: PathBuf) -> Result<()> {
    let client = OpenAiClient::from_config(&config)?;
    let tools = Arc::new(ToolRegistry::with_builtin_tools());
    let mut agent = Agent::new(
        Arc::new(client),
        Arc::clone(&tools),
        config.clone(),
        workspace.clone(),
    );
    match SessionStore::create(&session_root(), &workspace, &config.model) {
        Ok(session) => agent = agent.with_session(session),
        Err(error) => eprintln!("warning: session log disabled: {error}"),
    }

    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
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
    let mut state = UiState::new(config.model.clone(), workspace.display().to_string());

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
    while let Some(Command::Turn {
        input,
        cancel,
        reply,
    }) = commands.recv().await
    {
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
            if !state.busy {
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
            if state.busy {
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
        KeyCode::Enter if !state.busy => {
            submit(state, command_tx, ui_tx, workspace);
        }
        KeyCode::Esc => {
            if state.busy {
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
        handle_slash(state, command);
        return;
    }
    if let Some(command) = trimmed.strip_prefix('!') {
        run_shell(state, ui_tx, workspace, command.trim());
        return;
    }

    state.push_line(format!("› {input}"));
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
        UiMsg::TurnDone(result) => {
            flush_stream(state);
            state.busy = false;
            state.current_cancel = None;
            if let Err(error) = result {
                state.push_line(format!("! {error}"));
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

fn session_root() -> PathBuf {
    std::env::var_os("RSH_HOME").map_or_else(
        || {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".rsh")
        },
        PathBuf::from,
    )
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
        let mut state = UiState::new("m".into(), "cwd".into());
        handle_slash(&mut state, "quit");
        assert!(state.quit);
    }

    #[test]
    fn complete_lines_are_split_from_partial() {
        let mut state = UiState::new("m".into(), "cwd".into());
        state.stream_buf.push_str("one\ntwo");
        drain_complete_lines(&mut state);
        assert_eq!(state.pending, vec!["one"]);
        assert_eq!(state.stream_buf, "two");
        flush_stream(&mut state);
        assert_eq!(state.pending, vec!["one", "two"]);
    }
}
