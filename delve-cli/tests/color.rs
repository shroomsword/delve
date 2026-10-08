//! `--color` on the real binary: what `catalog` and `provenance` write to a
//! pipe (which is all a test can give them) under each choice.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use delve_core::prelude::*;
use delve_store_sqlite::SqliteStore;

/// A scratch directory holding a config and a database with one firmware
/// entry observed once, removed on drop.
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    async fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("delve-color-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("delve.sqlite");
        std::fs::write(
            dir.join("config.toml"),
            format!("database_path = {:?}\n", db.to_str().unwrap()),
        )
        .unwrap();

        let store = SqliteStore::open(db.to_str().unwrap()).await.unwrap();
        let url = url::Url::parse("https://example.test/widget-1.0.bin").unwrap();
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
                raw: "1.0".into(),
                scheme: VersionScheme::Semver,
                ordinal: Some(vec![1, 0]),
            },
            release_date: None,
            sha256: Some([7; 32]),
            signature: None,
            hardware_targets: vec!["w1".into()],
            release_notes_url: None,
            display_name: Some("Widget".into()),
        };
        let run = store.start_run("acme", RunKind::Baseline).await.unwrap();
        store.upsert(&firmware_ref, &meta, run).await.unwrap();
        store.complete_run(run, RunOutcome::Success).await.unwrap();
        Self { dir }
    }

    /// Runs `delve <args>` with a clean color environment plus `env`.
    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_delve"));
        cmd.arg("--config")
            .arg(self.dir.join("config.toml"))
            .args(args);
        for var in ["NO_COLOR", "CLICOLOR", "CLICOLOR_FORCE", "TERM"] {
            cmd.env_remove(var);
        }
        cmd.envs(env.iter().copied());
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    fn stdout(&self, args: &[&str], env: &[(&str, &str)]) -> String {
        String::from_utf8(self.run(args, env).stdout).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        remove(&self.dir);
    }
}

fn remove(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

const COMMANDS: &[&[&str]] = &[
    &["catalog"],
    &["catalog", "--long"],
    &["catalog", "--latest"],
    &[
        "provenance",
        "--vendor",
        "acme",
        "--device-family",
        "widget",
    ],
];

fn colored(out: &str) -> bool {
    out.contains('\x1b')
}

#[tokio::test]
async fn piped_output_has_no_escape_sequences_by_default() {
    let f = Fixture::new().await;
    for args in COMMANDS {
        let out = f.stdout(args, &[]);
        assert!(!colored(&out), "{args:?}: {out:?}");
        assert!(out.contains("1.0"), "{args:?}: {out:?}");
    }
}

#[tokio::test]
async fn never_removes_all_escape_sequences() {
    let f = Fixture::new().await;
    for args in COMMANDS {
        let mut with = args.to_vec();
        with.extend(["--color=never"]);
        let out = f.stdout(&with, &[("CLICOLOR_FORCE", "1")]);
        assert!(!colored(&out), "{args:?}: {out:?}");
    }
}

#[tokio::test]
async fn always_colors_even_when_piped() {
    let f = Fixture::new().await;
    for args in COMMANDS {
        let mut with = args.to_vec();
        with.extend(["--color", "always"]);
        let out = f.stdout(&with, &[]);
        assert!(colored(&out), "{args:?}: {out:?}");
    }
}

#[tokio::test]
async fn always_wins_over_no_color_and_a_dumb_terminal() {
    let f = Fixture::new().await;
    let out = f.stdout(
        &["--color=always", "catalog"],
        &[("NO_COLOR", "1"), ("TERM", "dumb")],
    );
    assert!(colored(&out), "{out:?}");
}

#[tokio::test]
async fn color_choice_is_accepted_before_or_after_the_subcommand() {
    let f = Fixture::new().await;
    let before = f.stdout(&["--color=always", "catalog"], &[]);
    let after = f.stdout(&["catalog", "--color=always"], &[]);
    assert_eq!(before, after);
}

#[tokio::test]
async fn escapes_do_not_change_what_is_printed() {
    let f = Fixture::new().await;
    for args in COMMANDS {
        let mut always = args.to_vec();
        always.push("--color=always");
        let mut never = args.to_vec();
        never.push("--color=never");
        let styled = f.stdout(&always, &[]);
        let plain = f.stdout(&never, &[]);
        assert!(colored(&styled), "{args:?}");
        // Stripping the escapes leaves exactly the plain output, so no column
        // moved.
        assert_eq!(
            anstream::adapter::strip_str(&styled).to_string(),
            plain,
            "{args:?}"
        );
    }
}

#[tokio::test]
async fn an_unknown_color_choice_is_a_usage_error() {
    let f = Fixture::new().await;
    let out = Command::new(env!("CARGO_BIN_EXE_delve"))
        .arg("--config")
        .arg(f.dir.join("config.toml"))
        .args(["catalog", "--color=sometimes"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("auto") && err.contains("always") && err.contains("never"),
        "{err}"
    );
}
