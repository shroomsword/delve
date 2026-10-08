//! How a fatal error reaches stderr.
//!
//! `main` used to return the error and let the runtime print a bare
//! `Error: ...`, the one line in a cron log with no time on it. It is written
//! here instead, behind the same timestamp as every other log line, and with
//! the message exactly as the runtime printed it.

use std::io::Write;

use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};

/// The clock for every log line, fatal errors included. Both the log
/// subscriber and [`write_fatal`] take it from here, so the two cannot drift
/// apart.
pub(crate) fn timer() -> SystemTime {
    SystemTime
}

/// `<timestamp> Error: <message>`, with anyhow's `Caused by:` lines after it as
/// before. Only the first line is stamped: the rest belong to the same record.
fn fatal_text(timer: &impl FormatTime, err: &anyhow::Error) -> String {
    let mut stamp = String::new();
    // Formatting into a `String` cannot fail; a timer that does leaves no stamp.
    let _ = timer.format_time(&mut Writer::new(&mut stamp));
    format!("{stamp} Error: {err:?}")
}

/// Writes the error that ends the program to stderr. A closed stderr is
/// ignored: there is nowhere left to report it.
pub(crate) fn write_fatal(err: &anyhow::Error) {
    let _ = writeln!(std::io::stderr(), "{}", fatal_text(&timer(), err));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock that always reads the same, so the output can be compared.
    struct Fixed;

    impl FormatTime for Fixed {
        fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
            write!(w, "2026-10-05T18:44:18.740169Z")
        }
    }

    #[test]
    fn the_message_follows_the_timestamp_unchanged() {
        let err = anyhow::anyhow!(
            "[subscribers.email] password references env var 'RESEND_API_KEY', which is not set"
        );
        // Starts with, because a backtrace follows when `RUST_BACKTRACE` is set
        // (CI sets it), exactly as it did when the runtime printed the error.
        let text = fatal_text(&Fixed, &err);
        assert!(
            text.starts_with(
                "2026-10-05T18:44:18.740169Z Error: [subscribers.email] password references env \
                 var 'RESEND_API_KEY', which is not set"
            ),
            "{text}"
        );
    }

    #[test]
    fn the_cause_chain_is_kept_after_a_stamped_first_line() {
        let err = anyhow::anyhow!("no such file")
            .context("failed to read config")
            .context("dig failed");
        let text = fatal_text(&Fixed, &err);
        let plain = format!("Error: {err:?}");
        assert_eq!(text, format!("2026-10-05T18:44:18.740169Z {plain}"));
        assert!(text.contains("\n\nCaused by:\n"), "{text}");
        assert_eq!(text.lines().filter(|l| l.starts_with("2026")).count(), 1);
    }

    #[test]
    fn the_real_clock_uses_the_same_shape_as_the_log_lines() {
        let mut stamp = String::new();
        timer().format_time(&mut Writer::new(&mut stamp)).unwrap();
        // 2026-10-05T18:44:18.740169Z: UTC, microseconds, a Z suffix.
        let b = stamp.as_bytes();
        assert_eq!(stamp.len(), 27, "{stamp}");
        assert!(b[4] == b'-' && b[10] == b'T' && b[19] == b'.' && stamp.ends_with('Z'));
    }
}
