//! Rendering a conversation slice for the compaction prompt.

use crate::llm::{Message, Role};

pub fn render_transcript(messages: &[Message]) -> String {
    let mut output = String::new();
    for message in messages {
        let role = match message.role {
            Role::System => "System",
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::Tool => "Tool",
        };
        output.push_str(role);
        output.push_str(": ");
        if !message.content.is_empty() {
            output.push_str(&truncate_chars(&message.content, 2_000));
        }
        if !message.tool_calls.is_empty() {
            let names: Vec<&str> = message
                .tool_calls
                .iter()
                .map(|call| call.name.as_str())
                .collect();
            output.push_str(&format!(" [calls: {}]", names.join(", ")));
        }
        output.push('\n');
    }
    truncate_chars(&output, 60_000)
}

#[must_use]
pub fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut shortened: String = text.chars().take(limit).collect();
    shortened.push('…');
    shortened
}
