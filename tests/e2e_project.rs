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
        .prefix("e2e-project-")
        .tempdir_in(&base)
        .unwrap()
}

#[tokio::test]
#[ignore = "live: needs a real API key and network; run with --ignored"]
async fn builds_a_python_todo_cli_with_tests() {
    require_live();
    let dir = scratch_dir();
    let prompt = "\
Create a Python 3 todo CLI in the current directory using only the standard library:\n\
- todo.py with commands: add <text>, list, done <index>; state persisted in todo.json\n\
- test_todo.py with unittest tests covering add, list, and done\n\
Run `python3 -m unittest -q` and fix failures until it passes.\n\
Do not create any other files.";
    let child = Command::new(env!("CARGO_BIN_EXE_rsh"))
        .arg("exec")
        .arg("--yolo")
        .arg(prompt)
        .current_dir(dir.path())
        .env("RSH_HOME", dir.path().join(".rsh"))
        .kill_on_drop(true)
        .output();

    let output = tokio::time::timeout(Duration::from_secs(480), child)
        .await
        .expect("rsh exec timed out")
        .expect("spawn rsh");
    assert!(
        output.status.success(),
        "rsh failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    for file in ["todo.py", "test_todo.py"] {
        assert!(
            dir.path().join(file).is_file(),
            "missing {file}; stdout:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    let status = Command::new("python3")
        .args(["-m", "unittest", "-q"])
        .current_dir(dir.path())
        .status()
        .await
        .expect("run unittest");
    assert!(status.success(), "python unittest failed");
}
