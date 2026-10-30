use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::llm::openai::{LlmEvent, SseLine, StreamAccumulator, StreamOutcome, parse_sse_line};
use crate::llm::types::ChatRequest;

/// Transport-agnostic streaming interface used by the agent loop and mocked in
/// tests.
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn stream_chat(
        &self,
        request: ChatRequest,
        events: mpsc::Sender<LlmEvent>,
        cancel: CancellationToken,
    ) -> Result<StreamOutcome>;
}

#[derive(Clone)]
pub struct OpenAiClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    max_tokens: u32,
    temperature: f32,
    send_reasoning: bool,
}

impl OpenAiClient {
    pub fn from_config(config: &Config) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .build()
                .context("build http client")?,
            base_url: config.base_url.clone(),
            api_key: config.api_key()?,
            model: config.model.clone(),
            max_tokens: config.max_tokens,
            temperature: config.temperature,
            send_reasoning: config.send_reasoning,
        })
    }

    /// Test/embedded constructor with small defaults.
    #[must_use]
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .expect("build http client"),
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            max_tokens: 1024,
            temperature: 0.0,
            send_reasoning: false,
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    async fn send(&self, body: &Value, cancel: &CancellationToken) -> Result<reqwest::Response> {
        let mut delay = Duration::from_millis(250);
        for attempt in 0..3 {
            let request = self
                .http
                .post(self.endpoint())
                .bearer_auth(&self.api_key)
                .json(body);
            let response = tokio::select! {
                () = cancel.cancelled() => bail!("request cancelled"),
                response = request.send() => response,
            };
            match response {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) if retryable_status(response.status()) && attempt < 2 => {
                    tokio::time::sleep(delay).await;
                    delay *= 4;
                }
                Ok(response) => {
                    let status = response.status();
                    let text = response.text().await.unwrap_or_default();
                    bail!("LLM HTTP {status}: {text}");
                }
                Err(error) if retryable_error(&error) && attempt < 2 => {
                    tokio::time::sleep(delay).await;
                    delay *= 4;
                }
                Err(error) => return Err(error).context("send chat request"),
            }
        }
        unreachable!("retry loop returns on the final attempt")
    }

    async fn handle_line(
        accumulator: &mut StreamAccumulator,
        events: &mpsc::Sender<LlmEvent>,
        line: &[u8],
    ) -> Result<bool> {
        let text = String::from_utf8_lossy(line);
        match parse_sse_line(&text)? {
            SseLine::Ignore => Ok(false),
            SseLine::Done => Ok(true),
            SseLine::Chunk(chunk) => {
                for event in accumulator.push_chunk(chunk) {
                    let _ = events.send(event).await;
                }
                Ok(false)
            }
        }
    }
}

#[async_trait]
impl LlmClient for OpenAiClient {
    async fn stream_chat(
        &self,
        mut request: ChatRequest,
        events: mpsc::Sender<LlmEvent>,
        cancel: CancellationToken,
    ) -> Result<StreamOutcome> {
        request.model = self.model.clone();
        request.max_tokens = self.max_tokens;
        request.temperature = self.temperature;
        request.send_reasoning = self.send_reasoning;

        let body = request.to_json();
        let response = self.send(&body, &cancel).await?;
        let mut stream = response.bytes_stream();
        let mut accumulator = StreamAccumulator::default();
        let mut buffer: Vec<u8> = Vec::new();

        loop {
            let next = tokio::select! {
                () = cancel.cancelled() => bail!("request cancelled"),
                next = stream.next() => next,
            };
            match next {
                Some(Ok(bytes)) => {
                    buffer.extend_from_slice(&bytes);
                    while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
                        let line: Vec<u8> = buffer.drain(..=position).collect();
                        if Self::handle_line(&mut accumulator, &events, &line).await? {
                            return Ok(accumulator.finish());
                        }
                    }
                }
                Some(Err(error)) => return Err(error).context("read SSE stream"),
                None => {
                    if !buffer.is_empty() {
                        let line = std::mem::take(&mut buffer);
                        let _ = Self::handle_line(&mut accumulator, &events, &line).await?;
                    }
                    return Ok(accumulator.finish());
                }
            }
        }
    }
}

fn retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn retryable_error(error: &reqwest::Error) -> bool {
    error.is_connect() || error.is_timeout()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::Message;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn serve(response: String) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&connections);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut request = [0u8; 8192];
                let _ = socket.read(&mut request).await;
                socket.write_all(response.as_bytes()).await.unwrap();
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://{address}"), connections)
    }

    fn http_response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn request() -> ChatRequest {
        ChatRequest {
            model: String::new(),
            messages: vec![Message::user("hi")],
            tools: Vec::new(),
            max_tokens: 0,
            temperature: 0.0,
            send_reasoning: false,
        }
    }

    #[tokio::test]
    async fn streams_text_tool_call_and_usage() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"ls\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}\n\n",
            "data: [DONE]\n\n",
        );
        let (base, server) = serve(http_response("200 OK", body)).await;
        let client = OpenAiClient::new(base, "test-key", "deepseek-flash");
        let (tx, mut rx) = mpsc::channel(64);

        let outcome = client
            .stream_chat(request(), tx, CancellationToken::new())
            .await
            .unwrap();

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LlmEvent::Text(text) if text == "OK"))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LlmEvent::ToolCall { .. }))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LlmEvent::Usage(_)))
        );
        assert_eq!(outcome.text, "OK");
        assert_eq!(outcome.tool_calls.len(), 1);
        assert_eq!(outcome.tool_calls[0].name, "bash");
        assert_eq!(outcome.usage.unwrap().total_tokens, 3);
        assert!(server.load(std::sync::atomic::Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn unauthorized_is_not_retried() {
        let (base, server) = serve(http_response("401 Unauthorized", "nope")).await;
        let client = OpenAiClient::new(base, "bad-key", "deepseek-flash");
        let (tx, _rx) = mpsc::channel(8);

        let error = client
            .stream_chat(request(), tx, CancellationToken::new())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("401"), "{error}");
        assert_eq!(
            server.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "401 must not be retried"
        );
    }

    #[tokio::test]
    async fn cancellation_stops_the_stream() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
        let (base, _server) = serve(http_response("200 OK", body)).await;
        let client = OpenAiClient::new(base, "test-key", "deepseek-flash");
        let (tx, _rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let error = client.stream_chat(request(), tx, cancel).await.unwrap_err();
        assert!(error.to_string().contains("cancelled"), "{error}");
    }
}
