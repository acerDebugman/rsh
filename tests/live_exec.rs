use std::time::Duration;

use tokio::process::Command;

/// Live tests are `#[ignore]`d: a normal `cargo test` must never report them
/// as passing. Running them costs money and needs a real key, so they also
/// demand `RSH_LIVE=1` as a second, deliberate confirmation.
fn require_live() {
    assert_eq!(
        std::env::var("RSH_LIVE").ok().as_deref(),
        Some("1"),
        "set RSH_LIVE=1 to run live tests"
    );
}

fn scratch_dir() -> tempfile::TempDir {
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp");
    std::fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("live-")
        .tempdir_in(&base)
        .unwrap()
}

#[tokio::test]
#[ignore = "live: needs a real API key and network; run with --ignored"]
async fn exec_creates_a_file_with_content() {
    require_live();
    let dir = scratch_dir();
    let prompt =
        "Create a file named a.txt whose content is exactly: hi. Then run `cat a.txt` to verify.";
    let child = Command::new(env!("CARGO_BIN_EXE_rsh"))
        .arg("exec")
        .arg("--yolo")
        .arg(prompt)
        .current_dir(dir.path())
        .env("RSH_HOME", dir.path().join(".rsh"))
        .kill_on_drop(true)
        .output();

    let output = tokio::time::timeout(Duration::from_secs(300), child)
        .await
        .expect("rsh exec timed out")
        .expect("spawn rsh");

    assert!(
        output.status.success(),
        "rsh failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let content = std::fs::read_to_string(dir.path().join("a.txt")).expect("a.txt should exist");
    assert!(content.contains("hi"), "unexpected content: {content:?}");
}
