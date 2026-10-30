use std::collections::BTreeMap;

use serde::Deserialize;

use crate::llm::types::{ToolCall, Usage};

/// One parsed `data:` frame from the SSE stream.
#[derive(Debug)]
pub enum SseLine {
    Ignore,
    Done,
    Chunk(ChatChunk),
}

#[derive(Debug, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
pub struct ChunkChoice {
    #[serde(default)]
    pub delta: Delta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Delta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallDelta>,
}

#[derive(Debug, Deserialize)]
pub struct ToolCallDelta {
    pub index: u32,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionDelta>,
}

#[derive(Debug, Deserialize)]
pub struct FunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

/// Parse one SSE line. Non-`data:` lines (comments, blank lines, event names)
/// are ignored; malformed JSON is an error so the caller can decide whether to
/// skip it.
pub fn parse_sse_line(line: &str) -> Result<SseLine, serde_json::Error> {
    let line = line.trim_end_matches(['\r', '\n']);
    let Some(rest) = line.strip_prefix("data:") else {
        return Ok(SseLine::Ignore);
    };
    let data = rest.trim();
    if data.is_empty() {
        return Ok(SseLine::Ignore);
    }
    if data == "[DONE]" {
        return Ok(SseLine::Done);
    }
    serde_json::from_str(data).map(SseLine::Chunk)
}

/// Incremental output of one streamed assistant turn.
#[derive(Debug, Clone, PartialEq)]
pub enum LlmEvent {
    Text(String),
    Reasoning(String),
    ToolCall {
        index: u32,
        id: Option<String>,
        name: Option<String>,
        arguments: Option<String>,
    },
    Usage(Usage),
}

#[derive(Debug, Default)]
pub struct StreamOutcome {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub finish_reason: Option<String>,
}

#[derive(Default)]
struct ToolCallBuf {
    id: String,
    name: String,
    arguments: String,
}

/// Reassembles streamed deltas into a final assistant message.
#[derive(Default)]
pub struct StreamAccumulator {
    text: String,
    reasoning: String,
    calls: BTreeMap<u32, ToolCallBuf>,
    usage: Option<Usage>,
    finish_reason: Option<String>,
}

impl StreamAccumulator {
    pub fn push_chunk(&mut self, chunk: ChatChunk) -> Vec<LlmEvent> {
        let mut events = Vec::new();
        if let Some(usage) = chunk.usage {
            self.usage = Some(usage.clone());
            events.push(LlmEvent::Usage(usage));
        }
        for choice in chunk.choices {
            if let Some(text) = choice.delta.content.filter(|text| !text.is_empty()) {
                self.text.push_str(&text);
                events.push(LlmEvent::Text(text));
            }
            if let Some(text) = choice
                .delta
                .reasoning_content
                .filter(|text| !text.is_empty())
            {
                self.reasoning.push_str(&text);
                events.push(LlmEvent::Reasoning(text));
            }
            for delta in choice.delta.tool_calls {
                let buffer = self.calls.entry(delta.index).or_default();
                let mut id = None;
                if let Some(value) = delta.id.filter(|value| !value.is_empty()) {
                    buffer.id = value.clone();
                    id = Some(value);
                }
                let mut name = None;
                let mut arguments = None;
                if let Some(function) = delta.function {
                    if let Some(value) = function.name.filter(|value| !value.is_empty()) {
                        buffer.name = value.clone();
                        name = Some(value);
                    }
                    if let Some(value) = function.arguments.filter(|value| !value.is_empty()) {
                        buffer.arguments.push_str(&value);
                        arguments = Some(value);
                    }
                }
                if id.is_some() || name.is_some() || arguments.is_some() {
                    events.push(LlmEvent::ToolCall {
                        index: delta.index,
                        id,
                        name,
                        arguments,
                    });
                }
            }
            if let Some(reason) = choice.finish_reason {
                self.finish_reason = Some(reason);
            }
        }
        events
    }

    #[must_use]
    pub fn finish(self) -> StreamOutcome {
        let tool_calls = self
            .calls
            .into_iter()
            .map(|(index, buffer)| ToolCall {
                id: if buffer.id.is_empty() {
                    format!("call_{index}")
                } else {
                    buffer.id
                },
                name: buffer.name,
                arguments: buffer.arguments,
            })
            .collect();
        StreamOutcome {
            text: self.text,
            reasoning: self.reasoning,
            tool_calls,
            usage: self.usage,
            finish_reason: self.finish_reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(json: &str) -> ChatChunk {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn ignores_comments_and_blank_data() {
        assert!(matches!(
            parse_sse_line(": keep-alive"),
            Ok(SseLine::Ignore)
        ));
        assert!(matches!(parse_sse_line("data:"), Ok(SseLine::Ignore)));
        assert!(matches!(
            parse_sse_line("event: message"),
            Ok(SseLine::Ignore)
        ));
    }

    #[test]
    fn parses_data_and_done() {
        assert!(matches!(
            parse_sse_line("data: {\"choices\":[]}").unwrap(),
            SseLine::Chunk(_)
        ));
        assert!(matches!(
            parse_sse_line("data: [DONE]").unwrap(),
            SseLine::Done
        ));
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(parse_sse_line("data: {oops").is_err());
    }

    #[test]
    fn accumulates_text_and_reasoning() {
        let mut accumulator = StreamAccumulator::default();
        accumulator.push_chunk(chunk(
            "{\"choices\":[{\"delta\":{\"reasoning_content\":\"think \"}}]}",
        ));
        let events =
            accumulator.push_chunk(chunk("{\"choices\":[{\"delta\":{\"content\":\"OK\"}}]}"));
        assert_eq!(
            events,
            vec![LlmEvent::Text("OK".to_string())],
            "reasoning deltas are emitted when they arrive"
        );
        let outcome = accumulator.finish();
        assert_eq!(outcome.text, "OK");
        assert_eq!(outcome.reasoning, "think ");
    }

    #[test]
    fn reassembles_tool_call_arguments_across_chunks() {
        let mut accumulator = StreamAccumulator::default();
        accumulator.push_chunk(chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"command\":"}}]}}]}"#,
        ));
        accumulator.push_chunk(chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]}}]}"#,
        ));
        let outcome = accumulator.finish();
        assert_eq!(outcome.tool_calls.len(), 1);
        assert_eq!(outcome.tool_calls[0].id, "call_1");
        assert_eq!(outcome.tool_calls[0].name, "bash");
        assert_eq!(outcome.tool_calls[0].arguments, r#"{"command":"ls"}"#);
    }

    #[test]
    fn keeps_usage_and_finish_reason() {
        let mut accumulator = StreamAccumulator::default();
        accumulator.push_chunk(chunk(
            "{\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}",
        ));
        let outcome = accumulator.finish();
        assert_eq!(outcome.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(outcome.usage.unwrap().total_tokens, 3);
    }
}
