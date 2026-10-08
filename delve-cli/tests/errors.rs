//! What reaches stderr when `delve` fails: the error, stamped like every log line.

use std::path::PathBuf;
use std::process::{Command, Output};

/// A scratch directory with a config pointing at a database inside it,
/// removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("delve-errors-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("delve.sqlite");
        std::fs::write(
            dir.join("config.toml"),
            format!("database_path = {:?}\n", db.to_str().unwrap()),
        )
        .unwrap();
        Self(dir)
    }

    fn config(&self) -> PathBuf {
        self.0.join("config.toml")
    }

    fn run(&self, config: &PathBuf, env: &[(&str, &str)], args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_delve"))
            .arg("--config")
            .arg(config)
            .args(args)
            .env_remove("RUST_LOG")
            // A backtrace after the error would add lines; CI sets these.
            .env_remove("RUST_BACKTRACE")
            .env_remove("RUST_LIB_BACKTRACE")
            .envs(env.iter().copied())
            .output()
            .unwrap()
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `2026-10-05T18:44:18.740169Z` with every digit replaced by `d`.
const STAMP_SHAPE: &str = "dddd-dd-ddTdd:dd:dd.ddddddZ";

fn shape(stamp: &str) -> String {
    stamp
        .chars()
        .map(|c| if c.is_ascii_digit() { 'd' } else { c })
        .collect()
}

/// Splits `<stamp> <rest>` after the 27-character timestamp.
fn split_stamp(line: &str) -> (&str, &str) {
    let (stamp, rest) = line.split_at_checked(27).unwrap_or((line, ""));
    (stamp, rest.strip_prefix(' ').unwrap_or(rest))
}

#[test]
fn a_fatal_error_is_stamped_and_the_message_is_unchanged() {
    let dir = Dir::new();
    let missing = dir.0.join("missing.toml");
    let out = dir.run(&missing, &[], &["catalog"]);

    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let err = String::from_utf8(out.stderr).unwrap();
    assert_eq!(err.lines().count(), 1, "{err}");
    let (stamp, rest) = split_stamp(err.trim_end());
    assert_eq!(shape(stamp), STAMP_SHAPE, "{err}");
    assert!(
        rest.starts_with("Error: failed to read config at "),
        "{err}"
    );
}

#[test]
fn an_error_from_a_running_command_is_stamped_too() {
    let dir = Dir::new();
    let out = dir.run(&dir.config(), &[], &["dig", "--vendor", "nope"]);

    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8(out.stderr).unwrap();
    let (stamp, rest) = split_stamp(err.trim_end());
    assert_eq!(shape(stamp), STAMP_SHAPE, "{err}");
    assert_eq!(rest, "Error: unknown vendor: nope");
}

#[test]
fn the_error_and_the_log_lines_use_the_same_timestamp_format() {
    let dir = Dir::new();
    // At trace level the database being opened logs lines before the failure.
    let out = dir.run(
        &dir.config(),
        &[("RUST_LOG", "trace")],
        &["dig", "--vendor", "nope"],
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let log_line =
        anstream::adapter::strip_str(stdout.lines().next().expect("a log line")).to_string();
    let err = String::from_utf8(out.stderr).unwrap();

    assert_eq!(shape(split_stamp(&log_line).0), STAMP_SHAPE, "{log_line}");
    assert_eq!(shape(split_stamp(err.trim_end()).0), STAMP_SHAPE, "{err}");
}

#[test]
fn a_usage_error_is_clap_s_own_message_with_exit_status_2() {
    let dir = Dir::new();
    let out = dir.run(&dir.config(), &[], &["catalog", "--color=sometimes"]);

    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.starts_with("error: invalid value"), "{err}");
}

#[test]
fn success_writes_nothing_to_stderr() {
    let dir = Dir::new();
    let out = dir.run(&dir.config(), &[], &["catalog"]);

    assert_eq!(out.status.code(), Some(0));
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
