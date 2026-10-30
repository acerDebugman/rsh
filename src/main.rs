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
use rsh::llm::{LlmClient, OpenAiClient};
use rsh::session::{SessionStore, default_root, find_session, latest_session, replay};
use rsh::skills::SkillRegistry;
use rsh::subagent::Manager;
use rsh::tools::ToolRegistry;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let workspace = std::env::current_dir().context("resolve current directory")?;
    let root = default_root();
    let resume = if let Some(id) = &cli.resume {
        Some(find_session(&root, id).with_context(|| format!("session not found: {id}"))?)
    } else if cli.continue_session {
        latest_session(&root, &workspace)
    } else {
        None
    };

    match cli.command {
        Some(Command::Exec(args)) => run_exec(args, workspace, resume, cli.read_only).await,
        None => {
            let mut config = Config::load(None)?;
            apply_read_only(&mut config, cli.read_only);
            rsh::tui::run(config, workspace, resume).await
        }
    }
}

fn apply_read_only(config: &mut Config, read_only: bool) {
    // The flag wins over both `--yolo` and the config file: the escape hatch
    // must never be the thing that gets silently overridden.
    if read_only {
        config.permissions.mode = PermissionMode::ReadOnly;
    }
}

async fn run_exec(
    args: ExecArgs,
    workspace: PathBuf,
    resume: Option<PathBuf>,
    read_only: bool,
) -> Result<()> {
    let mut config = Config::load(None)?;
    if let Some(model) = &args.model {
        config.model = model.clone();
    }
    if args.yolo {
        config.permissions.mode = PermissionMode::DangerFullAccess;
    }
    apply_read_only(&mut config, read_only);
    config.validate()?;
    // `exec` is headless: an approval prompt has nowhere to land, so refusing
    // up front keeps `--yolo` in --help from being a lie.
    if config.permissions.mode == PermissionMode::Ask {
        anyhow::bail!(
            "`rsh exec` cannot run in ask mode (no approval surface); pass --yolo to run with full access"
        );
    }

    let client: Arc<dyn LlmClient> = Arc::new(OpenAiClient::from_config(&config)?);
    let skills = Arc::new(SkillRegistry::discover(&workspace));
    let manager = Arc::new(Manager::new(
        Arc::clone(&client),
        config.clone(),
        Arc::clone(&skills),
        workspace.clone(),
    ));
    let tools = Arc::new(ToolRegistry::with_builtin_tools());
    let mut agent = Agent::new(client, tools, config.clone(), workspace.clone())
        .with_skills(skills)
        .with_subagents(manager);

    if let Some(path) = resume {
        let events = SessionStore::load(&path)?;
        agent = agent.with_messages(replay(&events));
        let store = SessionStore::open(&path)?;
        eprintln!("resumed session: {}", store.session_id());
        agent = agent.with_session(store);
    } else {
        match SessionStore::create(&default_root(), &workspace, &config.model) {
            Ok(session) => {
                eprintln!("session: {}", session.session_id());
                agent = agent.with_session(session);
            }
            Err(error) => eprintln!("warning: session log disabled: {error}"),
        }
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
