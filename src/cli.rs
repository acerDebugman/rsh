use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "rsh",
    version,
    about = "A terminal coding agent with a hand-written tool loop"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run one task without the interactive TUI
    Exec(ExecArgs),
}

#[derive(Debug, Args)]
pub struct ExecArgs {
    /// Explicitly acknowledge full access (the default) and skip approvals
    #[arg(long)]
    pub yolo: bool,

    /// Override the configured model
    #[arg(long)]
    pub model: Option<String>,

    /// Task prompt
    #[arg(required = true, num_args = 1..)]
    pub task: Vec<String>,
}

impl ExecArgs {
    #[must_use]
    pub fn prompt(&self) -> String {
        self.task.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_parses_prompt_words() {
        let cli = Cli::try_parse_from(["rsh", "exec", "--yolo", "create", "a.txt"]).unwrap();
        let Some(Command::Exec(args)) = cli.command else {
            panic!("expected exec");
        };
        assert!(args.yolo);
        assert_eq!(args.prompt(), "create a.txt");
    }

    #[test]
    fn empty_invocation_has_no_command() {
        let cli = Cli::try_parse_from(["rsh"]).unwrap();
        assert!(cli.command.is_none());
    }
}
