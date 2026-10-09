//! Commands must finish at any log level.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A scratch directory with a config pointing at a database inside it,
/// removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("delve-logging-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            format!(
                "database_path = {:?}\n",
                dir.join("delve.sqlite").to_str().unwrap()
            ),
        )
        .unwrap();
        Self(dir)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs `delve <args>` with `RUST_LOG=log` and returns its exit status and
/// stdout. Output goes to files, not pipes, so a full pipe cannot be what
/// stalls it; a run that is still going after 30 seconds is killed and fails
/// the test.
fn run_with_log(dir: &Dir, log: &str, args: &[&str]) -> (std::process::ExitStatus, String) {
    let stdout_path = dir.0.join("stdout.txt");
    let mut child = Command::new(env!("CARGO_BIN_EXE_delve"))
        .arg("--config")
        .arg(dir.0.join("config.toml"))
        .args(args)
        .env("RUST_LOG", log)
        .env_remove("RUST_BACKTRACE")
        .stdout(std::fs::File::create(&stdout_path).unwrap())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("`delve {args:?}` with RUST_LOG={log} did not exit within 30s");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    (status, std::fs::read_to_string(&stdout_path).unwrap())
}

#[test]
fn catalog_finishes_with_the_database_logging_at_debug_and_trace() {
    // sqlx logs each query from its own thread; with stdout locked for the
    // whole command, that thread could never write and the query never ended.
    for log in ["sqlx=debug", "debug", "trace"] {
        let dir = Dir::new();
        let (status, stdout) = run_with_log(&dir, log, &["catalog"]);
        assert!(status.success(), "RUST_LOG={log}: {status}");
        assert!(
            stdout.contains("No matching firmware entries."),
            "RUST_LOG={log}: {stdout:?}"
        );
    }
}
