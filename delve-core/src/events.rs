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

/// Somewhere a dig's events are delivered: an email, a webhook.
#[async_trait]
pub trait Subscriber: Send + Sync {
    fn id(&self) -> &'static str;

    /// Delivers every event of one `dig`, in the order they were found, once
    /// the dig is over: after the last vendor, whether or not every vendor
    /// succeeded. Called once per dig, and never with an empty slice — a dig
    /// that found nothing notifies no one.
    async fn notify(&self, events: &[FirmwareEvent]) -> Result<(), SubscriberError>;
}

/// Collects a dig's events and delivers them to every subscriber at once
/// when the dig is over (see [`EventBus::deliver`]). Each event is logged as
/// it is published, so the log is a complete record of what a dig found,
/// whenever it ends and whatever happens to delivery. Delivery to the
/// subscribers is concurrent, and one slow or broken subscriber never blocks
/// another — failures are logged rather than propagated, since a webhook
/// being down shouldn't fail the whole `dig`.
pub struct EventBus {
    subscribers: Vec<Box<dyn Subscriber>>,
    change_policy: ChangePolicy,
    pending: std::sync::Mutex<Vec<FirmwareEvent>>,
}

impl EventBus {
    /// A bus with the default [`ChangePolicy`].
    pub fn new(subscribers: Vec<Box<dyn Subscriber>>) -> Self {
        Self {
            subscribers,
            change_policy: ChangePolicy::default(),
            pending: std::sync::Mutex::new(Vec::new()),
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

    /// Logs `event` now, and holds it for [`deliver`](Self::deliver).
    pub fn publish(&self, event: FirmwareEvent) {
        log_event(&event);
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
    }

    /// How many events are waiting for [`deliver`](Self::deliver).
    pub fn pending(&self) -> usize {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Hands every event published since the last delivery to every
    /// subscriber, in one `notify` call each, and empties the queue. Call it
    /// once, when the whole dig is over — including when a vendor failed, so
    /// what the others found is still delivered. With nothing to deliver, no
    /// subscriber is called. A failure is logged and does not stop the others.
    pub async fn deliver(&self) {
        let events = std::mem::take(
            &mut *self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if events.is_empty() {
            return;
        }
        let futures = self.subscribers.iter().map(|s| s.notify(&events));
        let results = futures::future::join_all(futures).await;
        for (subscriber, result) in self.subscribers.iter().zip(results) {
            if let Err(e) = result {
                tracing::warn!(
                    subscriber = subscriber.id(),
                    events = events.len(),
                    error = %e,
                    "subscriber notification failed"
                );
            }
        }
    }
}

/// Writes one line for `event` to the log: the vendor, the product name when
/// there is one, the device family, the hardware and the version, plus the
/// previous version and its direction for an update.
fn log_event(event: &FirmwareEvent) {
    match event {
        FirmwareEvent::NewRelease {
            firmware,
            first_seen,
        } => {
            tracing::info!(
                vendor = %firmware.vendor,
                name = firmware.display_name.as_deref(),
                device_family = %firmware.device_family,
                hardware = %firmware.hardware_targets.join("+"),
                version = %firmware.version.raw,
                first_seen = %first_seen,
                "new firmware release detected"
            );
        }
        FirmwareEvent::UpdatedRelease {
            firmware,
            previous,
            changed_fields,
            version_direction,
        } => {
            tracing::info!(
                vendor = %firmware.vendor,
                name = firmware.display_name.as_deref(),
                device_family = %firmware.device_family,
                hardware = %firmware.hardware_targets.join("+"),
                version = %firmware.version.raw,
                previous_version = %previous.version.raw,
                ?version_direction,
                field_count = changed_fields.len(),
                "firmware release updated"
            );
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
    use chrono::TimeZone;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    fn release(version: &str) -> FirmwareEvent {
        let mut firmware = entry();
        firmware.version = crate::model::VersionKey::opaque(version.to_string());
        FirmwareEvent::NewRelease {
            firmware,
            first_seen: chrono::Utc.with_ymd_and_hms(2026, 10, 1, 12, 0, 0).unwrap(),
        }
    }

    fn version_of(event: &FirmwareEvent) -> &str {
        match event {
            FirmwareEvent::NewRelease { firmware, .. }
            | FirmwareEvent::UpdatedRelease { firmware, .. } => &firmware.version.raw,
        }
    }

    /// The versions in each `notify` call a `Recorder` received.
    type Calls = Arc<Mutex<Vec<Vec<String>>>>;

    /// Records each `notify` call's versions, and fails if told to.
    struct Recorder {
        calls: Calls,
        fail: bool,
    }

    #[async_trait]
    impl Subscriber for Recorder {
        fn id(&self) -> &'static str {
            "recorder"
        }

        async fn notify(&self, events: &[FirmwareEvent]) -> Result<(), SubscriberError> {
            self.calls
                .lock()
                .unwrap()
                .push(events.iter().map(|e| version_of(e).to_string()).collect());
            if self.fail {
                Err(SubscriberError::Delivery("down".into()))
            } else {
                Ok(())
            }
        }
    }

    fn recorder(fail: bool) -> (Box<dyn Subscriber>, Calls) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Box::new(Recorder {
                calls: calls.clone(),
                fail,
            }),
            calls,
        )
    }

    #[tokio::test]
    async fn events_are_held_until_delivery_then_sent_once_in_order() {
        let (sub, calls) = recorder(false);
        let bus = EventBus::new(vec![sub]);
        bus.publish(release("1"));
        bus.publish(release("2"));
        bus.publish(release("3"));
        assert!(
            calls.lock().unwrap().is_empty(),
            "nothing is sent before delivery"
        );
        assert_eq!(bus.pending(), 3);

        bus.deliver().await;
        assert_eq!(*calls.lock().unwrap(), [["1", "2", "3"]]);
        assert_eq!(bus.pending(), 0);

        // Delivered events aren't sent again.
        bus.deliver().await;
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn nothing_to_deliver_calls_no_subscriber() {
        let (sub, calls) = recorder(false);
        EventBus::new(vec![sub]).deliver().await;
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn delivery_reaches_every_subscriber_even_when_one_fails() {
        let (failing, failing_calls) = recorder(true);
        let (working, working_calls) = recorder(false);
        let bus = EventBus::new(vec![failing, working]);
        bus.publish(release("1"));
        bus.deliver().await;
        assert_eq!(failing_calls.lock().unwrap().len(), 1);
        assert_eq!(*working_calls.lock().unwrap(), [["1"]]);
    }

    /// Collects what is logged so a test can read it back.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Publishes `events` on a bus with no subscribers and returns the log.
    fn logged(events: Vec<FirmwareEvent>) -> String {
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let bus = EventBus::new(vec![]);
            for event in events {
                bus.publish(event);
            }
        });
        let bytes = capture.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn every_event_is_logged_when_it_is_published_not_when_it_is_delivered() {
        let log = logged(vec![release("1.1"), release("1.2")]);
        assert_eq!(
            log.matches("new firmware release detected").count(),
            2,
            "{log}"
        );
        for field in [
            "vendor=v",
            "device_family=f",
            "hardware=hw",
            "version=1.1",
            "version=1.2",
        ] {
            assert!(log.contains(field), "missing {field}: {log}");
        }
    }

    #[test]
    fn the_display_name_is_logged_when_there_is_one() {
        let mut named = release("1.1");
        if let FirmwareEvent::NewRelease { firmware, .. } = &mut named {
            firmware.display_name = Some("Switch Flex Mini".into());
        }
        let log = logged(vec![named]);
        assert!(log.contains("name=\"Switch Flex Mini\""), "{log}");
        // With no name the field is left out, not logged empty.
        assert!(!logged(vec![release("1.1")]).contains("name="));
    }

    #[test]
    fn an_update_is_logged_with_both_versions() {
        let mut previous = entry();
        previous.version = crate::model::VersionKey::opaque("1.0".to_string());
        let mut firmware = entry();
        firmware.version = crate::model::VersionKey::opaque("1.1".to_string());
        let log = logged(vec![FirmwareEvent::UpdatedRelease {
            firmware,
            previous,
            changed_fields: vec![],
            version_direction: VersionDirection::Newer,
        }]);
        for field in [
            "version=1.1",
            "previous_version=1.0",
            "firmware release updated",
        ] {
            assert!(log.contains(field), "missing {field}: {log}");
        }
    }
}
