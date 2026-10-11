//! `delve catalog | head -1`: a reader that stops early is not an error (#72).

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use delve_core::prelude::*;
use delve_store_sqlite::SqliteStore;

/// More rows than fit in a pipe buffer (64 KiB on Linux), so the command is
/// still writing when the reader goes away.
const VERSIONS: u32 = 2000;

/// A scratch directory holding a config and a database with one device
/// family at many versions, and version `2.0` of it with many revisions,
/// removed on drop.
struct Fixture(PathBuf);

impl Fixture {
    async fn new() -> Self {
        let dir =
            std::env::temp_dir().join(format!("delve-broken-pipe-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("delve.sqlite");
        std::fs::write(
            dir.join("config.toml"),
            format!("database_path = {:?}\n", db.to_str().unwrap()),
        )
        .unwrap();

        let store = SqliteStore::open(db.to_str().unwrap()).await.unwrap();
        let run = store.start_run("acme", RunKind::Baseline).await.unwrap();
        // Versions 1.0 to 1.1999 for `catalog`, then the revisions of 2.0 for
        // `provenance`: each upsert appends a revision to its entry's history.
        for n in 0..2 * VERSIONS {
            let (raw, ordinal) = if n < VERSIONS {
                (format!("1.{n}"), vec![1, n.into()])
            } else {
                ("2.0".to_string(), vec![2, 0])
            };
            let url = url::Url::parse(&format!("https://example.test/widget-{raw}.bin")).unwrap();
            let firmware_ref = FirmwareRef {
                vendor: "acme".into(),
                device_family: "widget".into(),
                source_url: url.clone(),
                discovered_at: chrono::Utc::now(),
            };
            let meta = FirmwareMetadata {
                vendor: "acme".into(),
                device_family: "widget".into(),
                source_url: url,
                version: VersionKey {
                    raw,
                    scheme: VersionScheme::Semver,
                    ordinal: Some(ordinal),
                },
                release_date: None,
                sha256: Some([(n % 251) as u8; 32]),
                signature: None,
                hardware_targets: vec!["w1".into()],
                release_notes_url: None,
                display_name: Some("Widget".into()),
            };
            store.upsert(&firmware_ref, &meta, run).await.unwrap();
        }
        store.complete_run(run, RunOutcome::Success).await.unwrap();
        Self(dir)
    }

    /// Runs `delve <args>`, reads one line of its stdout and closes the pipe.
    /// Returns that line, the exit status and what it wrote to stderr.
    fn read_one_line(&self, args: &[&str]) -> (String, std::process::ExitStatus, String) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_delve"))
            .arg("--config")
            .arg(self.0.join("config.toml"))
            .args(args)
            .env_remove("RUST_LOG")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut first = String::new();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        stdout.read_line(&mut first).unwrap();
        drop(stdout);
        let out = child.wait_with_output().unwrap();
        (
            first,
            out.status,
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn catalog_and_provenance_end_quietly_when_stdout_closes() {
    let f = Fixture::new().await;
    for args in [
        &["catalog"][..],
        &[
            "provenance",
            "--vendor",
            "acme",
            "--device-family",
            "widget",
            "--version",
            "2.0",
        ],
    ] {
        let (first, status, stderr) = f.read_one_line(args);
        assert!(!first.is_empty(), "{args:?} wrote nothing");
        assert!(status.success(), "{args:?}: {status}: {stderr}");
        assert_eq!(stderr, "", "{args:?}");
    }
}
