//! The release gate's one terminal test: `rsh` in a real PTY, against a fake
//! model. No network, no key, no human.
//!
//! Everything here exists because the key semantics of the composer are
//! invisible to unit tests: `Enter` sends, `Ctrl+J` inserts a newline, `Esc`
//! clears, bracketed paste arrives as one paste, and `!cmd` runs a shell
//! without waking the model. Each assertion reads the *request body* the fake
//! model received, so it tests the whole path (raw mode → crossterm → composer
//! → agent → HTTP), not a helper function.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const IDLE_MARKER: &str = "Ctrl+J";
const BUSY_MARKER: &str = "working";
const READY_MARKER: &str = "test-model";

/// A fake OpenAI-compatible endpoint: records each request body and answers
/// with a one-chunk SSE stream. Raw sockets keep the test dependency-free.
fn spawn_fake_model() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake model");
    let address = listener.local_addr().expect("local addr");
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&bodies);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let Ok(request) = read_request(&mut stream) else {
                continue;
            };
            recorded.lock().expect("bodies").push(request);
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n\
                        data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                        data: [DONE]\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/event-stream\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://{address}"), bodies)
}

/// Read exactly one HTTP/1.1 request (headers plus `Content-Length` body).
fn read_request(stream: &mut std::net::TcpStream) -> std::io::Result<String> {
    let mut raw = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(std::io::Error::other("closed before headers"));
        }
        raw.extend_from_slice(&chunk[..read]);
        if let Some(position) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while raw.len() < header_end + length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
    }
    Ok(String::from_utf8_lossy(&raw[header_end..]).to_string())
}

struct Pty {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    screen: Arc<Mutex<Vec<u8>>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    rows: u16,
}

impl Pty {
    fn spawn(workspace: &std::path::Path, home: &std::path::Path, base_url: &str) -> Self {
        let rows = 30_u16;
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols: 200,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_rsh"));
        command.cwd(workspace);
        command.env("TERM", "xterm-256color");
        command.env("RSH_HOME", home);
        command.env("RSH_BASE_URL", base_url);
        command.env("RSH_API_KEY", "test-key-not-a-secret");
        command.env("RSH_MODEL", "test-model");
        let child = pair.slave.spawn_command(command).expect("spawn rsh");
        drop(pair.slave);

        let screen = Arc::new(Mutex::new(Vec::new()));
        let writer: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(pair.master.take_writer().expect("pty writer")));

        // A dumb terminal still has to answer DSR: ratatui's inline viewport
        // blocks on a cursor-position report before it can draw.
        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        let sink = Arc::clone(&screen);
        let responder = Arc::clone(&writer);
        std::thread::spawn(move || {
            let mut answered = 0;
            let mut buffer = [0_u8; 4096];
            loop {
                let read = match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
                };
                let mut sink = sink.lock().expect("screen");
                sink.extend_from_slice(&buffer[..read]);
                let queries = sink
                    .windows(4)
                    .filter(|window| window == b"\x1b[6n")
                    .count();
                drop(sink);
                while answered < queries {
                    answered += 1;
                    // Report the last row so the viewport lands at the bottom.
                    let _ = responder
                        .lock()
                        .expect("writer")
                        .write_all(format!("\x1b[{rows};1R").as_bytes());
                }
            }
        });

        Self {
            writer,
            screen,
            child,
            rows,
        }
    }

    fn send(&self, bytes: &[u8]) {
        let mut writer = self.writer.lock().expect("writer");
        writer.write_all(bytes).expect("write keys");
        writer.flush().expect("flush keys");
    }

    fn screen(&self) -> String {
        String::from_utf8_lossy(&self.screen.lock().expect("screen")).into_owned()
    }

    fn wait_for(&self, what: &str, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.screen().contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!(
            "timed out waiting for {what} ({needle:?}); last screen:\n{}",
            tail(&self.screen(), 2000)
        );
    }

    /// Idle means the composer's idle status line was drawn more recently than
    /// the busy one; the viewport redraws in place, so the last marker wins.
    fn wait_until_idle(&self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            let screen = self.screen();
            let busy = screen.rfind(BUSY_MARKER);
            let idle = screen.rfind(IDLE_MARKER);
            if idle.is_some() && idle > busy {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!(
            "timed out waiting for the composer to become idle; last screen:\n{}",
            tail(&self.screen(), 2000)
        );
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn tail(text: &str, chars: usize) -> String {
    let total = text.chars().count();
    text.chars().skip(total.saturating_sub(chars)).collect()
}

/// The last message the fake model received, i.e. what the composer sent.
fn last_prompt(body: &str) -> String {
    let request: serde_json::Value = serde_json::from_str(body).expect("request json");
    request["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default()
        .to_string()
}

fn wait_for_requests(
    bodies: &Arc<Mutex<Vec<String>>>,
    count: usize,
    timeout: Duration,
) -> Vec<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let recorded = bodies.lock().expect("bodies");
        if recorded.len() >= count {
            return recorded.clone();
        }
        drop(recorded);
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "timed out waiting for request #{count}; got {}",
        bodies.lock().expect("bodies").len()
    );
}

fn scratch_dir(prefix: &str) -> std::path::PathBuf {
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp");
    std::fs::create_dir_all(&base).expect("test tmp");
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(&base)
        .expect("tempdir")
        .keep()
}

#[test]
fn the_composer_keys_keep_their_meanings_in_a_real_terminal() {
    let workspace = scratch_dir("pty-workspace-");
    let home = scratch_dir("pty-home-");
    let (base_url, bodies) = spawn_fake_model();
    let pty = Pty::spawn(&workspace, &home, &base_url);
    pty.wait_for("the first draw", READY_MARKER, Duration::from_secs(20));
    pty.wait_until_idle();

    // Enter sends.
    pty.send(b"first prompt");
    pty.send(b"\r");
    let recorded = wait_for_requests(&bodies, 1, Duration::from_secs(20));
    assert_eq!(last_prompt(&recorded[0]), "first prompt");
    pty.wait_until_idle();

    // Bracketed paste is one paste, not a burst of keystrokes.
    pty.send(b"\x1b[200~pasted text\x1b[201~");
    pty.send(b"\r");
    let recorded = wait_for_requests(&bodies, 2, Duration::from_secs(20));
    assert_eq!(last_prompt(&recorded[1]), "pasted text");
    pty.wait_until_idle();

    // Ctrl+J inserts a newline instead of sending.
    pty.send(b"line one");
    pty.send(b"\n");
    pty.send(b"line two");
    pty.send(b"\r");
    let recorded = wait_for_requests(&bodies, 3, Duration::from_secs(20));
    assert_eq!(last_prompt(&recorded[2]), "line one\nline two");
    pty.wait_until_idle();

    // Esc clears the composer.
    pty.send(b"discard me");
    std::thread::sleep(Duration::from_millis(200));
    pty.send(b"\x1b");
    std::thread::sleep(Duration::from_millis(500));
    pty.send(b"kept");
    pty.send(b"\r");
    let recorded = wait_for_requests(&bodies, 4, Duration::from_secs(20));
    assert_eq!(last_prompt(&recorded[3]), "kept");
    pty.wait_until_idle();

    // `!cmd` runs the shell directly: output appears, the model is not called.
    pty.send(b"!printf SHELL_MARKER_FROM_PTY");
    pty.send(b"\r");
    pty.wait_for(
        "the shell shortcut output",
        "SHELL_MARKER_FROM_PTY",
        Duration::from_secs(20),
    );
    pty.wait_until_idle();
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(
        bodies.lock().expect("bodies").len(),
        4,
        "`!cmd` must not reach the model"
    );

    // The session log holds the turns, including the ones full access ran.
    let sessions = home.join("sessions");
    let log = walk_jsonl(&sessions).expect("events.jsonl under RSH_HOME");
    let log_text = std::fs::read_to_string(log).expect("read events.jsonl");
    for prompt in ["first prompt", "pasted text", "kept"] {
        assert!(
            log_text.contains(prompt),
            "missing {prompt:?} in the session log"
        );
    }
    let _ = pty.rows;
}

fn walk_jsonl(root: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|name| name == "events.jsonl") {
                return Some(path);
            }
        }
    }
    None
}
