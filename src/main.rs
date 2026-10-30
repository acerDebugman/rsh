use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use rsh::agent::{Agent, AgentEvent};
use rsh::cli::{Cli, Command, ExecArgs};
use rsh::config::{Config, PermissionMode};
use rsh::llm::OpenAiClient;
use rsh::session::SessionStore;
use rsh::tools::ToolRegistry;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Exec(args)) => run_exec(args).await,
        None => {
            eprintln!("rsh: the interactive TUI lands in M2; use `rsh exec \"...\"` for now");
            Ok(())
        }
    }
}

async fn run_exec(args: ExecArgs) -> Result<()> {
    let mut config = Config::load(None)?;
    if let Some(model) = &args.model {
        config.model = model.clone();
    }
    if args.yolo {
        config.permissions.mode = PermissionMode::DangerFullAccess;
    }
    config.validate()?;

    let workspace = std::env::current_dir().context("resolve current directory")?;
    let client = OpenAiClient::from_config(&config)?;
    let tools = Arc::new(ToolRegistry::with_builtin_tools());
    let mut agent = Agent::new(Arc::new(client), tools, config.clone(), workspace.clone());

    match SessionStore::create(&session_root(), &workspace, &config.model) {
        Ok(session) => {
            eprintln!("session: {}", session.session_id());
            agent = agent.with_session(session);
        }
        Err(error) => eprintln!("warning: session log disabled: {error}"),
    }

    let (tx, mut rx) = mpsc::channel(256);
    let printer = tokio::spawn(async move {
        let mut stdout = std::io::stdout();
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::Text(text) => {
                    let _ = stdout.write_all(text.as_bytes());
                    let _ = stdout.flush();
                }
                AgentEvent::ToolStart { name, arguments } => {
                    eprintln!("\n[tool] {name} {arguments}");
                }
                AgentEvent::ToolEnd { name, ok, .. } => {
                    eprintln!("[{}] {name}", if ok { "ok" } else { "error" });
                }
                AgentEvent::Reasoning(_) => {}
            }
        }
        let _ = writeln!(stdout);
    });

    let outcome = agent
        .run_turn(&args.prompt(), tx, CancellationToken::new())
        .await;
    let _ = printer.await;
    outcome?;
    Ok(())
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
