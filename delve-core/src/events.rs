//! `FirmwareEvent`, `Subscriber`, `EventBus`. See the README's
//! "Notifications" section for the full rationale.
//!
//! Fully decoupled from plugins — a `VendorPlugin` only ever produces
//! `FirmwareMetadata`; deciding what's new/changed and fanning that out to
//! subscribers is the engine's job (see `dig_vendor` in engine.rs), not the
//! plugin's.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::model::{FirmwareMetadata, VersionDirection};

/// A field that can change while an entry keeps its identity key
/// (vendor, device family, hardware targets, version), and so can be
/// watched for an `UpdatedRelease`. The version and hardware targets are
/// part of the key: a change to either is a different entry, reported as a
/// new release or a version update whatever is watched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchedField {
    Sha256,
    ReleaseDate,
    ReleaseNotesUrl,
    Signature,
    SourceUrl,
}

impl WatchedField {
    pub const ALL: [WatchedField; 5] = [
        WatchedField::Sha256,
        WatchedField::ReleaseDate,
        WatchedField::ReleaseNotesUrl,
        WatchedField::Signature,
        WatchedField::SourceUrl,
    ];

    /// The name used in config files and in `FieldDiff::field`.
    pub fn name(self) -> &'static str {
        match self {
            WatchedField::Sha256 => "sha256",
            WatchedField::ReleaseDate => "release_date",
            WatchedField::ReleaseNotesUrl => "release_notes_url",
            WatchedField::Signature => "signature",
            WatchedField::SourceUrl => "source_url",
        }
    }

    /// The field's value in `m`, as shown in a `FieldDiff`; empty when absent.
    fn render(self, m: &FirmwareMetadata) -> String {
        match self {
            WatchedField::Sha256 => sha256_hex(m.sha256),
            WatchedField::ReleaseDate => m.release_date.map(|d| d.to_string()).unwrap_or_default(),
            WatchedField::ReleaseNotesUrl => m
                .release_notes_url
                .as_ref()
                .map(|u| u.to_string())
                .unwrap_or_default(),
            WatchedField::Signature => m
                .signature
                .as_ref()
                .map(|s| {
                    format!(
                        "{} by {} ({})",
                        s.scheme,
                        s.signer.as_deref().unwrap_or("unknown signer"),
                        if s.verified { "verified" } else { "unverified" }
                    )
                })
                .unwrap_or_default(),
            WatchedField::SourceUrl => m.source_url.to_string(),
        }
    }

    /// The difference in this field between `old` and `fresh`, if any.
    pub fn diff(self, old: &FirmwareMetadata, fresh: &FirmwareMetadata) -> Option<FieldDiff> {
        let (before, after) = (self.render(old), self.render(fresh));
        (before != after).then_some(FieldDiff {
            field: self.name(),
            before,
            after,
        })
    }
}

/// Why a name isn't a field that can be watched.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum WatchedFieldError {
    #[error("'version' can't be watched: a new version is a new entry, and is always reported")]
    Version,
    #[error(
        "'hardware_targets' can't be watched: the hardware targets are part of an entry's \
         identity, so a change makes a new entry, which is always reported"
    )]
    HardwareTargets,
    #[error("'display_name' can't be watched: it is display text only and never compared")]
    DisplayName,
    #[error(
        "unknown field '{0}'; expected sha256, release_date, release_notes_url, signature or \
         source_url"
    )]
    Unknown(String),
}

impl std::str::FromStr for WatchedField {
    type Err = WatchedFieldError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        if let Some(field) = WatchedField::ALL.into_iter().find(|f| f.name() == name) {
            return Ok(field);
        }
        Err(match name {
            "version" => WatchedFieldError::Version,
            "hardware_targets" => WatchedFieldError::HardwareTargets,
            "display_name" => WatchedFieldError::DisplayName,
            other => WatchedFieldError::Unknown(other.to_string()),
        })
    }
}

/// Which changes to an entry already stored, under the same version, are
/// reported as an `UpdatedRelease`. A new version is always reported,
/// whatever is watched. The default watches only the SHA-256 (a vendor
/// rebuilding a published version); an empty policy reports only new
/// versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangePolicy {
    fields: Vec<WatchedField>,
}

impl Default for ChangePolicy {
    fn default() -> Self {
        Self {
            fields: vec![WatchedField::Sha256],
        }
    }
}

impl ChangePolicy {
    /// Watches `fields`, ignoring repeats.
    pub fn new(fields: impl IntoIterator<Item = WatchedField>) -> Self {
        let mut watched = Vec::new();
        for field in fields {
            if !watched.contains(&field) {
                watched.push(field);
            }
        }
        Self { fields: watched }
    }

    pub fn fields(&self) -> &[WatchedField] {
        &self.fields
    }

    /// The watched fields that differ between `old` and `fresh`, in the
    /// order they are watched. Empty means nothing worth reporting changed.
    pub fn changes(&self, old: &FirmwareMetadata, fresh: &FirmwareMetadata) -> Vec<FieldDiff> {
        self.fields
            .iter()
            .filter_map(|f| f.diff(old, fresh))
            .collect()
    }
}

/// A SHA-256 as lowercase hex, or empty when there is none.
pub(crate) fn sha256_hex(sha: Option<[u8; 32]>) -> String {
    sha.map(|bytes| bytes.iter().map(|b| format!("{b:02x}")).collect())
        .unwrap_or_default()
}

/// One field that differs between two observations of the same firmware
/// entry. `changed_fields` on `UpdatedRelease` is a `Vec` of these rather
/// than a single before/after struct so subscribers can render "what
/// changed" without re-deriving the diff themselves.
#[derive(Debug, Clone)]
pub struct FieldDiff {
    pub field: &'static str,
    pub before: String,
    pub after: String,
}

// `UpdatedRelease` carries two full `FirmwareMetadata`s, so it is much larger
// than `NewRelease`. Events are published a handful at a time per dig, so the
// wasted space doesn't matter, and boxing the fields would make every
// subscriber and constructor more awkward for no real gain.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum FirmwareEvent {
    NewRelease {
        firmware: FirmwareMetadata,
        first_seen: DateTime<Utc>,
    },
    UpdatedRelease {
        firmware: FirmwareMetadata,
        previous: FirmwareMetadata,
        changed_fields: Vec<FieldDiff>,
        version_direction: VersionDirection,
    },
}

#[derive(Debug, Error)]
pub enum SubscriberError {
    #[error("delivery failed: {0}")]
    Delivery(String),
    #[error("subscriber misconfigured: {0}")]
    Configuration(String),
}

#[async_trait]
pub trait Subscriber: Send + Sync {
    fn id(&self) -> &'static str;
    async fn notify(&self, event: &FirmwareEvent) -> Result<(), SubscriberError>;

    /// Called once when a vendor's dig is over, after the last event, **whether
    /// the dig succeeded or failed**. A subscriber that holds events back (the
    /// email subscriber batches a dig's events into one message) sends them
    /// here. The default does nothing, for subscribers that act on each event
    /// as it arrives.
    async fn flush(&self) -> Result<(), SubscriberError> {
        Ok(())
    }
}

/// Fans an event out to every registered subscriber concurrently. One
/// slow/broken subscriber never blocks another — failures are logged and
/// swallowed here rather than propagated, since a webhook being down
/// shouldn't fail the whole `dig`.
pub struct EventBus {
    subscribers: Vec<Box<dyn Subscriber>>,
    change_policy: ChangePolicy,
}

impl EventBus {
    /// A bus with the default [`ChangePolicy`].
    pub fn new(subscribers: Vec<Box<dyn Subscriber>>) -> Self {
        Self {
            subscribers,
            change_policy: ChangePolicy::default(),
        }
    }

    /// Uses `policy` to decide which same-version changes the engine reports.
    #[must_use]
    pub fn with_change_policy(mut self, policy: ChangePolicy) -> Self {
        self.change_policy = policy;
        self
    }

    pub fn change_policy(&self) -> &ChangePolicy {
        &self.change_policy
    }

    /// Tells every subscriber the dig is over. Like `publish`, a failure is
    /// logged and does not stop the others.
    pub async fn flush(&self) {
        let futures = self.subscribers.iter().map(|s| s.flush());
        for result in futures::future::join_all(futures).await {
            if let Err(e) = result {
                tracing::warn!(error = %e, "subscriber flush failed");
            }
        }
    }

    pub async fn publish(&self, event: FirmwareEvent) {
        let futures = self.subscribers.iter().map(|s| s.notify(&event));
        for result in futures::future::join_all(futures).await {
            if let Err(e) = result {
                tracing::warn!(error = %e, "subscriber notification failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> FirmwareMetadata {
        FirmwareMetadata {
            vendor: "v".into(),
            device_family: "f".into(),
            source_url: "https://example.test/a".parse().unwrap(),
            version: crate::model::VersionKey::opaque("1.0".to_string()),
            release_date: None,
            sha256: None,
            signature: None,
            hardware_targets: vec!["hw".into()],
            release_notes_url: None,
            display_name: None,
        }
    }

    #[test]
    fn every_watchable_field_parses_from_its_own_name() {
        for field in WatchedField::ALL {
            assert_eq!(field.name().parse::<WatchedField>(), Ok(field));
        }
    }

    #[test]
    fn identity_and_display_fields_are_rejected_with_the_reason() {
        let err = |name: &str| name.parse::<WatchedField>().unwrap_err().to_string();
        assert!(
            err("version").contains("always reported"),
            "{}",
            err("version")
        );
        assert!(err("hardware_targets").contains("identity"));
        assert!(err("display_name").contains("display text"));
        let unknown = err("sha512");
        assert!(unknown.contains("unknown field 'sha512'"), "{unknown}");
        assert!(
            unknown.contains("release_notes_url"),
            "lists the valid names: {unknown}"
        );
    }

    #[test]
    fn the_default_policy_watches_only_the_hash_and_repeats_are_ignored() {
        assert_eq!(ChangePolicy::default().fields(), [WatchedField::Sha256]);
        let policy = ChangePolicy::new([
            WatchedField::SourceUrl,
            WatchedField::Sha256,
            WatchedField::SourceUrl,
        ]);
        assert_eq!(
            policy.fields(),
            [WatchedField::SourceUrl, WatchedField::Sha256]
        );
    }

    #[test]
    fn each_field_diff_shows_before_and_after_and_empty_for_absent() {
        let old = entry();
        let mut fresh = entry();
        fresh.sha256 = Some([0xab; 32]);
        fresh.release_date = chrono::NaiveDate::from_ymd_opt(2026, 3, 4);
        fresh.release_notes_url = Some("https://example.test/notes".parse().unwrap());
        fresh.signature = Some(crate::model::SignatureInfo {
            scheme: "pgp".into(),
            signer: Some("Vendor".into()),
            verified: true,
        });
        fresh.source_url = "https://example.test/b".parse().unwrap();

        let diffs = ChangePolicy::new(WatchedField::ALL).changes(&old, &fresh);
        let shown: Vec<(&str, &str, &str)> = diffs
            .iter()
            .map(|d| (d.field, d.before.as_str(), d.after.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                (
                    "sha256",
                    "",
                    "abababababababababababababababababababababababababababababababab"
                ),
                ("release_date", "", "2026-03-04"),
                ("release_notes_url", "", "https://example.test/notes"),
                ("signature", "", "pgp by Vendor (verified)"),
                (
                    "source_url",
                    "https://example.test/a",
                    "https://example.test/b"
                ),
            ]
        );
        assert!(ChangePolicy::new(WatchedField::ALL)
            .changes(&old, &old)
            .is_empty());
    }
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Flushed {
        flushes: Arc<AtomicUsize>,
        fail: bool,
    }

    #[async_trait]
    impl Subscriber for Flushed {
        fn id(&self) -> &'static str {
            "flushed"
        }

        async fn notify(&self, _event: &FirmwareEvent) -> Result<(), SubscriberError> {
            Ok(())
        }

        async fn flush(&self) -> Result<(), SubscriberError> {
            self.flushes.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(SubscriberError::Delivery("down".into()))
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn flush_reaches_every_subscriber_even_when_one_fails() {
        let (a, b) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let bus = EventBus::new(vec![
            Box::new(Flushed {
                flushes: a.clone(),
                fail: true,
            }),
            Box::new(Flushed {
                flushes: b.clone(),
                fail: false,
            }),
        ]);

        bus.flush().await;

        assert_eq!((a.load(Ordering::SeqCst), b.load(Ordering::SeqCst)), (1, 1));
    }

    struct NoFlush;

    #[async_trait]
    impl Subscriber for NoFlush {
        fn id(&self) -> &'static str {
            "no-flush"
        }

        async fn notify(&self, _event: &FirmwareEvent) -> Result<(), SubscriberError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_subscriber_that_does_not_batch_needs_no_flush() {
        // The default `flush` does nothing, so existing subscribers are unchanged.
        assert!(NoFlush.flush().await.is_ok());
    }
}
