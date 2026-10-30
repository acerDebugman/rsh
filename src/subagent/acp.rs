use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::config::Config;

use super::SubagentResult;

type Pending = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// Minimal hand-written ACP v1 client (newline-delimited JSON-RPC on stdio).
/// Kept deliberately small: initialize, session/new, set_config_option,
/// prompt, close, and auto-allow permission requests.
pub struct AcpClient {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
    updates: mpsc::UnboundedReceiver<Value>,
}

impl AcpClient {
    pub async fn spawn(
        command: &str,
        cwd: &Path,
        envs: &[(String, String)],
    ) -> Result<Self, String> {
        let mut process = Command::new("sh");
        process
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in envs {
            process.env(key, value);
        }
        let mut child = process
            .spawn()
            .map_err(|error| format!("spawn acp: {error}"))?;
        let stdin = child.stdin.take().ok_or("acp: no stdin")?;
        let stdout = child.stdout.take().ok_or("acp: no stdout")?;
        let stdin = Arc::new(Mutex::new(stdin));
        let pending: Pending = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (updates_tx, updates) = mpsc::unbounded_channel();

        let reader_stdin = Arc::clone(&stdin);
        let reader_pending = Arc::clone(&pending);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let id = value.get("id").and_then(Value::as_u64);
                let method = value.get("method").and_then(Value::as_str);
                match (id, method) {
                    (Some(id), Some("session/request_permission")) => {
                        let reply = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {"outcome": {"outcome": "selected", "optionId": "allow-once"}}
                        });
                        let mut stdin = reader_stdin.lock().await;
                        let _ = stdin.write_all(format!("{reply}\n").as_bytes()).await;
                        let _ = stdin.flush().await;
                    }
                    (Some(id), Some(_)) => {
                        let reply = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {"code": -32601, "message": "unsupported by rsh client"}
                        });
                        let mut stdin = reader_stdin.lock().await;
                        let _ = stdin.write_all(format!("{reply}\n").as_bytes()).await;
                        let _ = stdin.flush().await;
                    }
                    (Some(id), None) => {
                        if let Some(sender) = reader_pending.lock().unwrap().remove(&id) {
                            let result = if let Some(error) = value.get("error") {
                                Err(error["message"]
                                    .as_str()
                                    .unwrap_or("acp request failed")
                                    .to_string())
                            } else {
                                Ok(value.get("result").cloned().unwrap_or(Value::Null))
                            };
                            let _ = sender.send(result);
                        }
                    }
                    (None, Some("session/update")) => {
                        let _ = updates_tx.send(value);
                    }
                    _ => {}
                }
            }
        });

        Ok(Self {
            child,
            stdin,
            pending,
            next_id: Arc::new(AtomicU64::new(1)),
            updates,
        })
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, sender);
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(format!("{message}\n").as_bytes())
                .await
                .map_err(|error| format!("acp write: {error}"))?;
            stdin
                .flush()
                .await
                .map_err(|error| format!("acp flush: {error}"))?;
        }
        match tokio::time::timeout(Duration::from_secs(180), receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("acp connection closed".to_string()),
            Err(_) => Err(format!("acp {method} timed out")),
        }
    }

    pub fn collect_text(&mut self) -> String {
        let mut text = String::new();
        while let Ok(value) = self.updates.try_recv() {
            let update = &value["params"]["update"];
            if update["sessionUpdate"] == "agent_message_chunk" {
                if let Some(chunk) = update["content"]["text"].as_str() {
                    text.push_str(chunk);
                }
            }
        }
        text
    }

    pub async fn shutdown(mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

/// Run one task over ACP. Returns `Err` for transport/handshake failures so
/// the caller can fall back to the CLI backends.
pub async fn run(
    prompt: &str,
    cwd: &Path,
    config: &Config,
    _timeout: Duration,
) -> Result<SubagentResult, String> {
    let api_key = config.api_key().map_err(|error| error.to_string())?;
    let command =
        std::env::var("RSH_DSH_ACP_CMD").unwrap_or_else(|_| "dsh --profile acp".to_string());
    let mut envs = vec![
        ("DEEPSEEK_API_KEY".to_string(), api_key),
        (
            "DSH_PERMISSION_MODE".to_string(),
            "danger-full-access".to_string(),
        ),
    ];
    for key in ["DSH_HOME", "TMPDIR"] {
        if let Ok(value) = std::env::var(key) {
            envs.push((key.to_string(), value));
        }
    }

    let mut client = AcpClient::spawn(&command, cwd, &envs).await?;
    client
        .request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false
                },
                "clientInfo": {"name": "rsh", "version": env!("CARGO_PKG_VERSION")}
            }),
        )
        .await?;

    let session = client
        .request("session/new", json!({"cwd": cwd, "mcpServers": []}))
        .await?;
    let session_id = session["sessionId"]
        .as_str()
        .ok_or("acp: session/new returned no sessionId")?
        .to_string();

    let model_value = json!(["deepseek-official", config.model]).to_string();
    let _ = client
        .request(
            "session/set_config_option",
            json!({
                "sessionId": session_id,
                "configId": "model",
                "value": model_value
            }),
        )
        .await;

    let result = client
        .request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": prompt}]
            }),
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let text = client.collect_text();
    let stop_reason = result["stopReason"]
        .as_str()
        .unwrap_or("end_turn")
        .to_string();
    let _ = client
        .request("session/close", json!({"sessionId": session_id}))
        .await;
    client.shutdown().await;

    if stop_reason == "cancelled" {
        return Err("acp prompt was cancelled".to_string());
    }
    Ok(SubagentResult::ok(
        text,
        Vec::new(),
        "dsh-acp",
        Some(session_id),
    ))
}
