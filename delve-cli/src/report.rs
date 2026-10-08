//! How log lines and the fatal error are stamped and written.
//!
//! `main` used to return the error and let the runtime print a bare
//! `Error: ...`, the one line in a cron log with no time on it. It is written
//! here instead, behind the same clock as every other log line, and with the
//! message exactly as the runtime printed it.

use std::io::Write;

use chrono::{DateTime, Local, SecondsFormat, TimeZone, Utc};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};

/// The clock for every log line, fatal errors included. The log subscriber
/// and [`write_fatal`] both take it from here, so the two cannot drift apart.
///
/// Local time is the default, as for the timestamps commands print; `--utc`
/// selects UTC. Both keep microseconds, and differ only in the offset
/// (`2026-10-05T14:44:18.740169-04:00` against `2026-10-05T18:44:18.740169Z`).
#[derive(Debug, Clone, Copy)]
pub(crate) enum Clock {
    Local,
    Utc,
}

impl Clock {
    pub(crate) fn new(utc: bool) -> Self {
        if utc {
            Self::Utc
        } else {
            Self::Local
        }
    }
}

impl FormatTime for Clock {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        match self {
            // tracing's own UTC clock, unchanged from before `--utc` existed.
            Self::Utc => SystemTime.format_time(w),
            Self::Local => write!(w, "{}", format_in(Utc::now(), &Local)),
        }
    }
}

/// `t` in `tz`, to microseconds, with the numeric offset.
fn format_in<Tz: TimeZone>(t: DateTime<Utc>, tz: &Tz) -> String {
    t.with_timezone(tz)
        .to_rfc3339_opts(SecondsFormat::Micros, false)
}

/// `<timestamp> Error: <message>`, with anyhow's `Caused by:` lines after it as
/// before. Only the first line is stamped: the rest belong to the same record.
fn fatal_text(clock: &impl FormatTime, err: &anyhow::Error) -> String {
    let mut stamp = String::new();
    // Formatting into a `String` cannot fail; a clock that does leaves no stamp.
    let _ = clock.format_time(&mut Writer::new(&mut stamp));
    format!("{stamp} Error: {err:?}")
}

/// Writes the error that ends the program to stderr. A closed stderr is
/// ignored: there is nowhere left to report it.
pub(crate) fn write_fatal(clock: &impl FormatTime, err: &anyhow::Error) {
    let _ = writeln!(std::io::stderr(), "{}", fatal_text(clock, err));
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

    fn stamp(clock: Clock) -> String {
        let mut stamp = String::new();
        clock.format_time(&mut Writer::new(&mut stamp)).unwrap();
        stamp
    }

    #[test]
    fn utc_is_the_log_clock_it_always_was() {
        // 2026-10-05T18:44:18.740169Z: UTC, microseconds, a Z suffix.
        let s = stamp(Clock::Utc);
        let b = s.as_bytes();
        assert_eq!(s.len(), 27, "{s}");
        assert!(b[4] == b'-' && b[10] == b'T' && b[19] == b'.' && s.ends_with('Z'));
    }

    #[test]
    fn local_has_microseconds_and_a_numeric_offset() {
        // 2026-10-05T14:44:18.740169-04:00, whatever zone the machine is in.
        let s = stamp(Clock::Local);
        let b = s.as_bytes();
        assert_eq!(s.len(), 32, "{s}");
        assert!(
            b[10] == b'T' && b[19] == b'.' && (b[26] == b'+' || b[26] == b'-'),
            "{s}"
        );
        assert_eq!(b[29], b':', "{s}");
        let t = chrono::DateTime::parse_from_rfc3339(&s).unwrap();
        let drift = (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds();
        assert!(drift.abs() < 5, "{s}");
    }

    #[test]
    fn the_flag_picks_the_clock() {
        assert!(matches!(Clock::new(true), Clock::Utc));
        assert!(matches!(Clock::new(false), Clock::Local));
    }

    #[test]
    fn a_zone_shifts_the_time_and_keeps_the_microseconds() {
        let t = chrono::DateTime::parse_from_rfc3339("2026-10-05T18:44:18.740169589Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            format_in(t, &chrono_tz::America::New_York),
            "2026-10-05T14:44:18.740169-04:00"
        );
        assert_eq!(
            format_in(t, &chrono_tz::Asia::Kolkata),
            "2026-10-06T00:14:18.740169+05:30"
        );
        assert_eq!(
            format_in(t, &chrono_tz::UTC),
            "2026-10-05T18:44:18.740169+00:00"
        );
    }

    #[test]
    fn the_offset_follows_daylight_saving_between_lines() {
        // New York leaves daylight saving on 2026-11-01.
        let at = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap()
                .with_timezone(&Utc)
        };
        let ny = &chrono_tz::America::New_York;
        assert!(format_in(at("2026-10-31T12:00:00Z"), ny).ends_with("-04:00"));
        assert!(format_in(at("2026-11-02T12:00:00Z"), ny).ends_with("-05:00"));
    }
}
