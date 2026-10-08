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
const UTC_SHAPE: &str = "dddd-dd-ddTdd:dd:dd.ddddddZ";
/// The same, in local time: `2026-10-05T14:44:18.740169-04:00`.
const LOCAL_SHAPE: &str = "dddd-dd-ddTdd:dd:dd.dddddd+dd:dd";

fn shape(stamp: &str) -> String {
    let mut shape: Vec<char> = stamp
        .chars()
        .map(|c| if c.is_ascii_digit() { 'd' } else { c })
        .collect();
    // A local stamp's offset may be west of Greenwich.
    if shape.len() == LOCAL_SHAPE.len() && shape[26] == '-' {
        shape[26] = '+';
    }
    shape.into_iter().collect()
}

/// Splits `<stamp> <rest>` at the first space.
fn split_stamp(line: &str) -> (&str, &str) {
    line.split_once(' ').unwrap_or((line, ""))
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
    // Local time is the default, the same as for the timestamps in tables.
    assert_eq!(shape(stamp), LOCAL_SHAPE, "{err}");
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
    assert_eq!(shape(stamp), LOCAL_SHAPE, "{err}");
    assert_eq!(rest, "Error: unknown vendor: nope");
}

#[test]
fn utc_gives_the_z_form_to_the_error_and_the_log_lines() {
    let dir = Dir::new();
    // At trace level the database being opened logs lines before the failure.
    let out = dir.run(
        &dir.config(),
        &[("RUST_LOG", "trace")],
        &["dig", "--vendor", "nope", "--utc"],
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let log_line =
        anstream::adapter::strip_str(stdout.lines().next().expect("a log line")).to_string();
    let err = String::from_utf8(out.stderr).unwrap();

    assert_eq!(shape(split_stamp(&log_line).0), UTC_SHAPE, "{log_line}");
    assert_eq!(shape(split_stamp(err.trim_end()).0), UTC_SHAPE, "{err}");
}

#[test]
fn the_error_and_the_log_lines_agree_on_the_default_local_form() {
    let dir = Dir::new();
    let out = dir.run(
        &dir.config(),
        &[("RUST_LOG", "trace")],
        &["dig", "--vendor", "nope"],
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let log_line =
        anstream::adapter::strip_str(stdout.lines().next().expect("a log line")).to_string();
    let err = String::from_utf8(out.stderr).unwrap();

    assert_eq!(shape(split_stamp(&log_line).0), LOCAL_SHAPE, "{log_line}");
    assert_eq!(shape(split_stamp(err.trim_end()).0), LOCAL_SHAPE, "{err}");
    // The same offset on both streams.
    let offset = |s: &str| split_stamp(s).0[26..].to_string();
    assert_eq!(offset(&log_line), offset(err.trim_end()));
}

/// chrono reads `TZ` on Unix; Windows ignores it and asks the OS.
#[cfg(unix)]
#[test]
fn the_machines_timezone_sets_the_offset_and_utc_overrides_it() {
    let dir = Dir::new();
    let kolkata = [("TZ", "Asia/Kolkata"), ("RUST_LOG", "trace")];
    let out = dir.run(&dir.config(), &kolkata, &["dig", "--vendor", "nope"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let log_line = anstream::adapter::strip_str(stdout.lines().next().unwrap()).to_string();
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(split_stamp(&log_line).0.ends_with("+05:30"), "{log_line}");
    assert!(split_stamp(err.trim_end()).0.ends_with("+05:30"), "{err}");

    let out = dir.run(
        &dir.config(),
        &kolkata,
        &["dig", "--vendor", "nope", "--utc"],
    );
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(split_stamp(err.trim_end()).0.ends_with('Z'), "{err}");
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
