//! The per-vendor dig loop. See the README's "Baseline vs incremental
//! digs" section for the full rationale, and the baseline/incremental
//! logic and the correctness notes this implementation follows closely.

use std::collections::HashMap;

use uuid::Uuid;

use crate::context::ScrapeContext;
use crate::events::{EventBus, FieldDiff, FirmwareEvent};
use crate::model::{FirmwareMetadata, VersionDirection};
use crate::plugin::{PluginError, VendorPlugin};
#[cfg(test)]
use crate::store::StoredFirmware;
use crate::store::{MetadataStore, RunKind, RunOutcome, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Plugin(#[from] PluginError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Diffs two observations of the same entry into the field-level changes an
/// `UpdatedRelease` event carries. Deliberately minimal here — sha256 and
/// version are the two fields the default "what counts as changed" policy
/// the README's "Notifications" section treats as notification-worthy;
/// widening this to release notes /
/// hardware targets is a config knob to add later, not a structural change.
fn diff_fields(
    old: &crate::model::FirmwareMetadata,
    fresh: &crate::model::FirmwareMetadata,
) -> Vec<FieldDiff> {
    let mut diffs = Vec::new();
    if old.version.raw != fresh.version.raw {
        diffs.push(FieldDiff {
            field: "version",
            before: old.version.raw.clone(),
            after: fresh.version.raw.clone(),
        });
    }
    if old.sha256 != fresh.sha256 {
        diffs.push(FieldDiff {
            field: "sha256",
            before: crate::events::sha256_hex(old.sha256),
            after: crate::events::sha256_hex(fresh.sha256),
        });
    }
    diffs
}

async fn determine_run_kind(
    store: &dyn MetadataStore,
    vendor_id: &str,
) -> Result<RunKind, StoreError> {
    match store.has_completed_baseline(vendor_id).await? {
        true => Ok(RunKind::Incremental),
        false => Ok(RunKind::Baseline),
    }
}

/// Runs one vendor's dig: discover candidates, pull metadata for each,
/// persist unconditionally, and — on incremental runs only — diff against
/// the prior observation and publish events for anything new or changed.
///
/// Baseline runs persist everything but never call `bus.publish` (see the
/// README's "Baseline vs incremental digs" section) —
/// this is what makes a vendor's first-ever dig silent. `mark_baseline_complete`
/// is only called after the full loop below succeeds; if this function
/// returns early via `?`, the caller must not treat the run as a completed
/// baseline (see the correctness notes in the README's "Baseline vs
/// incremental digs" section about partial-failure runs).
pub async fn dig_vendor(
    vendor: &dyn VendorPlugin,
    ctx: &ScrapeContext,
    store: &dyn MetadataStore,
    bus: &EventBus,
) -> Result<(), EngineError> {
    let result = dig_vendor_run(vendor, ctx, store, bus).await;
    // However the dig ended, subscribers holding events back (the email
    // subscriber batches them) get to send what they have. Done out here
    // because the body below returns early through `?` in several places.
    bus.flush().await;
    result
}

async fn dig_vendor_run(
    vendor: &dyn VendorPlugin,
    ctx: &ScrapeContext,
    store: &dyn MetadataStore,
    bus: &EventBus,
) -> Result<(), EngineError> {
    let vendor_id = vendor.vendor_id();
    let run_kind = determine_run_kind(store, vendor_id).await?;
    let run_id: Uuid = store.start_run(vendor_id, run_kind).await?;

    let result = run_loop(vendor, ctx, store, bus, run_kind, run_id).await;

    match &result {
        Ok(()) => {
            store.complete_run(run_id, RunOutcome::Success).await?;
            if run_kind == RunKind::Baseline {
                store.mark_baseline_complete(vendor_id).await?;
            }
        }
        Err(e) => {
            store
                .complete_run(run_id, RunOutcome::Failed(e.to_string()))
                .await?;
        }
    }

    result
}

async fn run_loop(
    vendor: &dyn VendorPlugin,
    ctx: &ScrapeContext,
    store: &dyn MetadataStore,
    bus: &EventBus,
    run_kind: RunKind,
    run_id: Uuid,
) -> Result<(), EngineError> {
    let refs = vendor.discover(ctx).await?;

    // The newest version known for each line (vendor/device_family/hardware)
    // *before this dig*, looked up the first time the line comes up — which is
    // always before this dig stores anything for it. Every release of the line
    // is compared with this, not with the store's running state: otherwise each
    // release would be compared with ones stored moments earlier in the same
    // dig, so a line delve had never seen would read as one new release and a
    // string of "updates" and "downgrades", in whatever order the vendor's API
    // happened to list them.
    let mut known_before_dig: HashMap<(String, Vec<String>), Option<FirmwareMetadata>> =
        HashMap::new();

    for r in refs {
        let mut fresh = vendor.metadata(ctx, &r).await?;
        // Populated here rather than left to each plugin author — guarantees
        // these always match the FirmwareRef that produced this metadata,
        // and means a plugin's metadata() implementation never has to
        // remember to set them itself.
        fresh.vendor = r.vendor.clone();
        fresh.device_family = r.device_family.clone();
        fresh.source_url = r.source_url.clone();

        // Two different questions, both needed: `lookup` asks "have we
        // stored exactly this version before" (catches a vendor silently
        // rebuilding an already-published version number); `latest_known`
        // asks "what's the newest version we knew about for this line
        // before this dig" (the only way to detect an ordinary version bump
        // and its direction — `lookup`'s exact-version key can never match a
        // version that just arrived for the first time, so on its own it
        // can't tell a bump apart from a first-ever release).
        let exact_key = crate::store::FirmwareKey::from_ref_and_metadata(&r, &fresh);
        let exact_prior = store.lookup(&exact_key).await?;
        let line = (fresh.device_family.clone(), fresh.hardware_targets.clone());
        let latest_known_before_dig = match known_before_dig.get(&line) {
            Some(known) => known.clone(),
            None => {
                let known = store
                    .latest_known(&fresh.vendor, &fresh.device_family, &fresh.hardware_targets)
                    .await?;
                known_before_dig.insert(line, known.clone());
                known
            }
        };

        // Always persisted, regardless of run_kind.
        store.upsert(&r, &fresh, run_id).await?;

        if run_kind == RunKind::Baseline {
            continue; // no notifications on the first dig, by design
        }

        match exact_prior {
            Some(old) => {
                // This version is already stored. Whether a change to it is
                // reported is the bus's change policy: by default only a new
                // SHA-256, a silent rebuild under an already-published
                // version number. There's no meaningful version direction
                // here (old.version.raw == fresh.version.raw by construction,
                // since exact_key was built from fresh's own version), so
                // this always reports Unordered rather than a guessed one.
                let changed_fields = bus.change_policy().changes(&old, &fresh);
                if !changed_fields.is_empty() {
                    bus.publish(FirmwareEvent::UpdatedRelease {
                        firmware: fresh,
                        previous: old,
                        changed_fields,
                        version_direction: VersionDirection::Unordered,
                    })
                    .await;
                }
            }
            None => match latest_known_before_dig {
                // Nothing was known for this vendor/device_family/hardware
                // line before this dig, so every release of it is new to
                // delve — not a bump from anything, including from another
                // release found earlier in this same dig.
                None => {
                    bus.publish(FirmwareEvent::NewRelease {
                        firmware: fresh,
                        first_seen: chrono::Utc::now(),
                    })
                    .await;
                }
                // A version we've never stored before has arrived, and we
                // already knew some other version for this line before the
                // dig — this is the version-bump case, and version_direction
                // here can actually be Newer or Older (not just Unordered),
                // since `old` and `fresh` are genuinely different versions.
                // Older means the vendor published a version below the
                // newest one delve knew before this dig.
                Some(old) => {
                    let changed_fields = diff_fields(&old, &fresh);
                    let version_direction = VersionDirection::between(&old.version, &fresh.version);
                    bus.publish(FirmwareEvent::UpdatedRelease {
                        firmware: fresh,
                        previous: old,
                        changed_fields,
                        version_direction,
                    })
                    .await;
                }
            },
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Transport;
    use crate::events::{Subscriber, SubscriberError};
    use crate::model::{FirmwareMetadata, FirmwareRef, VersionKey, VersionScheme};
    use crate::plugin::{ArtifactSink, PluginCapabilities};
    use crate::store::{FirmwareKey, FirmwareRevision, FirmwareSelector};
    use async_trait::async_trait;
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};

    // ---------------------------------------------------------------
    // Test doubles
    // ---------------------------------------------------------------

    /// A plugin whose `discover`/`metadata` results are fixed at
    /// construction time, so each test can set up exactly the scenario it
    /// wants to exercise (a new release, a changed hash, a mid-scrape
    /// failure) without touching the network.
    struct MockPlugin {
        refs: Vec<FirmwareRef>,
        metadata_by_url: HashMap<String, Result<FirmwareMetadata, String>>,
    }

    #[async_trait]
    impl VendorPlugin for MockPlugin {
        fn vendor_id(&self) -> &'static str {
            "mockvendor"
        }

        fn capabilities(&self) -> PluginCapabilities {
            PluginCapabilities {
                tos_reviewed: true,
                supports_signature_verification: false,
            }
        }

        async fn discover(&self, _ctx: &ScrapeContext) -> Result<Vec<FirmwareRef>, PluginError> {
            Ok(self.refs.clone())
        }

        async fn metadata(
            &self,
            _ctx: &ScrapeContext,
            r: &FirmwareRef,
        ) -> Result<FirmwareMetadata, PluginError> {
            match self.metadata_by_url.get(r.source_url.as_str()) {
                Some(Ok(m)) => Ok(m.clone()),
                Some(Err(msg)) => Err(PluginError::UnexpectedResponse(msg.clone())),
                None => Err(PluginError::UnexpectedResponse(format!(
                    "no mock metadata for {}",
                    r.source_url
                ))),
            }
        }

        async fn fetch(
            &self,
            _ctx: &ScrapeContext,
            _r: &FirmwareRef,
            _sink: &mut dyn ArtifactSink,
        ) -> Result<(), PluginError> {
            Err(PluginError::Unimplemented) // not exercised by dig — see the README's "CLI commands" section
        }
    }

    /// In-memory `MetadataStore`. Deliberately independent of
    /// `delve-store-sqlite` — these tests are about the engine's
    /// baseline/diff logic, not the storage backend, so a `sqlx`-backed
    /// store would be slower to run and would conflate two different
    /// things under test. `delve-store-sqlite` gets its own test suite
    /// against the real backend.
    #[derive(Default)]
    struct MockStore {
        current: Mutex<HashMap<String, FirmwareMetadata>>,
        revisions: Mutex<Vec<FirmwareRevision>>,
        baselines: Mutex<HashSet<String>>,
        run_outcomes: Mutex<Vec<(Uuid, RunOutcome)>>,
    }

    fn composite_key(vendor: &str, device_family: &str, hw: &str, version: &str) -> String {
        format!("{vendor}\0{device_family}\0{hw}\0{version}")
    }

    impl MockStore {
        /// Exposes recorded run outcomes as simple tags, so tests can
        /// assert `complete_run` was called with the right outcome without
        /// needing `RunOutcome` to implement `PartialEq`.
        fn run_outcome_kinds(&self) -> Vec<&'static str> {
            self.run_outcomes
                .lock()
                .unwrap()
                .iter()
                .map(|(_, o)| match o {
                    RunOutcome::Success => "success",
                    RunOutcome::PartialFailure { .. } => "partial_failure",
                    RunOutcome::Failed(_) => "failed",
                })
                .collect()
        }
    }

    #[async_trait]
    impl MetadataStore for MockStore {
        async fn lookup(
            &self,
            key: &FirmwareKey<'_>,
        ) -> Result<Option<FirmwareMetadata>, StoreError> {
            let hw = crate::model::hardware_key(key.hardware_targets);
            let k = composite_key(key.vendor, key.device_family, &hw, key.version_raw);
            Ok(self.current.lock().unwrap().get(&k).cloned())
        }

        async fn latest_known(
            &self,
            vendor: &str,
            device_family: &str,
            hardware_targets: &[String],
        ) -> Result<Option<FirmwareMetadata>, StoreError> {
            let hw = crate::model::hardware_key(hardware_targets);
            // Mirrors SqliteStore's approach: prefer the highest derivable
            // ordinal among matching entries; entries with no ordinal
            // (Opaque scheme) are never picked over one that has an
            // ordinal, and if *no* matching entry has an ordinal at all,
            // there's nothing reliable to return.
            let current = self.current.lock().unwrap();
            Ok(current
                .values()
                .filter(|m| {
                    m.vendor == vendor
                        && m.device_family == device_family
                        && crate::model::hardware_key(&m.hardware_targets) == hw
                })
                .filter(|m| m.version.ordinal.is_some())
                .max_by(|a, b| a.version.ordinal.cmp(&b.version.ordinal))
                .cloned())
        }

        async fn upsert(
            &self,
            r: &FirmwareRef,
            meta: &FirmwareMetadata,
            run_id: Uuid,
        ) -> Result<(), StoreError> {
            let hw = crate::model::hardware_key(&meta.hardware_targets);
            let k = composite_key(&r.vendor, &r.device_family, &hw, &meta.version.raw);
            self.current.lock().unwrap().insert(k, meta.clone());
            self.revisions.lock().unwrap().push(FirmwareRevision {
                metadata: meta.clone(),
                observed_at: chrono::Utc::now(),
                run_id,
            });
            Ok(())
        }

        async fn has_completed_baseline(&self, vendor_id: &str) -> Result<bool, StoreError> {
            Ok(self.baselines.lock().unwrap().contains(vendor_id))
        }

        async fn mark_baseline_complete(&self, vendor_id: &str) -> Result<(), StoreError> {
            self.baselines.lock().unwrap().insert(vendor_id.to_string());
            Ok(())
        }

        async fn clear_baseline(&self, vendor_id: &str) -> Result<(), StoreError> {
            self.baselines.lock().unwrap().remove(vendor_id);
            Ok(())
        }

        async fn start_run(&self, _vendor_id: &str, _kind: RunKind) -> Result<Uuid, StoreError> {
            Ok(Uuid::new_v4())
        }

        async fn complete_run(&self, run_id: Uuid, outcome: RunOutcome) -> Result<(), StoreError> {
            self.run_outcomes.lock().unwrap().push((run_id, outcome));
            Ok(())
        }

        async fn history(
            &self,
            key: &FirmwareKey<'_>,
        ) -> Result<Vec<FirmwareRevision>, StoreError> {
            let hw = crate::model::hardware_key(key.hardware_targets);
            Ok(self
                .revisions
                .lock()
                .unwrap()
                .iter()
                .filter(|rev| {
                    rev.metadata.vendor == key.vendor
                        && rev.metadata.device_family == key.device_family
                        && crate::model::hardware_key(&rev.metadata.hardware_targets) == hw
                        && rev.metadata.version.raw == key.version_raw
                })
                .cloned()
                .collect())
        }

        async fn all_current(&self, vendor_id: &str) -> Result<Vec<FirmwareMetadata>, StoreError> {
            Ok(self
                .current
                .lock()
                .unwrap()
                .values()
                .filter(|m| m.vendor == vendor_id)
                .cloned()
                .collect())
        }

        async fn resolve_one(
            &self,
            _selector: &FirmwareSelector,
        ) -> Result<Option<StoredFirmware>, StoreError> {
            unimplemented!("not exercised by engine tests — see delve-store-sqlite's test suite")
        }

        async fn resolve_many(
            &self,
            _selector: &FirmwareSelector,
        ) -> Result<Vec<StoredFirmware>, StoreError> {
            unimplemented!("not exercised by engine tests — see delve-store-sqlite's test suite")
        }
    }

    /// Captures every published event, in order, for assertions.
    #[derive(Default)]
    struct CollectingSubscriber {
        events: Arc<Mutex<Vec<FirmwareEvent>>>,
    }

    #[async_trait]
    impl Subscriber for CollectingSubscriber {
        fn id(&self) -> &'static str {
            "test-collector"
        }

        async fn notify(&self, event: &FirmwareEvent) -> Result<(), SubscriberError> {
            self.events.lock().unwrap().push(event.clone());
            Ok(())
        }
    }

    // ---------------------------------------------------------------
    // Fixtures
    // ---------------------------------------------------------------

    fn ctx() -> ScrapeContext {
        ScrapeContext::new(
            Transport::Direct,
            Default::default(),
            Default::default(),
            Default::default(),
        )
        .expect("Direct transport never fails to build")
    }

    fn firmware_ref(url: &str) -> FirmwareRef {
        FirmwareRef {
            vendor: "mockvendor".into(),
            device_family: "isr4000".into(),
            source_url: url.parse().unwrap(),
            discovered_at: chrono::Utc::now(),
        }
    }

    fn metadata(version: &str, ordinal: Vec<u64>, sha_byte: u8) -> FirmwareMetadata {
        FirmwareMetadata {
            vendor: "mockvendor".into(),
            device_family: "isr4000".into(),
            // Overwritten by the engine from the originating FirmwareRef in
            // real dig_vendor runs (see run_loop) — this fixture's value
            // only matters for tests that pre-populate "prior state"
            // directly via store.upsert, bypassing the engine.
            source_url: "https://example.test/a".parse().unwrap(),
            version: VersionKey {
                raw: version.into(),
                scheme: VersionScheme::Semver,
                ordinal: Some(ordinal),
            },
            release_date: None,
            sha256: Some([sha_byte; 32]),
            signature: None,
            hardware_targets: vec!["isr4331".into()],
            release_notes_url: None,
            display_name: None,
        }
    }

    fn bus_with_collector() -> (EventBus, Arc<Mutex<Vec<FirmwareEvent>>>) {
        let collector = CollectingSubscriber::default();
        let events = collector.events.clone();
        (EventBus::new(vec![Box::new(collector)]), events)
    }

    // ---------------------------------------------------------------
    // Tests
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn baseline_run_persists_everything_but_publishes_no_events() {
        let refs = vec![
            firmware_ref("https://example.test/a"),
            firmware_ref("https://example.test/b"),
        ];
        let mut metadata_by_url = HashMap::new();
        metadata_by_url.insert(
            "https://example.test/a".to_string(),
            Ok(metadata("1.0.0", vec![1, 0, 0], 1)),
        );
        metadata_by_url.insert(
            "https://example.test/b".to_string(),
            Ok(metadata("1.1.0", vec![1, 1, 0], 2)),
        );
        let plugin = MockPlugin {
            refs,
            metadata_by_url,
        };

        let store = MockStore::default();
        let (bus, events) = bus_with_collector();

        dig_vendor(&plugin, &ctx(), &store, &bus)
            .await
            .expect("baseline dig should succeed");

        assert!(events.lock().unwrap().is_empty(), "first-ever dig must be silent — see the README's \"Baseline vs incremental digs\" section");
        assert!(
            store.has_completed_baseline("mockvendor").await.unwrap(),
            "baseline must be marked complete after a fully successful run"
        );
        assert_eq!(
            store.all_current("mockvendor").await.unwrap().len(),
            2,
            "both entries must still be persisted"
        );
        assert_eq!(store.run_outcome_kinds(), vec!["success"]);
    }

    #[tokio::test]
    async fn dig_populates_source_url_from_the_discovering_ref_not_the_plugins_metadata_response() {
        // The metadata() fixture always defaults source_url to
        // ".../a" regardless of which ref it's answering for — so this
        // only passes if the engine actually overwrites it from `r`
        // (matching vendor/device_family's population, see run_loop).
        let refs = vec![
            firmware_ref("https://example.test/a"),
            firmware_ref("https://example.test/b"),
        ];
        let mut metadata_by_url = HashMap::new();
        metadata_by_url.insert(
            "https://example.test/a".to_string(),
            Ok(metadata("1.0.0", vec![1, 0, 0], 1)),
        );
        metadata_by_url.insert(
            "https://example.test/b".to_string(),
            Ok(metadata("2.0.0", vec![2, 0, 0], 2)),
        );
        let plugin = MockPlugin {
            refs,
            metadata_by_url,
        };

        let store = MockStore::default();
        let (bus, _events) = bus_with_collector();
        dig_vendor(&plugin, &ctx(), &store, &bus).await.unwrap();

        let all = store.all_current("mockvendor").await.unwrap();
        let entry_b = all
            .iter()
            .find(|m| m.version.raw == "2.0.0")
            .expect("entry for version 2.0.0 must exist");
        assert_eq!(
            entry_b.source_url.as_str(),
            "https://example.test/b",
            "source_url must reflect the FirmwareRef that actually produced this metadata"
        );
    }

    #[tokio::test]
    async fn incremental_run_emits_new_release_for_a_previously_unseen_entry() {
        let plugin = MockPlugin {
            refs: vec![firmware_ref("https://example.test/a")],
            metadata_by_url: HashMap::from([(
                "https://example.test/a".to_string(),
                Ok(metadata("2.0.0", vec![2, 0, 0], 9)),
            )]),
        };

        let store = MockStore::default();
        store.mark_baseline_complete("mockvendor").await.unwrap(); // simulate a prior baseline run
        let (bus, events) = bus_with_collector();

        dig_vendor(&plugin, &ctx(), &store, &bus)
            .await
            .expect("incremental dig should succeed");

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            FirmwareEvent::NewRelease { firmware, .. } => assert_eq!(firmware.version.raw, "2.0.0"),
            other => panic!("expected NewRelease, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn incremental_run_reports_newer_direction_on_a_real_version_bump() {
        let store = MockStore::default();
        let prior_ref = firmware_ref("https://example.test/a");
        store
            .upsert(
                &prior_ref,
                &metadata("1.0.0", vec![1, 0, 0], 1),
                Uuid::new_v4(),
            )
            .await
            .unwrap();
        store.mark_baseline_complete("mockvendor").await.unwrap();

        let plugin = MockPlugin {
            refs: vec![firmware_ref("https://example.test/a")],
            metadata_by_url: HashMap::from([(
                "https://example.test/a".to_string(),
                Ok(metadata("2.0.0", vec![2, 0, 0], 2)),
            )]),
        };

        let (bus, events) = bus_with_collector();
        dig_vendor(&plugin, &ctx(), &store, &bus).await.unwrap();

        let events = events.lock().unwrap();
        assert_eq!(
            events.len(),
            1,
            "a version bump must produce exactly one event, not both New and Updated"
        );
        match &events[0] {
            FirmwareEvent::UpdatedRelease {
                previous,
                version_direction,
                ..
            } => {
                assert_eq!(previous.version.raw, "1.0.0");
                assert_eq!(*version_direction, VersionDirection::Newer);
            }
            other => panic!("expected UpdatedRelease for a version bump, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn incremental_run_reports_older_direction_on_a_version_rollback() {
        // A lower version number arriving after a higher one was already
        // known — e.g. a vendor pulled a bad release. The README's
        // "Notifications" section calls this out
        // explicitly as a meaningfully different signal from a normal bump.
        let store = MockStore::default();
        let prior_ref = firmware_ref("https://example.test/a");
        store
            .upsert(
                &prior_ref,
                &metadata("2.0.0", vec![2, 0, 0], 1),
                Uuid::new_v4(),
            )
            .await
            .unwrap();
        store.mark_baseline_complete("mockvendor").await.unwrap();

        let plugin = MockPlugin {
            refs: vec![firmware_ref("https://example.test/a")],
            metadata_by_url: HashMap::from([(
                "https://example.test/a".to_string(),
                Ok(metadata("1.0.0", vec![1, 0, 0], 2)),
            )]),
        };

        let (bus, events) = bus_with_collector();
        dig_vendor(&plugin, &ctx(), &store, &bus).await.unwrap();

        let events = events.lock().unwrap();
        match &events[0] {
            FirmwareEvent::UpdatedRelease {
                version_direction, ..
            } => {
                assert_eq!(*version_direction, VersionDirection::Older);
            }
            other => panic!("expected UpdatedRelease, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_new_device_family_is_new_release_even_when_other_device_families_have_history() {
        // latest_known must be scoped to (vendor, device_family, hardware) —
        // an existing version for "widget" must not be treated as the
        // "prior" version for a first-time observation of "gadget".
        let store = MockStore::default();
        let widget_ref = FirmwareRef {
            vendor: "mockvendor".into(),
            device_family: "widget".into(),
            source_url: "https://example.test/widget".parse().unwrap(),
            discovered_at: chrono::Utc::now(),
        };
        let mut widget_meta = metadata("9.9.9", vec![9, 9, 9], 1);
        widget_meta.device_family = "widget".into();
        store
            .upsert(&widget_ref, &widget_meta, Uuid::new_v4())
            .await
            .unwrap();
        store.mark_baseline_complete("mockvendor").await.unwrap();

        let gadget_ref = FirmwareRef {
            vendor: "mockvendor".into(),
            device_family: "gadget".into(),
            source_url: "https://example.test/gadget".parse().unwrap(),
            discovered_at: chrono::Utc::now(),
        };
        let mut gadget_meta = metadata("1.0.0", vec![1, 0, 0], 2);
        gadget_meta.device_family = "gadget".into();

        let plugin = MockPlugin {
            refs: vec![gadget_ref],
            metadata_by_url: HashMap::from([(
                "https://example.test/gadget".to_string(),
                Ok(gadget_meta),
            )]),
        };

        let (bus, events) = bus_with_collector();
        dig_vendor(&plugin, &ctx(), &store, &bus).await.unwrap();

        let events = events.lock().unwrap();
        match &events[0] {
            FirmwareEvent::NewRelease { firmware, .. } => assert_eq!(firmware.device_family, "gadget"),
            other => panic!("a first-ever version for a different device family must be NewRelease, got {other:?}"),
        }
    }

    /// A plugin that discovers `releases` in the given order, one URL each.
    fn plugin_returning(releases: Vec<FirmwareMetadata>) -> MockPlugin {
        let mut refs = Vec::new();
        let mut metadata_by_url = HashMap::new();
        for (i, m) in releases.into_iter().enumerate() {
            let url = format!("https://example.test/r{i}");
            refs.push(firmware_ref(&url));
            metadata_by_url.insert(url, Ok(m));
        }
        MockPlugin {
            refs,
            metadata_by_url,
        }
    }

    /// A store whose baseline is complete and which knows `known` already.
    async fn store_after_baseline(known: Vec<FirmwareMetadata>) -> MockStore {
        let store = MockStore::default();
        for m in known {
            store
                .upsert(
                    &firmware_ref("https://example.test/prior"),
                    &m,
                    Uuid::new_v4(),
                )
                .await
                .unwrap();
        }
        store.mark_baseline_complete("mockvendor").await.unwrap();
        store
    }

    /// Each event as `new <version>` or `<previous> -> <version> <direction>`.
    fn describe(events: &[FirmwareEvent]) -> Vec<String> {
        events
            .iter()
            .map(|e| match e {
                FirmwareEvent::NewRelease { firmware, .. } => {
                    format!("new {}", firmware.version.raw)
                }
                FirmwareEvent::UpdatedRelease {
                    firmware,
                    previous,
                    version_direction,
                    ..
                } => format!(
                    "{} -> {} {version_direction:?}",
                    previous.version.raw, firmware.version.raw
                ),
            })
            .collect()
    }

    /// Digs `fresh` (one release) over a store already holding `known`,
    /// with `policy`, and returns each event's changed field names.
    async fn changed_fields_reported(
        policy: crate::events::ChangePolicy,
        known: FirmwareMetadata,
        fresh: FirmwareMetadata,
    ) -> Vec<Vec<&'static str>> {
        let store = store_after_baseline(vec![known]).await;
        let collector = CollectingSubscriber::default();
        let events = collector.events.clone();
        let bus = EventBus::new(vec![Box::new(collector)]).with_change_policy(policy);
        dig_vendor(&plugin_returning(vec![fresh]), &ctx(), &store, &bus)
            .await
            .unwrap();
        let events = events.lock().unwrap();
        events
            .iter()
            .map(|e| match e {
                FirmwareEvent::UpdatedRelease {
                    changed_fields,
                    version_direction,
                    ..
                } => {
                    assert_eq!(*version_direction, VersionDirection::Unordered);
                    changed_fields.iter().map(|d| d.field).collect()
                }
                other => panic!("expected UpdatedRelease, got {other:?}"),
            })
            .collect()
    }

    fn with_notes(mut m: FirmwareMetadata, url: &str) -> FirmwareMetadata {
        m.release_notes_url = Some(url.parse().unwrap());
        m
    }

    #[tokio::test]
    async fn by_default_only_a_new_hash_under_the_same_version_is_reported() {
        use crate::events::ChangePolicy;
        let known = with_notes(
            metadata("1.0.0", vec![1, 0, 0], 1),
            "https://example.test/a",
        );

        let notes_only = with_notes(
            metadata("1.0.0", vec![1, 0, 0], 1),
            "https://example.test/b",
        );
        assert!(
            changed_fields_reported(ChangePolicy::default(), known.clone(), notes_only)
                .await
                .is_empty()
        );

        let rebuilt = with_notes(
            metadata("1.0.0", vec![1, 0, 0], 2),
            "https://example.test/b",
        );
        // Only the watched field is listed, though the notes changed too.
        assert_eq!(
            changed_fields_reported(ChangePolicy::default(), known, rebuilt).await,
            [["sha256"]]
        );
    }

    #[tokio::test]
    async fn watched_fields_are_reported_and_unwatched_ones_are_not() {
        use crate::events::{ChangePolicy, WatchedField};
        let known = with_notes(
            metadata("1.0.0", vec![1, 0, 0], 1),
            "https://example.test/a",
        );
        let notes_policy = || ChangePolicy::new([WatchedField::ReleaseNotesUrl]);

        let notes_only = with_notes(
            metadata("1.0.0", vec![1, 0, 0], 1),
            "https://example.test/b",
        );
        assert_eq!(
            changed_fields_reported(notes_policy(), known.clone(), notes_only).await,
            [["release_notes_url"]]
        );

        // A new hash isn't reported when sha256 isn't watched.
        let rebuilt = with_notes(
            metadata("1.0.0", vec![1, 0, 0], 2),
            "https://example.test/a",
        );
        assert!(
            changed_fields_reported(notes_policy(), known.clone(), rebuilt)
                .await
                .is_empty()
        );

        // Several watched fields changing make one event listing each, in
        // the order they are watched.
        let mut both = with_notes(
            metadata("1.0.0", vec![1, 0, 0], 2),
            "https://example.test/b",
        );
        both.release_date = chrono::NaiveDate::from_ymd_opt(2026, 1, 2);
        let policy = ChangePolicy::new([
            WatchedField::ReleaseDate,
            WatchedField::Sha256,
            WatchedField::ReleaseNotesUrl,
        ]);
        assert_eq!(
            changed_fields_reported(policy, known, both).await,
            [["release_date", "sha256", "release_notes_url"]]
        );
    }

    #[tokio::test]
    async fn watching_nothing_still_reports_new_versions() {
        use crate::events::ChangePolicy;
        let store = store_after_baseline(vec![metadata("1.0.0", vec![1, 0, 0], 1)]).await;
        let collector = CollectingSubscriber::default();
        let events = collector.events.clone();
        let bus =
            EventBus::new(vec![Box::new(collector)]).with_change_policy(ChangePolicy::new([]));
        let plugin = plugin_returning(vec![
            metadata("1.0.0", vec![1, 0, 0], 9), // rebuilt: not watched
            metadata("1.1.0", vec![1, 1, 0], 2), // new version: always reported
        ]);
        dig_vendor(&plugin, &ctx(), &store, &bus).await.unwrap();
        assert_eq!(describe(&events.lock().unwrap()), ["1.0.0 -> 1.1.0 Newer"]);
    }

    #[tokio::test]
    async fn a_line_first_seen_after_the_baseline_reports_every_release_as_new_in_any_order() {
        // A device that wasn't tracked at the baseline, such as a newly
        // enabled product: its history is new to delve, not a series of
        // updates and downgrades, whatever order the vendor lists it in.
        for order in [[1u64, 3, 2], [3, 2, 1], [1, 2, 3]] {
            let store = store_after_baseline(vec![]).await;
            let plugin = plugin_returning(
                order
                    .iter()
                    .map(|&v| metadata(&format!("{v}.0.0"), vec![v, 0, 0], v as u8))
                    .collect(),
            );
            let (bus, events) = bus_with_collector();
            dig_vendor(&plugin, &ctx(), &store, &bus).await.unwrap();

            let expected: Vec<String> = order.iter().map(|v| format!("new {v}.0.0")).collect();
            assert_eq!(
                describe(&events.lock().unwrap()),
                expected,
                "order {order:?}"
            );
        }
    }

    #[tokio::test]
    async fn several_new_versions_of_a_known_line_are_each_compared_with_what_was_known_before_the_dig(
    ) {
        let store = store_after_baseline(vec![metadata("2.0.0", vec![2, 0, 0], 2)]).await;
        // 3.0.0 is newer than anything known; 2.5.0 and 1.5.0 arrive after it
        // in the same dig and must not be reported as downgrades from 3.0.0.
        let plugin = plugin_returning(vec![
            metadata("3.0.0", vec![3, 0, 0], 3),
            metadata("2.5.0", vec![2, 5, 0], 4),
            metadata("1.5.0", vec![1, 5, 0], 5),
        ]);
        let (bus, events) = bus_with_collector();
        dig_vendor(&plugin, &ctx(), &store, &bus).await.unwrap();

        assert_eq!(
            describe(&events.lock().unwrap()),
            [
                "2.0.0 -> 3.0.0 Newer",
                "2.0.0 -> 2.5.0 Newer",
                // Older than what was known before the dig: a version the
                // vendor published below the latest, which is what Older is for.
                "2.0.0 -> 1.5.0 Older",
            ]
        );
    }

    #[tokio::test]
    async fn a_known_line_and_a_new_line_in_one_dig_do_not_affect_each_other() {
        let store = store_after_baseline(vec![metadata("1.0.0", vec![1, 0, 0], 1)]).await;
        let mut other = metadata("5.0.0", vec![5, 0, 0], 9);
        other.hardware_targets = vec!["isr4351".into()];
        let mut other_older = metadata("4.0.0", vec![4, 0, 0], 8);
        other_older.hardware_targets = vec!["isr4351".into()];

        let plugin = plugin_returning(vec![
            other,
            metadata("1.1.0", vec![1, 1, 0], 2),
            other_older,
        ]);
        let (bus, events) = bus_with_collector();
        dig_vendor(&plugin, &ctx(), &store, &bus).await.unwrap();

        assert_eq!(
            describe(&events.lock().unwrap()),
            ["new 5.0.0", "1.0.0 -> 1.1.0 Newer", "new 4.0.0"]
        );
    }

    #[tokio::test]
    async fn a_second_dig_compares_with_everything_the_first_one_stored() {
        // The pre-dig state is per dig: what one dig stores is "known" to the next.
        let store = store_after_baseline(vec![]).await;
        let (bus, _) = bus_with_collector();
        dig_vendor(
            &plugin_returning(vec![metadata("1.0.0", vec![1, 0, 0], 1)]),
            &ctx(),
            &store,
            &bus,
        )
        .await
        .unwrap();

        let (bus, events) = bus_with_collector();
        dig_vendor(
            &plugin_returning(vec![
                metadata("1.0.0", vec![1, 0, 0], 1),
                metadata("1.1.0", vec![1, 1, 0], 2),
            ]),
            &ctx(),
            &store,
            &bus,
        )
        .await
        .unwrap();
        assert_eq!(describe(&events.lock().unwrap()), ["1.0.0 -> 1.1.0 Newer"]);
    }

    #[tokio::test]
    async fn incremental_run_emits_updated_release_when_hash_changes_under_the_same_version() {
        let store = MockStore::default();
        // Pre-populate "prior state" directly, bypassing the engine —
        // simulates a version that was already seen on an earlier dig.
        let prior_ref = firmware_ref("https://example.test/a");
        let prior_meta = metadata("1.0.0", vec![1, 0, 0], 0xAA);
        store
            .upsert(&prior_ref, &prior_meta, Uuid::new_v4())
            .await
            .unwrap();
        store.mark_baseline_complete("mockvendor").await.unwrap();

        // Same version string, different hash — e.g. a vendor silently
        // re-published the same version number with different bytes.
        let fresh_meta = metadata("1.0.0", vec![1, 0, 0], 0xBB);
        let plugin = MockPlugin {
            refs: vec![firmware_ref("https://example.test/a")],
            metadata_by_url: HashMap::from([(
                "https://example.test/a".to_string(),
                Ok(fresh_meta),
            )]),
        };

        let (bus, events) = bus_with_collector();
        dig_vendor(&plugin, &ctx(), &store, &bus)
            .await
            .expect("incremental dig should succeed");

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            FirmwareEvent::UpdatedRelease {
                changed_fields,
                version_direction,
                ..
            } => {
                assert!(
                    changed_fields.iter().any(|f| f.field == "sha256"),
                    "hash change must appear in changed_fields even though version string didn't change"
                );
                assert_eq!(
                    *version_direction,
                    VersionDirection::Unordered,
                    "equal version ordinals must report Unordered, not a guessed direction"
                );
            }
            other => panic!("expected UpdatedRelease, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn incremental_run_publishes_nothing_when_nothing_changed() {
        let store = MockStore::default();
        let r = firmware_ref("https://example.test/a");
        let meta = metadata("1.0.0", vec![1, 0, 0], 1);
        store.upsert(&r, &meta, Uuid::new_v4()).await.unwrap();
        store.mark_baseline_complete("mockvendor").await.unwrap();

        // Plugin reports back the exact same observation.
        let plugin = MockPlugin {
            refs: vec![r.clone()],
            metadata_by_url: HashMap::from([("https://example.test/a".to_string(), Ok(meta))]),
        };

        let (bus, events) = bus_with_collector();
        dig_vendor(&plugin, &ctx(), &store, &bus).await.unwrap();

        assert!(
            events.lock().unwrap().is_empty(),
            "no diff means no event, even on an incremental run"
        );
    }

    #[tokio::test]
    async fn a_dig_that_fails_partway_does_not_mark_the_baseline_complete() {
        // Two refs: the first metadata() call succeeds, the second fails —
        // this is the "died mid-scrape" scenario the README's "Baseline vs
        // incremental digs" section warns about. The
        // baseline flag must stay unset so the *next* dig is still treated
        // as a baseline, not an incremental run that would notify on
        // everything the failed run didn't get to.
        let plugin = MockPlugin {
            refs: vec![
                firmware_ref("https://example.test/a"),
                firmware_ref("https://example.test/b"),
            ],
            metadata_by_url: HashMap::from([
                (
                    "https://example.test/a".to_string(),
                    Ok(metadata("1.0.0", vec![1, 0, 0], 1)),
                ),
                (
                    "https://example.test/b".to_string(),
                    Err("simulated vendor site failure".to_string()),
                ),
            ]),
        };

        let store = MockStore::default();
        let (bus, events) = bus_with_collector();

        let result = dig_vendor(&plugin, &ctx(), &store, &bus).await;

        assert!(
            result.is_err(),
            "the dig as a whole must surface the plugin error"
        );
        assert!(
            !store.has_completed_baseline("mockvendor").await.unwrap(),
            "a baseline run that dies mid-scrape must NOT be marked complete — see the README's \"Baseline vs incremental digs\" correctness notes"
        );
        assert!(
            events.lock().unwrap().is_empty(),
            "a failed baseline run must not have published anything either"
        );
        assert_eq!(
            store.run_outcome_kinds(),
            vec!["failed"],
            "complete_run must be called with Failed, not silently dropped"
        );
        // What was successfully fetched before the failure is still kept —
        // upsert isn't rolled back, only the baseline flag is withheld.
        assert_eq!(store.all_current("mockvendor").await.unwrap().len(), 1);
    }

    /// Records the order of `notify` and `flush` calls.
    struct OrderRecorder(Arc<Mutex<Vec<&'static str>>>);

    #[async_trait]
    impl Subscriber for OrderRecorder {
        fn id(&self) -> &'static str {
            "test-order"
        }

        async fn notify(&self, _event: &FirmwareEvent) -> Result<(), SubscriberError> {
            self.0.lock().unwrap().push("notify");
            Ok(())
        }

        async fn flush(&self) -> Result<(), SubscriberError> {
            self.0.lock().unwrap().push("flush");
            Ok(())
        }
    }

    #[tokio::test]
    async fn subscribers_are_flushed_once_after_the_last_event_of_a_dig() {
        let store = MockStore::default();
        store.mark_baseline_complete("mockvendor").await.unwrap(); // so events are published
        let plugin = MockPlugin {
            refs: vec![
                firmware_ref("https://example.test/a"),
                firmware_ref("https://example.test/b"),
            ],
            metadata_by_url: HashMap::from([
                (
                    "https://example.test/a".to_string(),
                    Ok(metadata("1.0.0", vec![1, 0, 0], 1)),
                ),
                (
                    "https://example.test/b".to_string(),
                    Ok(metadata("2.0.0", vec![2, 0, 0], 2)),
                ),
            ]),
        };
        let order = Arc::new(Mutex::new(Vec::new()));
        let bus = EventBus::new(vec![Box::new(OrderRecorder(order.clone()))]);

        dig_vendor(&plugin, &ctx(), &store, &bus).await.unwrap();

        // Two releases, then exactly one flush, after both.
        assert_eq!(*order.lock().unwrap(), ["notify", "notify", "flush"]);
    }

    #[tokio::test]
    async fn subscribers_are_flushed_even_when_the_dig_fails_partway() {
        // The first release is published, then the second fails to load: what
        // a batching subscriber holds must still be sent.
        let store = MockStore::default();
        store.mark_baseline_complete("mockvendor").await.unwrap();
        let plugin = MockPlugin {
            refs: vec![
                firmware_ref("https://example.test/a"),
                firmware_ref("https://example.test/b"),
            ],
            metadata_by_url: HashMap::from([
                (
                    "https://example.test/a".to_string(),
                    Ok(metadata("1.0.0", vec![1, 0, 0], 1)),
                ),
                (
                    "https://example.test/b".to_string(),
                    Err("simulated vendor site failure".to_string()),
                ),
            ]),
        };
        let order = Arc::new(Mutex::new(Vec::new()));
        let bus = EventBus::new(vec![Box::new(OrderRecorder(order.clone()))]);

        let result = dig_vendor(&plugin, &ctx(), &store, &bus).await;

        assert!(result.is_err());
        assert_eq!(*order.lock().unwrap(), ["notify", "flush"]);
    }

    #[tokio::test]
    async fn a_baseline_dig_publishes_nothing_but_still_flushes() {
        let store = MockStore::default();
        let plugin = MockPlugin {
            refs: vec![firmware_ref("https://example.test/a")],
            metadata_by_url: HashMap::from([(
                "https://example.test/a".to_string(),
                Ok(metadata("1.0.0", vec![1, 0, 0], 1)),
            )]),
        };
        let order = Arc::new(Mutex::new(Vec::new()));
        let bus = EventBus::new(vec![Box::new(OrderRecorder(order.clone()))]);

        dig_vendor(&plugin, &ctx(), &store, &bus).await.unwrap();

        assert_eq!(*order.lock().unwrap(), ["flush"]);
    }

    #[tokio::test]
    async fn redig_clears_the_baseline_so_the_next_dig_is_silent_again() {
        let store = MockStore::default();
        store.mark_baseline_complete("mockvendor").await.unwrap();

        // Without clearing, this would be an incremental run and would notify.
        store.clear_baseline("mockvendor").await.unwrap();

        let plugin = MockPlugin {
            refs: vec![firmware_ref("https://example.test/a")],
            metadata_by_url: HashMap::from([(
                "https://example.test/a".to_string(),
                Ok(metadata("1.0.0", vec![1, 0, 0], 1)),
            )]),
        };
        let (bus, events) = bus_with_collector();

        dig_vendor(&plugin, &ctx(), &store, &bus).await.unwrap();

        assert!(
            events.lock().unwrap().is_empty(),
            "--redig must make the next dig silent, same as a first-ever dig"
        );
        assert!(store.has_completed_baseline("mockvendor").await.unwrap());
    }

    #[tokio::test]
    async fn diff_fields_reports_both_version_and_hash_when_both_change() {
        let old = metadata("1.0.0", vec![1, 0, 0], 1);
        let fresh = metadata("1.1.0", vec![1, 1, 0], 2);
        let diffs = diff_fields(&old, &fresh);
        let fields: Vec<_> = diffs.iter().map(|d| d.field).collect();
        assert!(fields.contains(&"version"));
        assert!(fields.contains(&"sha256"));
    }

    #[tokio::test]
    async fn diff_fields_reports_nothing_when_only_release_notes_url_changes() {
        // Matches the README's "Notifications" section's default "what counts
        // as changed" policy: only
        // hash-or-version is notification-worthy by default.
        let mut old = metadata("1.0.0", vec![1, 0, 0], 1);
        let mut fresh = old.clone();
        old.release_notes_url = Some("https://example.test/notes-old".parse().unwrap());
        fresh.release_notes_url = Some("https://example.test/notes-new".parse().unwrap());
        assert!(diff_fields(&old, &fresh).is_empty());
    }
}
