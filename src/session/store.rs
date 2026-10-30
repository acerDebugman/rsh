use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::llm::types::{Message, ToolCall};

pub const EVENT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    pub v: u32,
    pub ts: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    SessionStart {
        session_id: String,
        cwd: PathBuf,
        model: String,
    },
    UserMessage {
        text: String,
    },
    AssistantMessage {
        text: String,
        #[serde(default)]
        reasoning: Option<String>,
        #[serde(default)]
        tool_calls: Vec<ToolCall>,
    },
    ToolResult {
        call_id: String,
        name: String,
        ok: bool,
        content: String,
    },
    Compaction {
        summary: String,
        replaces_until: usize,
    },
    TurnEnd {
        reason: String,
    },
    #[serde(other)]
    Unknown,
}

impl SessionEvent {
    #[must_use]
    pub fn new(kind: EventKind) -> Self {
        Self {
            v: EVENT_VERSION,
            ts: Utc::now(),
            kind,
        }
    }
}

pub struct SessionStore {
    path: PathBuf,
    session_id: String,
    file: File,
}

impl SessionStore {
    /// Create `root/<cwd-slug>/<session-id>/events.jsonl` and write the
    /// `session_start` event.
    pub fn create(root: &Path, cwd: &Path, model: &str) -> Result<Self> {
        let session_id = format!(
            "session-{}-{}",
            Utc::now().format("%Y%m%dT%H%M%S%.3f"),
            std::process::id()
        );
        let dir = root.join(slug(cwd)).join(&session_id);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create session dir {}", dir.display()))?;
        let path = dir.join("events.jsonl");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open session log {}", path.display()))?;
        let mut store = Self {
            path,
            session_id,
            file,
        };
        store.append(EventKind::SessionStart {
            session_id: store.session_id.clone(),
            cwd: cwd.to_path_buf(),
            model: model.to_string(),
        })?;
        Ok(store)
    }

    pub fn append(&mut self, kind: EventKind) -> Result<()> {
        let event = SessionEvent::new(kind);
        serde_json::to_writer(&mut self.file, &event).context("serialize session event")?;
        self.file.write_all(b"\n").context("write session event")?;
        self.file.flush().context("flush session log")?;
        Ok(())
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn load(path: &Path) -> Result<Vec<SessionEvent>> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let mut events = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line.context("read session line")?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionEvent>(&line) {
                Ok(event) => {
                    if !matches!(event.kind, EventKind::Unknown) {
                        events.push(event);
                    }
                }
                Err(error) => {
                    eprintln!("warning: skipping malformed session event: {error}");
                }
            }
        }
        Ok(events)
    }
}

/// Rebuild the model context from a session log. Compaction events replace
/// the prefix they cover with a summary system message.
#[must_use]
pub fn replay(events: &[SessionEvent]) -> Vec<Message> {
    let mut messages = Vec::new();
    for event in events {
        match &event.kind {
            EventKind::UserMessage { text } => messages.push(Message::user(text.clone())),
            EventKind::AssistantMessage {
                text,
                reasoning,
                tool_calls,
            } => messages.push(Message::assistant(
                text.clone(),
                reasoning.clone(),
                tool_calls.clone(),
            )),
            EventKind::ToolResult {
                call_id, content, ..
            } => messages.push(Message::tool(call_id.clone(), content.clone())),
            EventKind::Compaction {
                summary,
                replaces_until,
            } => {
                let boundary = (*replaces_until).min(messages.len());
                let tail = messages.split_off(boundary);
                messages.clear();
                messages.push(Message::system(format!("[compaction summary]\n{summary}")));
                messages.extend(tail);
            }
            EventKind::SessionStart { .. } | EventKind::TurnEnd { .. } | EventKind::Unknown => {}
        }
    }
    messages
}

#[must_use]
pub fn slug(cwd: &Path) -> String {
    let text = cwd.to_string_lossy();
    let trimmed = text.trim_matches('/');
    if trimmed.is_empty() {
        return "--root--".to_string();
    }
    format!("--{}--", trimmed.replace('/', "-"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::Builder::new()
            .prefix("session-")
            .tempdir_in(&base)
            .unwrap()
            .keep()
    }

    #[test]
    fn append_and_load_round_trip() {
        let root = root();
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut store = SessionStore::create(&root, &cwd, "deepseek-flash").unwrap();
        store
            .append(EventKind::UserMessage {
                text: "hello".into(),
            })
            .unwrap();
        let path = store.path().to_path_buf();
        drop(store);

        let events = SessionStore::load(&path).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0].kind, EventKind::SessionStart { .. }));
        assert!(matches!(&events[1].kind, EventKind::UserMessage { text } if text == "hello"));
    }

    #[test]
    fn replay_rebuilds_text_and_tool_messages() {
        let events = vec![
            SessionEvent::new(EventKind::UserMessage {
                text: "run ls".into(),
            }),
            SessionEvent::new(EventKind::AssistantMessage {
                text: String::new(),
                reasoning: None,
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "bash".into(),
                    arguments: "{\"command\":\"ls\"}".into(),
                }],
            }),
            SessionEvent::new(EventKind::ToolResult {
                call_id: "call_1".into(),
                name: "bash".into(),
                ok: true,
                content: "a.txt\n[exit code: 0]".into(),
            }),
            SessionEvent::new(EventKind::AssistantMessage {
                text: "done".into(),
                reasoning: None,
                tool_calls: Vec::new(),
            }),
        ];
        let messages = replay(&events);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].content, "run ls");
        assert_eq!(messages[1].tool_calls.len(), 1);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(messages[3].content, "done");
    }

    #[test]
    fn compaction_replaces_the_prefix() {
        let events = vec![
            SessionEvent::new(EventKind::UserMessage { text: "one".into() }),
            SessionEvent::new(EventKind::AssistantMessage {
                text: "two".into(),
                reasoning: None,
                tool_calls: Vec::new(),
            }),
            SessionEvent::new(EventKind::UserMessage {
                text: "three".into(),
            }),
            SessionEvent::new(EventKind::Compaction {
                summary: "we discussed one and two".into(),
                replaces_until: 2,
            }),
            SessionEvent::new(EventKind::UserMessage {
                text: "four".into(),
            }),
        ];
        let messages = replay(&events);
        assert_eq!(messages.len(), 3);
        assert!(messages[0].content.contains("compaction summary"));
        assert_eq!(messages[1].content, "three");
        assert_eq!(messages[2].content, "four");
    }

    #[test]
    fn unknown_events_are_skipped() {
        let root = root();
        let path = root.join("events.jsonl");
        std::fs::write(
            &path,
            "{\"v\":1,\"ts\":\"2026-10-31T00:00:00Z\",\"type\":\"mystery\",\"x\":1}\n{\"v\":1,\"ts\":\"2026-10-31T00:00:00Z\",\"type\":\"user_message\",\"text\":\"hi\"}\n",
        )
        .unwrap();
        let events = SessionStore::load(&path).unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0].kind, EventKind::UserMessage { text } if text == "hi"));
    }

    #[test]
    fn slug_is_readable_and_stable() {
        assert_eq!(slug(Path::new("/home/algo/proj")), "--home-algo-proj--");
        assert_eq!(slug(Path::new("/")), "--root--");
    }
}
