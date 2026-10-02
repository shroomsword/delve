//! Vendor-agnostic data model. See the README's "Data model and identity
//! keys" section for the full rationale.
//!
//! `VersionKey` deliberately separates logical precedence (can we say one
//! version supersedes another) from chronological order (`release_date` on
//! `FirmwareMetadata`). Vendors sometimes disagree with themselves — a
//! backported patch, a yanked-and-republished version with an unchanged
//! date — so these two notions of "order" must never be conflated.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use url::Url;

/// A candidate firmware artifact discovered by a vendor plugin, before its
/// full metadata has been pulled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirmwareRef {
    pub vendor: String,
    pub device_family: String,
    pub source_url: Url,
    pub discovered_at: DateTime<Utc>,
}

/// Everything we know about one firmware release, short of the binary
/// itself. `MetadataStore::upsert` persists this; the binary is only ever
/// pulled on demand via `VendorPlugin::fetch` (the `unearth` command).
///
/// `vendor`/`device_family`/`source_url` are duplicated here from
/// `FirmwareRef` (rather than requiring callers to carry both around) so
/// that a `StoredFirmware` from `MetadataStore::resolve_one`/`resolve_many`
/// is enough on its own to drive `provenance --id`/`unearth --id` without a
/// second lookup — `source_url` specifically is
/// what `unearth` needs to rebuild the `FirmwareRef` it hands to
/// `VendorPlugin::fetch`. The engine populates all three from the
/// originating `FirmwareRef` right after a plugin's `metadata()` call
/// returns — plugin authors don't need to set them themselves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirmwareMetadata {
    pub vendor: String,
    pub device_family: String,
    pub source_url: Url,
    pub version: VersionKey,
    pub release_date: Option<NaiveDate>,
    pub sha256: Option<[u8; 32]>,
    pub signature: Option<SignatureInfo>,
    /// Part of this entry's identity key alongside vendor/device_family/version
    /// — some vendors reuse version strings across hardware revisions that
    /// actually ship different binaries, so hardware has to be part of the
    /// key too (see the README's "Data model and identity keys" section).
    pub hardware_targets: Vec<String>,
    pub release_notes_url: Option<Url>,
    /// A human-readable product name, such as "Switch Flex Mini", for
    /// notifications and `catalog`. Not part of the identity key and not
    /// compared when deciding what changed. Plugins that have no such name
    /// leave it `None`; entries stored before this field existed read as
    /// `None` until their next dig.
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureInfo {
    pub scheme: String, // e.g. "pgp", "x509"
    pub signer: Option<String>,
    pub verified: bool,
}

/// How a vendor expresses version numbers, and whether we can derive a
/// reliable ordering from it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VersionScheme {
    Semver,
    /// A consistent but non-semver numeric scheme (e.g. "12.4.2T",
    /// "R8.1.1.4"). Plugins own the parser for their vendor's format.
    VendorNumeric,
    /// No reliable ordering can be derived; only equality/inequality is known.
    Opaque,
}

/// A version as reported by a vendor, plus (when derivable) a comparable
/// ordinal for determining precedence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionKey {
    /// Exactly as the vendor published it — used for display and as the
    /// natural-key component in storage.
    pub raw: String,
    pub scheme: VersionScheme,
    /// Comparable tuple. `None` when `scheme` is `Opaque` or the plugin
    /// couldn't parse this particular string.
    pub ordinal: Option<Vec<u64>>,
}

impl VersionKey {
    pub fn opaque(raw: impl Into<String>) -> Self {
        Self {
            raw: raw.into(),
            scheme: VersionScheme::Opaque,
            ordinal: None,
        }
    }
}

impl PartialEq for VersionKey {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl PartialOrd for VersionKey {
    /// Only returns `Some` when both sides have a derived ordinal. Two
    /// `Opaque` versions are never orderable against each other — callers
    /// (see `VersionDirection`) must treat that as "changed, direction
    /// unknown," not silently fall back to string comparison.
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (&self.ordinal, &other.ordinal) {
            (Some(a), Some(b)) => a.partial_cmp(b),
            _ => None,
        }
    }
}

/// Direction of a version change between two observations of the same
/// firmware entry. Surfaced in `FirmwareEvent::UpdatedRelease` diffs (see
/// the README's "Notifications" section) — a downgrade is a meaningfully
/// different signal to a subscriber than an upgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VersionDirection {
    Newer,
    Older,
    Unordered,
}

impl VersionDirection {
    pub fn between(old: &VersionKey, new: &VersionKey) -> Self {
        match old.partial_cmp(new) {
            Some(std::cmp::Ordering::Less) => VersionDirection::Newer,
            Some(std::cmp::Ordering::Greater) => VersionDirection::Older,
            _ => VersionDirection::Unordered,
        }
    }
}

/// Canonicalizes `hardware_targets` into a single sorted, delimited string
/// for use as part of the SQL identity key (see the README's "Data model
/// and identity keys" and "Storage" sections). The real `Vec<String>`
/// stays in `FirmwareMetadata` for display and querying — this is purely a
/// storage-key concern.
pub fn hardware_key(targets: &[String]) -> String {
    let mut sorted: Vec<&str> = targets.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(raw: &str, ordinal: Option<Vec<u64>>) -> VersionKey {
        VersionKey {
            raw: raw.to_string(),
            scheme: if ordinal.is_some() {
                VersionScheme::Semver
            } else {
                VersionScheme::Opaque
            },
            ordinal,
        }
    }

    #[test]
    fn hardware_key_sorts_and_joins_regardless_of_input_order() {
        let a = hardware_key(&["rev-b".into(), "rev-a".into()]);
        let b = hardware_key(&["rev-a".into(), "rev-b".into()]);
        assert_eq!(
            a, b,
            "order of hardware_targets must not affect the derived key"
        );
        assert_eq!(a, "rev-a+rev-b");
    }

    #[test]
    fn hardware_key_empty_input_is_empty_string() {
        assert_eq!(hardware_key(&[]), "");
    }

    #[test]
    fn version_key_equality_is_by_raw_string_only() {
        // Two Opaque versions with the same raw string are equal even
        // though neither has a derivable ordinal.
        let a = key("1.2.3-vendor-build-7", None);
        let b = key("1.2.3-vendor-build-7", None);
        assert_eq!(a, b);
    }

    #[test]
    fn version_key_ordering_requires_both_sides_to_have_an_ordinal() {
        let with_ordinal = key("1.2.0", Some(vec![1, 2, 0]));
        let opaque = key("R8-weird", None);
        // Neither direction is derivable when either side lacks an ordinal.
        assert_eq!(with_ordinal.partial_cmp(&opaque), None);
        assert_eq!(opaque.partial_cmp(&with_ordinal), None);
    }

    #[test]
    fn version_key_ordering_compares_ordinals_lexicographically() {
        let older = key("1.2.0", Some(vec![1, 2, 0]));
        let newer = key("1.10.0", Some(vec![1, 10, 0]));
        assert!(
            older < newer,
            "1.10.0 must sort after 1.2.0 by numeric tuple, not string compare"
        );
    }

    #[test]
    fn version_direction_newer_and_older_are_correctly_distinguished() {
        let v1 = key("1.0.0", Some(vec![1, 0, 0]));
        let v2 = key("2.0.0", Some(vec![2, 0, 0]));

        assert_eq!(VersionDirection::between(&v1, &v2), VersionDirection::Newer);
        assert_eq!(VersionDirection::between(&v2, &v1), VersionDirection::Older);
    }

    #[test]
    fn version_direction_is_unordered_for_two_opaque_versions() {
        // This is the rule the README's "Data model and identity keys"
        // section calls out explicitly: a plugin that can't parse a
        // vendor's scheme must report an unordered change, never a
        // guessed direction.
        let a = key("build-2024-01", None);
        let b = key("build-2024-07", None);
        assert_eq!(
            VersionDirection::between(&a, &b),
            VersionDirection::Unordered
        );
    }

    #[test]
    fn version_direction_equal_ordinals_are_unordered_not_newer() {
        let a = key("1.0.0", Some(vec![1, 0, 0]));
        let b = key("1.0.0-rebuild", Some(vec![1, 0, 0]));
        // Equal ordinals produce Ordering::Equal, which `between` maps to
        // Unordered (neither Newer nor Older) rather than picking one.
        assert_eq!(
            VersionDirection::between(&a, &b),
            VersionDirection::Unordered
        );
    }

    /// Every database written before `display_name` existed stores
    /// metadata without it; those rows must keep loading.
    #[test]
    fn metadata_stored_before_display_name_existed_still_loads() {
        let old = r#"{
            "vendor": "unifi",
            "device_family": "USW",
            "source_url": "https://example.test/fw",
            "version": {"raw": "1.0", "scheme": "Semver", "ordinal": [1, 0]},
            "release_date": null,
            "sha256": null,
            "signature": null,
            "hardware_targets": ["USMINI"],
            "release_notes_url": null
        }"#;
        let meta: FirmwareMetadata = serde_json::from_str(old).unwrap();
        assert_eq!(meta.display_name, None);

        let mut named = meta.clone();
        named.display_name = Some("Switch Flex Mini".into());
        let again: FirmwareMetadata =
            serde_json::from_str(&serde_json::to_string(&named).unwrap()).unwrap();
        assert_eq!(again.display_name.as_deref(), Some("Switch Flex Mini"));
    }
}
