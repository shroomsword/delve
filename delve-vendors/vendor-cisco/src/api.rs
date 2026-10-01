//! Cisco Software Suggestion API: request URLs, response shapes, and
//! mapping into `delve_core`'s `FirmwareRef`/`FirmwareMetadata`.
//!
//! **⚠️ UNVERIFIED AGAINST A LIVE RESPONSE.** Everything in this module —
//! the endpoint paths, the JSON field names in [`SuggestionListResponse`]/
//! [`SoftwareSuggestion`], the assumption that a per-release "detail"
//! endpoint exists and returns a single unwrapped suggestion — was written
//! without network access to Cisco's API documentation or a live endpoint
//! to test against. It's a structurally reasonable first draft based on
//! how REST APIs of this shape typically look (and general familiarity
//! with Cisco's Support APIs existing and using this rough pattern), not a
//! confirmed integration. Before trusting this:
//!
//! 1. Get real API Console access at developer.cisco.com and pull one
//!    actual response for a known product ID.
//! 2. Compare it against [`SuggestionListResponse`] field-by-field — the
//!    `#[serde(rename_all = "camelCase")]` convention assumed here is a
//!    guess, not a confirmed fact about Cisco's API.
//! 3. Confirm whether a per-ID "detail" endpoint
//!    ([`suggestion_detail_url`]) actually exists and returns an unwrapped
//!    [`SoftwareSuggestion`] — `lib.rs`'s `VendorPlugin::metadata` impl
//!    assumes it does, to avoid re-fetching the entire per-device-family
//!    list on every single release. If it doesn't exist, that method needs
//!    to change to work off the list response instead.
//! 4. Confirm what checksum type (if any) the API actually returns —
//!    Cisco's download pages have historically shown MD5 and/or SHA512,
//!    not SHA256, which is why `sha256` is left `None` below rather than
//!    populated from a field that probably isn't actually a SHA-256 hash.
//!
//! The tests in this module use hand-written JSON fixtures that match
//! *this module's assumed schema* — a passing test proves the
//! deserialization and mapping logic is internally consistent, not that it
//! matches Cisco's real API.

use chrono::NaiveDate;
use delve_core::model::{FirmwareMetadata, FirmwareRef, VersionKey};
use delve_core::plugin::PluginError;
use serde::Deserialize;

use crate::version::parse_ios_xe_version;

/// A device family this plugin knows how to query, and the Cisco base
/// Product ID (PID) the Suggestion API expects for it. Hardcoded to one
/// entry for now — a real implementation would likely load this from
/// config rather than a compiled-in list, since Cisco has thousands of
/// product families and there's no reason every `delve` deployment needs
/// all of them compiled in.
pub struct CiscoProduct {
    pub device_family: &'static str,
    pub base_pid: &'static str,
}

pub const KNOWN_PRODUCTS: &[CiscoProduct] = &[CiscoProduct {
    device_family: "isr4000",
    base_pid: "ISR4000",
}];

// VERIFY: endpoint path and query/path-param name for listing suggestions
// by base PID. This is the single most likely thing here to be wrong or
// outdated — Cisco API paths change.
pub fn suggestions_list_url(base_pid: &str) -> String {
    format!(
        "https://apix.cisco.com/software/suggestion/v2/suggestions/software/productIds/{base_pid}"
    )
}

// VERIFY: whether this endpoint exists at all — see this module's top doc
// comment, point 3.
pub fn suggestion_detail_url(id: &str) -> String {
    format!("https://apix.cisco.com/software/suggestion/v2/suggestions/software/id/{id}")
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SuggestionListResponse {
    #[serde(default)]
    pub product_list: Vec<ProductEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProductEntry {
    #[serde(default)]
    pub suggestions: Vec<SoftwareSuggestion>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SoftwareSuggestion {
    pub id: String,
    /// The version string as Cisco publishes it, e.g. `"17.9.4a"`.
    #[serde(default)]
    pub release_format: Option<String>,
    /// VERIFY: assumed `"YYYY-MM-DD"` — not confirmed against a real
    /// response. See `parse_release_date` below.
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub release_notes_url: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum MapError {
    #[error("suggestion is missing a usable version string")]
    MissingVersion,
    #[error("invalid release_notes_url: {0}")]
    InvalidReleaseNotesUrl(String),
    #[error("invalid detail URL constructed for suggestion id '{0}'")]
    InvalidSourceUrl(String),
}

/// Turns a raw HTTP response body from the list endpoint into
/// `FirmwareRef`s, one per suggestion. `source_url` is set to the assumed
/// per-release detail endpoint (see this module's top doc comment) so
/// `metadata()` can fetch exactly one release's data rather than
/// re-parsing the whole list.
pub fn parse_suggestion_list(
    body: &str,
    vendor: &str,
    device_family: &str,
) -> Result<Vec<FirmwareRef>, PluginError> {
    let parsed: SuggestionListResponse = serde_json::from_str(body)
        .map_err(|e| PluginError::Parse(format!("Cisco suggestion list: {e}")))?;

    let mut refs = Vec::new();
    for product in parsed.product_list {
        for suggestion in product.suggestions {
            let source_url = suggestion_detail_url(&suggestion.id).parse().map_err(|e| {
                PluginError::Parse(format!(
                    "invalid detail URL for suggestion {}: {e}",
                    suggestion.id
                ))
            })?;
            refs.push(FirmwareRef {
                vendor: vendor.to_string(),
                device_family: device_family.to_string(),
                source_url,
                discovered_at: chrono::Utc::now(),
            });
        }
    }
    Ok(refs)
}

/// Turns a raw HTTP response body from the (assumed) per-ID detail
/// endpoint into `FirmwareMetadata`. `vendor`/`device_family` are filled
/// with placeholder values here since the engine overwrites both
/// immediately after `metadata()` returns anyway (see the README's "Data
/// model and identity keys" section) — this function only needs to get the
/// fields that actually come from Cisco right.
pub fn parse_suggestion_detail(
    body: &str,
    hardware_target: &str,
) -> Result<FirmwareMetadata, PluginError> {
    let suggestion: SoftwareSuggestion = serde_json::from_str(body)
        .map_err(|e| PluginError::Parse(format!("Cisco suggestion detail: {e}")))?;
    suggestion_to_metadata(&suggestion, hardware_target)
        .map_err(|e| PluginError::Parse(e.to_string()))
}

fn suggestion_to_metadata(
    suggestion: &SoftwareSuggestion,
    hardware_target: &str,
) -> Result<FirmwareMetadata, MapError> {
    let raw_version = suggestion
        .release_format
        .clone()
        .ok_or(MapError::MissingVersion)?;
    let version = match parse_ios_xe_version(&raw_version) {
        Some(ordinal) => VersionKey {
            raw: raw_version,
            scheme: delve_core::model::VersionScheme::VendorNumeric,
            ordinal: Some(ordinal),
        },
        // Classic IOS train notation, or anything else this parser
        // doesn't understand — Opaque, not a guessed ordering. See
        // version.rs's module doc comment.
        None => VersionKey::opaque(raw_version),
    };

    let release_notes_url = match &suggestion.release_notes_url {
        Some(url) => Some(
            url.parse()
                .map_err(|_| MapError::InvalidReleaseNotesUrl(url.clone()))?,
        ),
        None => None,
    };

    Ok(FirmwareMetadata {
        // Placeholder — overwritten by the engine from the originating
        // FirmwareRef immediately after metadata() returns.
        vendor: "cisco".to_string(),
        device_family: String::new(),
        source_url: suggestion_detail_url(&suggestion.id)
            .parse()
            .map_err(|_| MapError::InvalidSourceUrl(suggestion.id.clone()))?,
        version,
        release_date: parse_release_date(suggestion.release_date.as_deref()),
        // Left unpopulated — see this module's top doc comment, point 4:
        // Cisco's actual checksum field/type here is unconfirmed, and a
        // wrong-but-present value would be worse than an honest None, since
        // `unearth`'s default verification would then fail real downloads.
        sha256: None,
        signature: None,
        hardware_targets: vec![hardware_target.to_string()],
        release_notes_url,
    })
}

/// VERIFY: date format. Tries ISO 8601 (`YYYY-MM-DD`) first since it's the
/// most common for REST APIs; returns `None` (rather than guessing at
/// other formats) if that doesn't parse, since a silently wrong date is
/// worse than a missing one for something that only affects display/
/// chronological sorting, not identity or change detection.
fn parse_release_date(raw: Option<&str>) -> Option<NaiveDate> {
    raw.and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hand-written to match this module's ASSUMED schema — see the module
    // doc comment. Passing here means the parsing/mapping code is
    // internally consistent, not that it matches a real Cisco response.
    const LIST_FIXTURE: &str = r#"
    {
        "productList": [
            {
                "suggestions": [
                    { "id": "abc123", "releaseFormat": "17.9.4a", "releaseDate": "2023-05-12" },
                    { "id": "def456", "releaseFormat": "17.9.4", "releaseDate": "2023-02-01" }
                ]
            }
        ]
    }
    "#;

    const DETAIL_FIXTURE: &str = r#"
    {
        "id": "abc123",
        "releaseFormat": "17.9.4a",
        "releaseDate": "2023-05-12",
        "releaseNotesUrl": "https://example.test/release-notes/17.9.4a"
    }
    "#;

    #[test]
    fn parses_the_list_fixture_into_one_ref_per_suggestion() {
        let refs = parse_suggestion_list(LIST_FIXTURE, "cisco", "isr4000").unwrap();
        assert_eq!(refs.len(), 2);
        assert!(refs
            .iter()
            .all(|r| r.vendor == "cisco" && r.device_family == "isr4000"));
    }

    #[test]
    fn list_refs_point_at_the_per_id_detail_url() {
        let refs = parse_suggestion_list(LIST_FIXTURE, "cisco", "isr4000").unwrap();
        let urls: Vec<String> = refs.iter().map(|r| r.source_url.to_string()).collect();
        assert!(urls.iter().any(|u| u.contains("abc123")));
        assert!(urls.iter().any(|u| u.contains("def456")));
    }

    #[test]
    fn empty_product_list_yields_no_refs_not_an_error() {
        let refs = parse_suggestion_list(r#"{"productList": []}"#, "cisco", "isr4000").unwrap();
        assert!(refs.is_empty());
    }

    #[test]
    fn malformed_json_is_a_parse_error_not_a_panic() {
        let result = parse_suggestion_list("not json at all", "cisco", "isr4000");
        assert!(matches!(result, Err(PluginError::Parse(_))));
    }

    #[test]
    fn parses_the_detail_fixture_into_metadata_with_a_vendor_numeric_version() {
        let meta = parse_suggestion_detail(DETAIL_FIXTURE, "ISR4000").unwrap();
        assert_eq!(meta.version.raw, "17.9.4a");
        assert!(matches!(
            meta.version.scheme,
            delve_core::model::VersionScheme::VendorNumeric
        ));
        assert_eq!(meta.version.ordinal, Some(vec![17, 9, 4, 1]));
    }

    #[test]
    fn detail_release_date_parses_as_iso8601() {
        let meta = parse_suggestion_detail(DETAIL_FIXTURE, "ISR4000").unwrap();
        assert_eq!(
            meta.release_date,
            Some(NaiveDate::from_ymd_opt(2023, 5, 12).unwrap())
        );
    }

    #[test]
    fn detail_sha256_is_always_none_since_cisco_checksum_type_is_unconfirmed() {
        let meta = parse_suggestion_detail(DETAIL_FIXTURE, "ISR4000").unwrap();
        assert_eq!(meta.sha256, None);
    }

    #[test]
    fn detail_hardware_target_comes_from_the_caller_not_the_response() {
        let meta = parse_suggestion_detail(DETAIL_FIXTURE, "ISR4000").unwrap();
        assert_eq!(meta.hardware_targets, vec!["ISR4000".to_string()]);
    }

    #[test]
    fn a_classic_ios_version_string_falls_back_to_opaque_not_a_guessed_ordinal() {
        let suggestion = SoftwareSuggestion {
            id: "xyz".to_string(),
            release_format: Some("15.2(7)E3".to_string()),
            release_date: None,
            release_notes_url: None,
        };
        let meta = suggestion_to_metadata(&suggestion, "ISR4000").unwrap();
        assert!(matches!(
            meta.version.scheme,
            delve_core::model::VersionScheme::Opaque
        ));
        assert_eq!(meta.version.ordinal, None);
    }

    #[test]
    fn a_missing_release_format_is_a_mapping_error_not_a_fabricated_version() {
        let suggestion = SoftwareSuggestion {
            id: "xyz".to_string(),
            release_format: None,
            release_date: None,
            release_notes_url: None,
        };
        let result = suggestion_to_metadata(&suggestion, "ISR4000");
        assert!(matches!(result, Err(MapError::MissingVersion)));
    }

    #[test]
    fn an_unparseable_release_date_is_none_not_an_error() {
        let suggestion = SoftwareSuggestion {
            id: "xyz".to_string(),
            release_format: Some("17.9.4a".to_string()),
            release_date: Some("not-a-date".to_string()),
            release_notes_url: None,
        };
        let meta = suggestion_to_metadata(&suggestion, "ISR4000").unwrap();
        assert_eq!(
            meta.release_date, None,
            "an unparseable date should degrade to None, not fail the whole mapping"
        );
    }
}
