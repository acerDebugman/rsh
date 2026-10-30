use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

fn live_enabled() -> bool {
    std::env::var("RSH_LIVE").ok().as_deref() == Some("1")
}

fn scratch_dir() -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/e2e-grill");
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn send(writer: &mut Box<dyn Write + Send>, text: &str) {
    let _ = writer.write_all(text.as_bytes());
    let _ = writer.write_all(b"\r");
    let _ = writer.flush();
}

fn press_enter(writer: &mut Box<dyn Write + Send>) {
    let _ = writer.write_all(b"\r");
    let _ = writer.flush();
}

fn has_sections(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| {
        ["目标", "约束", "决策", "风险", "下一步"]
            .iter()
            .all(|section| text.contains(section))
    })
}

/// The status line says `Enter send` when idle and `working…` while a turn
/// runs. Wait until the newest status marker is the idle one.
async fn wait_idle(output: &Arc<Mutex<Vec<u8>>>, mark: usize, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        {
            let buffer = output.lock().unwrap();
            let slice = if mark < buffer.len() {
                &buffer[mark..]
            } else {
                &[][..]
            };
            let tail = String::from_utf8_lossy(slice);
            let busy = tail.rfind("working");
            let idle_marker = tail
                .rfind("Ctrl+J")
                .into_iter()
                .chain(tail.rfind("answer the question"))
                .max();
            match (idle_marker, busy) {
                (Some(idle), Some(busy)) if idle > busy => return true,
                (Some(_), None) => return true,
                _ => {}
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test]
async fn grill_me_interview_writes_the_result_file() {
    if !live_enabled() {
        eprintln!("skipping live e2e: set RSH_LIVE=1 to run it");
        return;
    }
    let dir = scratch_dir();
    let home = dir.join(".rsh");
    let repo_result = Path::new(env!("CARGO_MANIFEST_DIR")).join("grill-me-result.md");
    let _ = std::fs::remove_file(&repo_result);
    let candidates: Vec<PathBuf> = vec![dir.join("grill-me-result.md"), repo_result];
    let find_result = || {
        candidates
            .iter()
            .find(|path| path.is_file() && has_sections(path))
            .cloned()
    };

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 34,
            cols: 110,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_rsh"));
    command.cwd(&dir);
    command.env("RSH_HOME", &home);
    let mut child = pair.slave.spawn_command(command).expect("spawn rsh");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("pty reader");
    let mut writer = pair.master.take_writer().expect("pty writer");
    let output = Arc::new(Mutex::new(Vec::<u8>::new()));
    let reader_output = Arc::clone(&output);
    let log_path = dir.join("pty.log");
    std::thread::spawn(move || {
        let mut log = std::fs::File::create(log_path).unwrap();
        let mut buffer = [0u8; 8192];
        while let Ok(read) = reader.read(&mut buffer) {
            if read == 0 {
                break;
            }
            let _ = log.write_all(&buffer[..read]);
            let _ = log.flush();
            let mut captured = reader_output.lock().unwrap();
            captured.extend_from_slice(&buffer[..read]);
            if captured.len() > 300_000 {
                let drain = captured.len() - 200_000;
                captured.drain(..drain);
            }
        }
    });

    tokio::time::sleep(Duration::from_secs(3)).await;
    let mark = output.lock().unwrap().len();
    send(
        &mut writer,
        "/skill grill-me 不要读取任何文件或仓库，只基于下面的描述提问。我要用 Rust 写一个终端 coding agent：自研 loop、9 个工具、skills、native/dsh 子代理、默认全开权限。请就范围、权限、测试各追问我一次，然后收敛。",
    );
    assert!(
        wait_idle(&output, mark, Duration::from_secs(180)).await,
        "initial turn did not finish"
    );

    let mut result = None;
    let mark = output.lock().unwrap().len();
    send(
        &mut writer,
        "都按你的建议，直接收敛。请把最终结论写到 grill-me-result.md，包含目标、约束、决策、风险、下一步五节。",
    );
    assert!(
        wait_idle(&output, mark, Duration::from_secs(240)).await,
        "convergence turn did not finish"
    );
    for _ in 0..60 {
        if let Some(path) = find_result() {
            result = Some(path);
            break;
        }
        press_enter(&mut writer);
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    let _ = child.kill();
    let path = result.expect("grill-me-result.md was not written with all five sections");
    let text = std::fs::read_to_string(&path).expect("read result");
    for section in ["目标", "约束", "决策", "风险", "下一步"] {
        assert!(
            text.contains(section),
            "result is missing section: {section}"
        );
    }
}
