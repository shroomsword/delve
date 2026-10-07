//! How commands show a point in time to a person.
//!
//! Stored values and machine-readable output (the webhook payload) stay UTC;
//! this is only for what a command prints for someone to read.

use chrono::{DateTime, Local, TimeZone, Utc};

/// The timezone a command shows timestamps in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Display {
    /// The machine's timezone, with the numeric offset
    /// (`2026-10-05T14:44:18.740169589-04:00`), so each value still names one
    /// instant.
    Local,
    /// UTC (`2026-10-05T18:44:18.740169589+00:00`), which is what scripts that
    /// parse the output should pin with `--utc`.
    Utc,
}

impl Display {
    pub(crate) fn from_utc_flag(utc: bool) -> Self {
        if utc {
            Self::Utc
        } else {
            Self::Local
        }
    }

    pub(crate) fn format(self, t: DateTime<Utc>) -> String {
        match self {
            Self::Local => format_in(t, &Local),
            Self::Utc => t.to_rfc3339(),
        }
    }
}

/// `t` in `tz`, with the numeric offset. Fractional seconds are written the
/// same way in every zone (none when zero, else 3, 6 or 9 digits), so a local
/// value differs from its UTC form only in the offset.
fn format_in<Tz: TimeZone>(t: DateTime<Utc>, tz: &Tz) -> String {
    t.with_timezone(tz).to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn utc_keeps_the_precision_the_command_always_printed() {
        let t = at("2026-10-05T18:44:18.740169589Z");
        assert_eq!(
            Display::Utc.format(t),
            "2026-10-05T18:44:18.740169589+00:00"
        );
    }

    #[test]
    fn a_negative_offset_shifts_the_time_and_names_the_offset() {
        let tz = FixedOffset::west_opt(4 * 3600).unwrap();
        let t = at("2026-10-05T18:44:18.740169589Z");
        assert_eq!(format_in(t, &tz), "2026-10-05T14:44:18.740169589-04:00");
    }

    #[test]
    fn a_positive_offset_can_move_the_date_forward() {
        let tz = FixedOffset::east_opt(5 * 3600 + 30 * 60).unwrap();
        let t = at("2026-10-05T22:00:00Z");
        assert_eq!(format_in(t, &tz), "2026-10-06T03:30:00+05:30");
    }

    #[test]
    fn the_flag_picks_utc_and_local_is_the_default() {
        assert_eq!(Display::from_utc_flag(true), Display::Utc);
        assert_eq!(Display::from_utc_flag(false), Display::Local);
    }

    #[test]
    fn local_follows_a_real_zone_including_half_hour_offsets() {
        let t = at("2026-10-05T22:00:00.5Z");
        assert_eq!(
            format_in(t, &chrono_tz::Asia::Kolkata),
            "2026-10-06T03:30:00.500+05:30"
        );
        assert_eq!(
            format_in(t, &chrono_tz::UTC),
            "2026-10-05T22:00:00.500+00:00"
        );
    }

    #[test]
    fn the_offset_follows_daylight_saving_between_rows() {
        // New York leaves daylight saving on 2026-11-01.
        let zone = &chrono_tz::America::New_York;
        assert_eq!(
            format_in(at("2026-10-31T12:00:00Z"), zone),
            "2026-10-31T08:00:00-04:00"
        );
        assert_eq!(
            format_in(at("2026-11-02T12:00:00Z"), zone),
            "2026-11-02T07:00:00-05:00"
        );
    }
}
