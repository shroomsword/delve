//! Derives a comparable ordinal from a UniFi firmware record's version.
//!
//! UniFi versions look like `v6.6.77+15402`: a semver-style
//! `major.minor.patch` plus a build number after the `+`. The API also
//! returns the parts as separate fields (`version_major`, `version_minor`,
//! `version_patch`, `version_build`, `version_prerelease`), which agree
//! with the string on every record checked, so the ordinal is built from
//! those rather than by re-parsing the string.
//!
//! Three shapes appear in the release channel:
//!
//! - **Numeric build** (almost everything): `v6.6.77+15402` →
//!   `[6, 6, 77, 15402]`. The build number increases across releases, so
//!   it's a meaningful last component.
//! - **Git-hash build** (Cloud Keys: `UCK`, `UCKG2`, `UCKP`):
//!   `v2.1.11+a7986ca` → `[2, 1, 11]`. A commit hash has no order, so it's
//!   left out. No model mixes numeric and hash builds, and no two releases
//!   of one model share a `major.minor.patch`, so the shorter ordinal never
//!   has to be compared against a longer one for the same model.
//! - **Pre-release** (two UXG-Pro records): `v1.11.0-23+3923` → no ordinal
//!   (`Opaque`). The same model also has `v1.11.0+3923`, with the same build
//!   number, so a numeric ordinal would claim an order the data doesn't
//!   support. Equality-based change detection still works.

use crate::api::FirmwareRecord;

pub fn parse_unifi_version(record: &FirmwareRecord) -> Option<Vec<u64>> {
    if record.version_prerelease.is_some() {
        return None;
    }

    let mut ordinal = vec![
        record.version_major,
        record.version_minor,
        record.version_patch,
    ];
    if let Some(build) = record.version_build.as_deref() {
        if !build.is_empty() && build.bytes().all(|b| b.is_ascii_digit()) {
            ordinal.push(build.parse().ok()?);
        }
    }
    Some(ordinal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(version: &str, build: Option<&str>, prerelease: Option<&str>) -> FirmwareRecord {
        let parts: Vec<u64> = version
            .trim_start_matches('v')
            .split(['+', '-'])
            .next()
            .unwrap()
            .split('.')
            .map(|p| p.parse().unwrap())
            .collect();
        serde_json::from_value(serde_json::json!({
            "id": "test",
            "product": "unifi-firmware",
            "channel": "release",
            "platform": "TEST",
            "version": version,
            "version_major": parts[0],
            "version_minor": parts[1],
            "version_patch": parts[2],
            "version_build": build,
            "version_prerelease": prerelease,
            "created": "2024-01-01T00:00:00Z",
            "_links": {"self": {"href": "https://fw-update.ui.com/api/firmware/test"}}
        }))
        .unwrap()
    }

    #[test]
    fn numeric_build_is_the_last_ordinal_component() {
        let r = record("v6.6.77+15402", Some("15402"), None);
        assert_eq!(parse_unifi_version(&r), Some(vec![6, 6, 77, 15402]));
    }

    #[test]
    fn git_hash_build_is_left_out() {
        let r = record("v2.1.11+a7986ca", Some("a7986ca"), None);
        assert_eq!(parse_unifi_version(&r), Some(vec![2, 1, 11]));
    }

    #[test]
    fn missing_build_uses_major_minor_patch() {
        let r = record("v0.0.1", None, None);
        assert_eq!(parse_unifi_version(&r), Some(vec![0, 0, 1]));
    }

    #[test]
    fn prerelease_is_opaque() {
        let r = record("v1.11.0-23+3923", Some("3923"), Some("23"));
        assert_eq!(parse_unifi_version(&r), None);
    }

    #[test]
    fn compares_numerically_not_lexicographically() {
        let older = parse_unifi_version(&record("v6.6.9+15000", Some("15000"), None)).unwrap();
        let newer = parse_unifi_version(&record("v6.6.10+15001", Some("15001"), None)).unwrap();
        assert!(older < newer);
        let older = parse_unifi_version(&record("v6.9.0+1", Some("1"), None)).unwrap();
        let newer = parse_unifi_version(&record("v6.10.0+2", Some("2"), None)).unwrap();
        assert!(older < newer);
    }

    #[test]
    fn a_build_number_too_large_for_u64_is_opaque_not_a_panic() {
        let r = record(
            "v1.0.0+99999999999999999999",
            Some("99999999999999999999"),
            None,
        );
        assert_eq!(parse_unifi_version(&r), None);
    }
}
