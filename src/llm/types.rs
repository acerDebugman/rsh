use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A tool invocation requested by the model. Arguments stay as the raw JSON
/// string from the wire so a malformed call becomes a tool error instead of a
/// stream failure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default)]
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    #[must_use]
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: text.into(),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: text.into(),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    #[must_use]
    pub fn assistant(
        text: impl Into<String>,
        reasoning: Option<String>,
        tool_calls: Vec<ToolCall>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content: text.into(),
            reasoning,
            tool_calls,
            tool_call_id: None,
        }
    }

    #[must_use]
    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }

    /// Serialize to the OpenAI chat-completions message shape.
    #[must_use]
    pub fn to_api(&self, send_reasoning: bool) -> Value {
        let mut value = json!({"role": self.role, "content": self.content});
        if !self.tool_calls.is_empty() {
            value["tool_calls"] = Value::Array(
                self.tool_calls
                    .iter()
                    .map(|call| {
                        json!({
                            "id": call.id,
                            "type": "function",
                            "function": {"name": call.name, "arguments": call.arguments}
                        })
                    })
                    .collect(),
            );
        }
        if let Some(call_id) = &self.tool_call_id {
            value["tool_call_id"] = json!(call_id);
        }
        if send_reasoning {
            if let Some(reasoning) = &self.reasoning {
                value["reasoning_content"] = json!(reasoning);
            }
        }
        value
    }
}

#[derive(Clone, Debug)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<Value>,
    pub max_tokens: u32,
    pub temperature: f32,
    pub send_reasoning: bool,
}

impl ChatRequest {
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "model": self.model,
            "messages": self.messages.iter().map(|m| m.to_api(self.send_reasoning)).collect::<Vec<_>>(),
            "tools": self.tools,
            "tool_choice": "auto",
            "stream": true,
            "stream_options": {"include_usage": true},
            "max_tokens": self.max_tokens,
            "temperature": self.temperature,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_tool_call_serializes_as_openai_function() {
        let message = Message::assistant(
            "",
            None,
            vec![ToolCall {
                id: "call_1".into(),
                name: "bash".into(),
                arguments: "{\"command\":\"ls\"}".into(),
            }],
        );
        let value = message.to_api(false);
        assert_eq!(value["role"], "assistant");
        assert_eq!(value["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(
            value["tool_calls"][0]["function"]["arguments"],
            "{\"command\":\"ls\"}"
        );
        assert!(value.get("reasoning_content").is_none());
    }

    #[test]
    fn tool_message_carries_call_id() {
        let value = Message::tool("call_1", "ok").to_api(false);
        assert_eq!(value["role"], "tool");
        assert_eq!(value["tool_call_id"], "call_1");
    }

    #[test]
    fn reasoning_is_opt_in() {
        let message = Message::assistant("hi", Some("why".into()), Vec::new());
        assert!(message.to_api(false).get("reasoning_content").is_none());
        assert_eq!(message.to_api(true)["reasoning_content"], "why");
    }

    #[test]
    fn request_sets_stream_and_includes_usage() {
        let request = ChatRequest {
            model: "deepseek-flash".into(),
            messages: vec![Message::user("hi")],
            tools: Vec::new(),
            max_tokens: 16,
            temperature: 0.2,
            send_reasoning: false,
        };
        let value = request.to_json();
        assert_eq!(value["stream"], true);
        assert_eq!(value["stream_options"]["include_usage"], true);
        assert_eq!(value["tool_choice"], "auto");
    }
}
