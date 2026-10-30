//! System prompt assembly.
//!
//! Deliberately one place, not overridable from a user file: if the prompt can
//! be replaced, "is this a prompt problem or a code problem?" becomes
//! unanswerable. Sections are ordered identity → environment → tools →
//! workflow → skills → subagents → asking → safety → output.

use std::path::Path;

use crate::config::{Config, PermissionMode};
use crate::skills::SkillRegistry;

/// `AGENTS.md` is appended, never merged into the middle of the prompt.
pub const AGENTS_MD_LIMIT: usize = 64 * 1024;

const IDENTITY: &str = "\
You are rsh, a terminal coding agent working inside the user's workspace.";

/// One line per tool: the schema already says how to call it, so repeating
/// parameter names here only creates a second source of truth.
const TOOLS: &[(&str, &str)] = &[
    ("bash", "run a shell command in the workspace"),
    (
        "read_file",
        "read a text file with line numbers (use offset/limit instead of reading everything)",
    ),
    ("write_file", "create or overwrite a file"),
    (
        "edit_file",
        "replace an exact string; read the file first so the match is unique",
    ),
    ("glob", "find files by glob pattern"),
    ("grep", "search file contents with a regex"),
    ("skill", "load a skill's instructions on demand"),
    (
        "subagent",
        "delegate one isolated task and get a summary back",
    ),
    (
        "ask_user",
        "ask the user a question when a decision cannot be inferred",
    ),
];

const WORKFLOW: &str = "\
Workflow:
- Read before editing. Never rewrite a file you have not read.
- Check the [exit code: N] marker on every bash result and investigate failures before moving on.
- Keep replies short and concrete. Do not invent file contents.";

const SAFETY: &str = "\
Safety:
- Prefer reversible steps; state what a destructive command will do before running it.
- Never print secrets or credentials, and never exfiltrate workspace contents.";

pub struct PromptContext<'a> {
    pub workspace: &'a Path,
    pub config: &'a Config,
    pub skills: Option<&'a SkillRegistry>,
    /// Injected so prompt tests stay deterministic.
    pub today: Option<String>,
    pub is_git_repo: Option<bool>,
}

impl<'a> PromptContext<'a> {
    #[must_use]
    pub fn new(workspace: &'a Path, config: &'a Config) -> Self {
        Self {
            workspace,
            config,
            skills: None,
            today: None,
            is_git_repo: None,
        }
    }

    #[must_use]
    pub fn with_skills(mut self, skills: Option<&'a SkillRegistry>) -> Self {
        self.skills = skills;
        self
    }
}

#[must_use]
pub fn system_prompt(ctx: &PromptContext<'_>) -> String {
    let mut prompt = String::from(IDENTITY);
    prompt.push_str("\n\n");

    let today = ctx
        .today
        .clone()
        .unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d").to_string());
    let git = ctx
        .is_git_repo
        .unwrap_or_else(|| ctx.workspace.join(".git").exists());
    prompt.push_str(&format!(
        "Environment:\n- cwd: {}\n- os: {}\n- date: {today}\n- model: {}\n- permission mode: {}\n- git repository: {}\n\n",
        ctx.workspace.display(),
        std::env::consts::OS,
        ctx.config.model,
        ctx.config.permissions.mode.as_str(),
        if git { "yes" } else { "no" },
    ));
    if ctx.config.permissions.mode == PermissionMode::Ask {
        prompt.push_str(
            "Permission mode is `ask`: every bash command and every path outside the \
             workspace waits for user approval. If an action is denied, adapt instead of \
             retrying the same call.\n\n",
        );
    }

    prompt.push_str("Tools:\n");
    for (name, purpose) in TOOLS {
        prompt.push_str(&format!("- {name}: {purpose}\n"));
    }
    prompt.push('\n');
    prompt.push_str(WORKFLOW);
    prompt.push_str("\n\n");

    if let Some(skills) = ctx.skills {
        let visible: Vec<_> = skills
            .list()
            .into_iter()
            .filter(|skill| !skill.disable_model_invocation)
            .collect();
        if !visible.is_empty() {
            prompt.push_str("Skills available through the skill tool:\n");
            for skill in visible {
                if skill.description.is_empty() {
                    prompt.push_str(&format!("- {}\n", skill.name));
                } else {
                    prompt.push_str(&format!("- {}: {}\n", skill.name, skill.description));
                }
            }
            prompt.push('\n');
        }
    }

    prompt.push_str(
        "Subagents:\n- The subagent tool starts with one task and returns one summary. It \
         cannot ask the user questions and cannot start another subagent.\n\n",
    );
    prompt.push_str(
        "Asking the user:\n- Use ask_user only for decisions you cannot infer from the \
         workspace. If the answer is cancelled, stop and report rather than guessing.\n\n",
    );
    prompt.push_str(SAFETY);
    prompt.push('\n');

    if let Some(agents_md) = agents_md(ctx.workspace) {
        prompt.push_str(&format!(
            "\nWorkspace instructions from AGENTS.md:\n{agents_md}\n"
        ));
    }
    prompt
}

/// Append `AGENTS.md` verbatim, truncated on a char boundary with a visible
/// marker so a giant file cannot silently eat the context window.
#[must_use]
pub fn agents_md(workspace: &Path) -> Option<String> {
    let path = workspace.join("AGENTS.md");
    let text = std::fs::read_to_string(&path).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(truncate_chars(trimmed, AGENTS_MD_LIMIT))
}

fn truncate_chars(text: &str, limit: usize) -> String {
    let total = text.chars().count();
    if total <= limit {
        return text.to_string();
    }
    let mut shortened: String = text.chars().take(limit).collect();
    shortened.push_str(&format!("\n[truncated {} chars]", total - limit));
    shortened
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> std::path::PathBuf {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::Builder::new()
            .prefix("prompt-")
            .tempdir_in(&base)
            .unwrap()
            .keep()
    }

    fn prompt(workspace: &Path, config: &Config) -> String {
        let mut ctx = PromptContext::new(workspace, config);
        ctx.today = Some("2026-10-31".into());
        system_prompt(&ctx)
    }

    #[test]
    fn includes_runtime_environment() {
        let config = Config::default();
        let workspace = workspace();
        let text = prompt(&workspace, &config);
        assert!(text.contains(&workspace.display().to_string()));
        assert!(text.contains("date: 2026-10-31"));
        assert!(text.contains("model: deepseek-flash"));
        assert!(text.contains("permission mode: danger-full-access"));
        assert!(text.contains("git repository: no"));
    }

    #[test]
    fn lists_all_nine_tools_once() {
        let config = Config::default();
        let text = prompt(&workspace(), &config);
        for (name, _) in TOOLS {
            assert_eq!(
                text.matches(&format!("- {name}:")).count(),
                1,
                "tool {name} listed once"
            );
        }
        assert_eq!(TOOLS.len(), 9);
    }

    #[test]
    fn ask_mode_explains_approval() {
        let mut config = Config::default();
        config.permissions.mode = PermissionMode::Ask;
        let text = prompt(&workspace(), &config);
        assert!(text.contains("permission mode: ask"));
        assert!(text.contains("waits for user approval"));
    }

    #[test]
    fn agents_md_is_appended_and_truncated() {
        let workspace = workspace();
        std::fs::write(workspace.join("AGENTS.md"), "Run cargo fmt.").unwrap();
        let text = prompt(&workspace, &Config::default());
        assert!(text.contains("Workspace instructions from AGENTS.md:\nRun cargo fmt."));

        std::fs::write(
            workspace.join("AGENTS.md"),
            "x".repeat(AGENTS_MD_LIMIT + 10),
        )
        .unwrap();
        let text = prompt(&workspace, &Config::default());
        assert!(text.contains("[truncated"));
    }

    #[test]
    fn missing_agents_md_is_absent() {
        assert!(agents_md(&workspace()).is_none());
    }
}
