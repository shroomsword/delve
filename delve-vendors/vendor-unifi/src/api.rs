//! Ubiquiti firmware update API: request URLs, response shapes, and
//! mapping into `delve_core`'s `FirmwareRef`/`FirmwareMetadata`.
//!
//! Unlike `vendor-cisco`'s `api.rs`, everything here was **checked against
//! live responses** from `https://fw-update.ui.com/api/firmware` (September
//! 2026), and the fixtures under `fixtures/` are real records copied from
//! those responses, not hand-written guesses. The API is still
//! **undocumented** — it's the endpoint UniFi devices and controllers poll
//! for updates, not a published developer API — so Ubiquiti can change it
//! without notice. What was confirmed:
//!
//! - No authentication is required.
//! - `filter=eq~~<field>~~<value>` query parameters filter server-side and
//!   can be repeated (`product`, `channel`, and `platform` all work).
//! - The default page size is 25; `limit` raises it, and one request with
//!   a large `limit` returned all ~3,400 `unifi-firmware` release records.
//!   **`offset` is ignored** — every offset returns the same first page — so
//!   there is no real pagination. [`LIST_LIMIT`] is set far above the
//!   current record count and a response that fills it is treated as
//!   possibly truncated rather than silently accepted.
//! - `sha256_checksum` is the SHA-256 of the file at `_links.data.href`
//!   (verified by downloading one image and hashing it).
//! - `platform` is the device model code (`U7PG2` = UAP-AC-Pro, `USMINI` =
//!   USW-Flex-Mini, ...). There is no human-readable model name or product
//!   line in the response.
//! - `release_date` is present on only a handful of records; `created`
//!   (upload time) is present on all of them.
//! - None of the `unifi-firmware` records has a `changelog` link, although
//!   other Ubiquiti products in the same API do.
//!
//! The console products (`unifi-dream`, `unifi-nvr`, `unifi-drive`,
//! `unifi-cloudkey`) were checked the same way in October 2026, with the same
//! record shape, a `sha256_checksum` and a download link on every record, and
//! no duplicate `(platform, version)` within a product. What differs:
//!
//! - Versions are `major.minor.patch+<git hash>`, where `unifi-firmware`
//!   has numeric builds. The ordinal stops at the patch, as for the Cloud
//!   Keys (see `version.rs`).
//! - The Cloud Keys and the Express are listed under both `unifi-firmware`
//!   and a console product, as the same file (21 pairs, identical SHA-256).
//! - `unifi-drive` also lists `uos-*` application packages.
//! - Records carry a rollout `probability`. It doesn't hide them, except for
//!   two `unifi-cloudkey` records at 0.5 that were in only some responses.
//! - `uvc` (Protect cameras) was looked at and left out: `platform` is the
//!   camera's chip (`cv22`), not a model, and no record has a build number.

use chrono::{DateTime, NaiveDate, Utc};
use delve_core::model::{FirmwareMetadata, FirmwareRef, VersionKey, VersionScheme};
use delve_core::plugin::PluginError;
use serde::Deserialize;
use url::Url;

use crate::product_line::{device_family, product_name};
use crate::version::parse_unifi_version;

pub const API_BASE: &str = "https://fw-update.ui.com/api/firmware";

/// What is tracked when `products` isn't set: firmware for UniFi network
/// devices (access points, switches, gateways, older Cloud Keys). Adding a
/// product to the default would announce hundreds of "new" devices to every
/// existing user on their next dig, so the others are opt-in.
pub const DEFAULT_PRODUCTS: &[&str] = &["unifi-firmware"];

/// The products `products` may name: network device firmware, and the four
/// UniFi OS console products, each checked against live responses (October
/// 2026). The API lists about 300 more — Protect cameras (`uvc`, whose
/// "platform" is the camera's chip, not a model), Access and Talk devices,
/// airMAX, EdgeRouter, and application packages such as `unifi-controller` —
/// which need their own check before they are added; see the README.
///
/// - `unifi-dream`: Dream Machines, Dream Routers, Cloud Gateways, Express.
/// - `unifi-nvr`: Network Video Recorders.
/// - `unifi-drive`: UNAS network storage (and some `uos-*` packages, which
///   [`select_records`] drops).
/// - `unifi-cloudkey`: Cloud Key and Cloud Key Gen2 (+, Enterprise).
pub const SUPPORTED_PRODUCTS: &[&str] = &[
    "unifi-firmware",
    "unifi-dream",
    "unifi-nvr",
    "unifi-drive",
    "unifi-cloudkey",
];

/// Only the stable release channel is tracked by default. `beta-public`
/// exists too; see the README's vendor-unifi section for the note on
/// revisiting beta tracking.
pub const CHANNEL: &str = "release";

/// Far above the current ~3,400 records. The API has no working
/// pagination (see this module's doc comment), so a response containing
/// exactly this many records is treated as possibly truncated.
pub const LIST_LIMIT: usize = 100_000;

/// The list request for the release channel, narrowed to `product` and/or
/// `model` (the API's `platform`, e.g. `U7PG2`) when given. Filters are
/// ANDed, so one request can name only one product.
pub fn list_url(base: &Url, product: Option<&str>, model: Option<&str>) -> Url {
    let mut url = base.clone();
    {
        let mut query = url.query_pairs_mut();
        if let Some(product) = product {
            query.append_pair("filter", &format!("eq~~product~~{product}"));
        }
        query.append_pair("filter", &format!("eq~~channel~~{CHANNEL}"));
        if let Some(model) = model {
            query.append_pair("filter", &format!("eq~~platform~~{model}"));
        }
        query.append_pair("limit", &LIST_LIMIT.to_string());
    }
    url
}

pub fn api_base() -> Url {
    Url::parse(API_BASE).expect("API_BASE is a valid URL")
}

/// One firmware record as the API returns it, both in the list response
/// and from a record's own `_links.self` URL.
#[derive(Debug, Clone, Deserialize)]
pub struct FirmwareRecord {
    pub id: String,
    pub product: String,
    pub channel: String,
    pub platform: String,
    pub version: String,
    pub version_major: u64,
    pub version_minor: u64,
    pub version_patch: u64,
    #[serde(default)]
    pub version_build: Option<String>,
    #[serde(default)]
    pub version_prerelease: Option<String>,
    pub created: DateTime<Utc>,
    #[serde(default)]
    pub release_date: Option<DateTime<Utc>>,
    #[serde(default)]
    pub sha256_checksum: Option<String>,
    #[serde(default)]
    pub file_size: Option<u64>,
    #[serde(rename = "_links")]
    pub links: Links,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Links {
    #[serde(rename = "self")]
    pub self_link: Link,
    #[serde(default)]
    pub data: Option<Link>,
    #[serde(default)]
    pub changelog: Option<Link>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Link {
    pub href: Url,
}

#[derive(Debug, Deserialize)]
struct ListResponse {
    #[serde(rename = "_embedded", default)]
    embedded: Embedded,
}

#[derive(Debug, Default, Deserialize)]
struct Embedded {
    #[serde(default)]
    firmware: Vec<serde_json::Value>,
}

/// Result of parsing a list response. Records are parsed one at a time so
/// that a single record with an unexpected shape is skipped (and reported
/// in `skipped`) instead of failing the whole dig.
#[derive(Debug)]
pub struct ParsedList {
    pub records: Vec<FirmwareRecord>,
    pub skipped: Vec<String>,
    pub total: usize,
}

pub fn parse_list(body: &str) -> Result<ParsedList, PluginError> {
    let parsed: ListResponse = serde_json::from_str(body)
        .map_err(|e| PluginError::Parse(format!("UniFi firmware list: {e}")))?;

    let total = parsed.embedded.firmware.len();
    let mut records = Vec::with_capacity(total);
    let mut skipped = Vec::new();
    for value in parsed.embedded.firmware {
        let id = value
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("<no id>")
            .to_string();
        match serde_json::from_value::<FirmwareRecord>(value) {
            Ok(record) => records.push(record),
            Err(e) => skipped.push(format!("{id}: {e}")),
        }
    }

    // Every record failing to parse means the response shape has changed,
    // not that a few records are odd — fail loudly instead of reporting an
    // empty vendor.
    if total > 0 && records.is_empty() {
        return Err(PluginError::Parse(format!(
            "none of the {total} UniFi firmware records matched the expected shape; first error: {}",
            skipped.first().map(String::as_str).unwrap_or("")
        )));
    }

    Ok(ParsedList {
        records,
        skipped,
        total,
    })
}

pub fn parse_detail(body: &str) -> Result<FirmwareRecord, PluginError> {
    serde_json::from_str(body)
        .map_err(|e| PluginError::Parse(format!("UniFi firmware record: {e}")))
}

/// Narrows parsed records to the ones this plugin tracks, and makes each
/// identity key unique.
///
/// - Re-checks `product` and `channel` even though the request already
///   filters on them, so a server that ignored the filter can't flood the
///   store with other products or beta builds.
/// - Drops records with no downloadable file (one placeholder record,
///   `platform: "stat"`, has neither a file nor a hash).
/// - Drops `uos-*` platforms. `unifi-drive` lists the UniFi OS application
///   packages for Debian under those names (`uos-deb11-arm64`); they are
///   software for a console that has its own record, not a device.
/// - Keeps one record per `(platform, version)`. The store's identity key is
///   `(vendor, device_family, hardware_targets, version)`, and two records
///   sharing it would overwrite each other on every dig, flip-flopping
///   between them and firing a spurious "rebuilt" event each time. Within
///   one product the newest record wins. Across products the product named
///   first in `products` wins: the Cloud Keys and the Express are listed
///   under both `unifi-firmware` and a console product, as the same file
///   (identical SHA-256) — 21 such pairs in October 2026 — and picking by
///   list order keeps the record, and so the `source_url`, that an existing
///   `unifi-firmware` database already holds.
pub fn select_records<S: AsRef<str>>(
    records: Vec<FirmwareRecord>,
    products: &[S],
) -> Vec<FirmwareRecord> {
    let rank = |product: &str| products.iter().position(|p| p.as_ref() == product);

    let mut selected: Vec<FirmwareRecord> = Vec::with_capacity(records.len());
    let mut index: std::collections::HashMap<(String, String), usize> =
        std::collections::HashMap::new();

    for record in records {
        if rank(&record.product).is_none()
            || record.channel != CHANNEL
            || record.links.data.is_none()
            || record.platform.starts_with("uos-")
        {
            continue;
        }
        let key = (record.platform.clone(), record.version.clone());
        match index.get(&key) {
            Some(&i) => {
                let kept = &selected[i];
                let wins = match rank(&record.product).cmp(&rank(&kept.product)) {
                    std::cmp::Ordering::Less => true,
                    std::cmp::Ordering::Greater => false,
                    std::cmp::Ordering::Equal => record.created > kept.created,
                };
                if kept.product == record.product {
                    tracing::warn!(
                        platform = %record.platform,
                        version = %record.version,
                        "UniFi API returned more than one record for the same model and version; keeping the newest"
                    );
                }
                if wins {
                    selected[i] = record;
                }
            }
            None => {
                index.insert(key, selected.len());
                selected.push(record);
            }
        }
    }
    selected
}

/// The file name in a download URL, such as
/// `259f-U7PG2-6.8.2-5464c424-a775-4715-8bbc-d84602f55445.bin`, if the URL
/// has one: the last path segment, when it has an extension and only letters,
/// digits and `. _ - +`. Anything else (no segment, no extension, percent
/// escapes, other characters) is not trusted to be a file name, and the
/// caller names the file itself.
pub fn download_file_name(url: &Url) -> Option<String> {
    let name = url.path_segments()?.next_back()?;
    let plain = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'));
    let has_extension = name
        .rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && !ext.is_empty());
    (plain && has_extension).then(|| name.to_string())
}

/// `device_family` is the model's product line (`USW`, `UAP`, `U7`, ...)
/// when it has one, and its model code otherwise; `hardware_targets` is
/// always the model code. See `product_line.rs`.
pub fn record_to_ref(record: &FirmwareRecord, vendor: &str) -> FirmwareRef {
    FirmwareRef {
        vendor: vendor.to_string(),
        device_family: device_family(&record.platform).to_string(),
        source_url: record.links.self_link.href.clone(),
        discovered_at: Utc::now(),
    }
}

pub fn record_to_metadata(record: &FirmwareRecord, vendor: &str) -> FirmwareMetadata {
    let version = match parse_unifi_version(record) {
        Some(ordinal) => VersionKey {
            raw: record.version.clone(),
            scheme: VersionScheme::VendorNumeric,
            ordinal: Some(ordinal),
        },
        None => VersionKey::opaque(record.version.clone()),
    };

    FirmwareMetadata {
        // vendor/device_family/source_url are overwritten by the engine
        // from the originating FirmwareRef; set to matching values anyway
        // so this is correct when called outside the engine too.
        vendor: vendor.to_string(),
        device_family: device_family(&record.platform).to_string(),
        source_url: record.links.self_link.href.clone(),
        version,
        release_date: Some(release_date(record)),
        sha256: record.sha256_checksum.as_deref().and_then(decode_sha256),
        signature: None,
        hardware_targets: vec![record.platform.clone()],
        release_notes_url: record.links.changelog.as_ref().map(|l| l.href.clone()),
        display_name: product_name(&record.platform).map(String::from),
    }
}

/// The explicit `release_date` when the API has one, otherwise the upload
/// time. For the release channel the two are close (3-10 days apart on
/// the records that have both), but `created` is when the file was
/// uploaded, not necessarily when it was published.
fn release_date(record: &FirmwareRecord) -> NaiveDate {
    record.release_date.unwrap_or(record.created).date_naive()
}

/// Decodes a 64-character hex SHA-256. Anything else becomes `None`: a
/// wrong hash would make `unearth`'s default verification fail real
/// downloads, which is worse than having no hash to check.
pub fn decode_sha256(hex: &str) -> Option<[u8; 32]> {
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in bytes.chunks(2).enumerate() {
        let s = std::str::from_utf8(pair).ok()?;
        out[i] = u8::from_str_radix(s, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real records copied from live API responses (see the module doc
    // comment), trimmed to a representative handful.
    const LIST_FIXTURE: &str = include_str!("fixtures/firmware_list.json");
    const DETAIL_FIXTURE: &str = include_str!("fixtures/firmware_detail.json");

    fn fixture_records() -> Vec<FirmwareRecord> {
        parse_list(LIST_FIXTURE).unwrap().records
    }

    fn record(platform: &str) -> FirmwareRecord {
        fixture_records()
            .into_iter()
            .find(|r| r.platform == platform)
            .unwrap_or_else(|| panic!("fixture has no {platform} record"))
    }

    #[test]
    fn list_url_filters_on_product_and_release_channel() {
        let url = list_url(&api_base(), Some("unifi-firmware"), None).to_string();
        assert!(url.starts_with(API_BASE));
        assert!(url.contains("filter=eq%7E%7Eproduct%7E%7Eunifi-firmware"));
        assert!(url.contains("filter=eq%7E%7Echannel%7E%7Erelease"));
        assert!(!url.contains("platform"));
        assert!(url.contains("limit=100000"));
    }

    #[test]
    fn list_url_names_the_product_it_is_given() {
        let url = list_url(&api_base(), Some("unifi-dream"), None).to_string();
        assert!(url.contains("filter=eq%7E%7Eproduct%7E%7Eunifi-dream"));
        assert!(!url.contains("unifi-firmware"));
    }

    #[test]
    fn list_url_for_a_model_alone_has_no_product_filter() {
        let url = list_url(&api_base(), None, Some("UDMPRO")).to_string();
        assert!(!url.contains("product"));
        assert!(url.contains("filter=eq%7E%7Eplatform%7E%7EUDMPRO"));
    }

    #[test]
    fn list_url_for_one_model_adds_a_platform_filter() {
        let url = list_url(&api_base(), Some("unifi-firmware"), Some("U7PG2")).to_string();
        assert!(url.contains("filter=eq%7E%7Eproduct%7E%7Eunifi-firmware"));
        assert!(url.contains("filter=eq%7E%7Echannel%7E%7Erelease"));
        assert!(url.contains("filter=eq%7E%7Eplatform%7E%7EU7PG2"));
    }

    #[test]
    fn parses_every_fixture_record() {
        let parsed = parse_list(LIST_FIXTURE).unwrap();
        assert_eq!(parsed.total, 7);
        assert_eq!(parsed.records.len(), 7);
        assert!(parsed.skipped.is_empty());
    }

    #[test]
    fn a_malformed_record_is_skipped_not_fatal() {
        let body = r#"{"_embedded": {"firmware": [
            {"id": "broken", "product": "unifi-firmware"},
            {"id": "a", "product": "unifi-firmware", "channel": "release", "platform": "US8",
             "version": "v1.0.0+1", "version_major": 1, "version_minor": 0, "version_patch": 0,
             "created": "2024-01-01T00:00:00Z",
             "_links": {"self": {"href": "https://fw-update.ui.com/api/firmware/a"}}}
        ]}}"#;
        let parsed = parse_list(body).unwrap();
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.skipped.len(), 1);
        assert!(parsed.skipped[0].starts_with("broken:"));
    }

    #[test]
    fn every_record_malformed_is_an_error() {
        let body = r#"{"_embedded": {"firmware": [{"id": "x"}, {"id": "y"}]}}"#;
        assert!(matches!(parse_list(body), Err(PluginError::Parse(_))));
    }

    #[test]
    fn empty_and_non_json_responses() {
        assert_eq!(parse_list(r#"{"_links": {}}"#).unwrap().records.len(), 0);
        assert!(matches!(
            parse_list("<html>nope</html>"),
            Err(PluginError::Parse(_))
        ));
    }

    #[test]
    fn detail_response_parses_as_a_single_record() {
        let r = parse_detail(DETAIL_FIXTURE).unwrap();
        assert_eq!(r.platform, "USMINI");
        assert_eq!(r.version, "v1.6.3+574");
        assert_eq!(r.file_size, Some(502_064));
    }

    #[test]
    fn maps_a_record_to_metadata() {
        let meta = record_to_metadata(&record("USMINI"), "unifi");
        // The family is the product line; the model code stays the hardware.
        assert_eq!(meta.device_family, "USW");
        assert_eq!(meta.hardware_targets, vec!["USMINI".to_string()]);
        assert_eq!(meta.version.raw, "v1.6.3+574");
        assert_eq!(meta.version.ordinal, Some(vec![1, 6, 3, 574]));
        assert!(matches!(meta.version.scheme, VersionScheme::VendorNumeric));
        assert_eq!(
            meta.source_url.as_str(),
            "https://fw-update.ui.com/api/firmware/a4fb8871-1951-43bb-9db3-5d8a62e26e3d"
        );
        assert_eq!(meta.release_date, NaiveDate::from_ymd_opt(2020, 2, 14));
        assert_eq!(meta.release_notes_url, None);
        // Verified against the real downloaded file during development.
        assert_eq!(
            meta.sha256,
            decode_sha256("0af65245ffccc1daf964ce92a63de40f152d42d7d63e50994607e4bb5bcc43ee")
        );
        assert!(meta.sha256.is_some());
    }

    #[test]
    fn prefers_the_explicit_release_date_over_upload_time() {
        let r = record("UX");
        assert_eq!(
            r.created.date_naive(),
            NaiveDate::from_ymd_opt(2026, 6, 3).unwrap()
        );
        let meta = record_to_metadata(&r, "unifi");
        assert_eq!(meta.release_date, NaiveDate::from_ymd_opt(2026, 6, 7));
    }

    #[test]
    fn the_product_name_is_the_display_name() {
        let meta = record_to_metadata(&record("USMINI"), "unifi");
        assert_eq!(meta.display_name.as_deref(), Some("Switch Flex Mini"));

        // A model with no line still has a name, and keeps its code as family.
        let mut power = record("USMINI");
        power.platform = "USPRPS".to_string();
        let meta = record_to_metadata(&power, "unifi");
        assert_eq!(meta.device_family, "USPRPS");
        assert_eq!(meta.display_name.as_deref(), Some("Power Backup"));

        // A model nobody has named gets none, so nothing is invented.
        let mut unknown = record("USMINI");
        unknown.platform = "NEWMODEL".to_string();
        assert_eq!(record_to_metadata(&unknown, "unifi").display_name, None);
    }

    #[test]
    fn a_model_with_no_product_line_keeps_its_code_as_its_family() {
        let mut r = record("USMINI");
        r.platform = "USPRPS".to_string(); // a power product: not in any line
        let meta = record_to_metadata(&r, "unifi");
        assert_eq!(meta.device_family, "USPRPS");
        assert_eq!(meta.hardware_targets, vec!["USPRPS".to_string()]);
        assert_eq!(record_to_ref(&r, "unifi").device_family, "USPRPS");
    }

    #[test]
    fn two_models_in_one_line_stay_distinct_by_hardware() {
        // Both are in the UAP line, so the store tells them apart by
        // hardware target and not by family.
        let mut lite = record("U7PG2");
        lite.platform = "U7LT".to_string(); // the fixture has no U7LT record
        let a = record_to_metadata(&record("U7PG2"), "unifi");
        let b = record_to_metadata(&lite, "unifi");
        assert_eq!(a.device_family, b.device_family);
        assert_ne!(a.hardware_targets, b.hardware_targets);
    }

    #[test]
    fn ref_points_at_the_records_own_api_url() {
        let r = record("U7PG2");
        let fref = record_to_ref(&r, "unifi");
        assert_eq!(fref.vendor, "unifi");
        assert_eq!(fref.device_family, "UAP");
        assert_eq!(fref.source_url, r.links.self_link.href);
    }

    #[test]
    fn selection_drops_the_placeholder_record_with_no_file() {
        let selected = select_records(fixture_records(), DEFAULT_PRODUCTS);
        assert_eq!(selected.len(), 6);
        assert!(selected.iter().all(|r| r.platform != "stat"));
    }

    #[test]
    fn selection_drops_other_products_and_channels() {
        let mut records = fixture_records();
        records[0].channel = "beta-public".to_string();
        records[1].product = "unifi-dream".to_string();
        let selected = select_records(records, DEFAULT_PRODUCTS);
        assert_eq!(selected.len(), 4);
    }

    #[test]
    fn selection_keeps_only_the_newest_duplicate_of_a_model_and_version() {
        let newer = record("U7PG2");
        let mut older = newer.clone();
        older.id = "older".to_string();
        older.created = newer.created - chrono::Duration::days(30);

        let selected = select_records(vec![older.clone(), newer.clone()], DEFAULT_PRODUCTS);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, newer.id);

        // Same result regardless of the order the API returns them in.
        let selected = select_records(vec![newer.clone(), older], DEFAULT_PRODUCTS);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, newer.id);
    }

    const CONSOLE_FIXTURE: &str = include_str!("fixtures/console_list.json");

    /// Real records from the four console products. The Express (`UX`) and
    /// Cloud Key Gen2 (`UCKG2`) records are the same files as the
    /// `unifi-firmware` fixture's, which the API lists under both products.
    fn console_records() -> Vec<FirmwareRecord> {
        parse_list(CONSOLE_FIXTURE).unwrap().records
    }

    fn console(product: &str, platform: &str) -> FirmwareRecord {
        console_records()
            .into_iter()
            .find(|r| r.product == product && r.platform == platform)
            .unwrap_or_else(|| panic!("console fixture has no {product} {platform} record"))
    }

    const FIRMWARE_AND_DREAM: &[&str] = &["unifi-firmware", "unifi-dream"];

    #[test]
    fn console_records_map_to_their_product_line_and_name() {
        let meta = record_to_metadata(&console("unifi-dream", "UDMPRO"), "unifi");
        assert_eq!(meta.device_family, "UDM");
        assert_eq!(meta.hardware_targets, vec!["UDMPRO".to_string()]);
        assert_eq!(meta.display_name.as_deref(), Some("Dream Machine Pro"));
        assert_eq!(meta.version.raw, "v5.1.33+44ce47b");
        // A git-hash build has no order, so the ordinal stops at the patch.
        assert_eq!(meta.version.ordinal, Some(vec![5, 1, 33]));
        assert!(meta.sha256.is_some());

        let nvr = record_to_metadata(&console("unifi-nvr", "UNVRPRO"), "unifi");
        assert_eq!(nvr.device_family, "UNVR");
        assert_eq!(
            nvr.display_name.as_deref(),
            Some("Network Video Recorder Pro")
        );
    }

    #[test]
    fn selection_keeps_the_console_products_that_are_asked_for() {
        let mut records = fixture_records();
        records.extend(console_records());

        let only_firmware = select_records(records.clone(), DEFAULT_PRODUCTS);
        assert!(only_firmware.iter().all(|r| r.product == "unifi-firmware"));

        let with_nvr = select_records(records, &["unifi-firmware", "unifi-nvr"]);
        assert!(with_nvr.iter().any(|r| r.platform == "UNVRPRO"));
        assert!(with_nvr.iter().all(|r| r.product != "unifi-dream"));
    }

    #[test]
    fn a_file_listed_under_two_products_is_kept_once_from_the_first_listed() {
        // The Express firmware v4.0.15 is in both unifi-firmware and
        // unifi-dream, with the same SHA-256.
        let firmware = record("UX");
        let dream = console("unifi-dream", "UX");
        assert_eq!(firmware.version, dream.version);
        assert_eq!(firmware.sha256_checksum, dream.sha256_checksum);
        assert_ne!(firmware.id, dream.id);

        for records in [
            vec![firmware.clone(), dream.clone()],
            vec![dream.clone(), firmware.clone()],
        ] {
            let selected = select_records(records.clone(), FIRMWARE_AND_DREAM);
            let ux: Vec<_> = selected.iter().filter(|r| r.platform == "UX").collect();
            assert_eq!(ux.len(), 1);
            assert_eq!(ux[0].id, firmware.id);

            let selected = select_records(records, &["unifi-dream", "unifi-firmware"]);
            let ux: Vec<_> = selected.iter().filter(|r| r.platform == "UX").collect();
            assert_eq!(ux.len(), 1);
            assert_eq!(ux[0].id, dream.id);
        }
    }

    #[test]
    fn selection_drops_the_uos_application_packages() {
        let records = console_records();
        assert!(records.iter().any(|r| r.platform == "uos-deb11-arm64"));
        let selected = select_records(records, SUPPORTED_PRODUCTS);
        assert!(selected.iter().all(|r| !r.platform.starts_with("uos-")));
        assert_eq!(selected.len(), 4);
    }

    #[test]
    fn every_supported_product_is_a_distinct_name_and_the_default_is_supported() {
        let mut names = SUPPORTED_PRODUCTS.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), SUPPORTED_PRODUCTS.len());
        assert!(DEFAULT_PRODUCTS
            .iter()
            .all(|p| SUPPORTED_PRODUCTS.contains(p)));
    }

    #[test]
    fn a_download_url_with_a_file_name_gives_it() {
        let url = |s: &str| Url::parse(s).unwrap();
        assert_eq!(
            download_file_name(&url(
                "https://fw-download.ubnt.com/data/unifi-firmware/259f-U7PG2-6.8.2-5464c424-a775-4715-8bbc-d84602f55445.bin"
            ))
            .as_deref(),
            Some("259f-U7PG2-6.8.2-5464c424-a775-4715-8bbc-d84602f55445.bin")
        );
        // A console package.
        assert_eq!(
            download_file_name(&url(
                "https://fw-download.ubnt.com/data/unifi-drive/uos-deb11-arm64-3.2.14-f4af.deb"
            ))
            .as_deref(),
            Some("uos-deb11-arm64-3.2.14-f4af.deb")
        );
    }

    #[test]
    fn a_download_url_without_a_plain_file_name_gives_none() {
        let url = |s: &str| Url::parse(s).unwrap();
        // No extension: an id, not a file name.
        assert_eq!(
            download_file_name(&url("https://x.example/data/abc123")),
            None
        );
        // A directory.
        assert_eq!(download_file_name(&url("https://x.example/data/")), None);
        assert_eq!(download_file_name(&url("https://x.example")), None);
        // Only an extension, or a trailing dot.
        assert_eq!(
            download_file_name(&url("https://x.example/data/.bin")),
            None
        );
        assert_eq!(
            download_file_name(&url("https://x.example/data/abc.")),
            None
        );
        // Escapes and other characters could hide a separator.
        assert_eq!(
            download_file_name(&url("https://x.example/data/a%2Fb.bin")),
            None
        );
        assert_eq!(
            download_file_name(&url("https://x.example/data/a%20b.bin")),
            None
        );
        // A query string is not part of the name.
        assert_eq!(
            download_file_name(&url("https://x.example/data/fw.bin?token=1")).as_deref(),
            Some("fw.bin")
        );
    }

    #[test]
    fn decodes_valid_sha256_and_rejects_anything_else() {
        let hex = "0af65245ffccc1daf964ce92a63de40f152d42d7d63e50994607e4bb5bcc43ee";
        let bytes = decode_sha256(hex).unwrap();
        assert_eq!(bytes[0], 0x0a);
        assert_eq!(bytes[31], 0xee);
        assert_eq!(decode_sha256(""), None);
        assert_eq!(decode_sha256(&hex[..62]), None);
        assert_eq!(decode_sha256(&hex.replace('a', "z")), None);
        // An MD5 is not a SHA-256.
        assert_eq!(decode_sha256("325eb7e2f3f840fcabdb163ec1d2c23f"), None);
    }
}
