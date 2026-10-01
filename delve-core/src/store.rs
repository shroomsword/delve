//! `MetadataStore` trait. See the README's "Storage" section for the
//! full rationale. The SQLite implementation
//! lives in the separate `delve-store-sqlite` crate so `delve-core` doesn't
//! pull in `sqlx` — this trait is the only thing the engine and CLI depend
//! on directly.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::model::{FirmwareMetadata, FirmwareRef};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunKind {
    /// First-ever (or `--redig`'d) run for a vendor. Persists all current
    /// data, suppresses notifications entirely — see the README's
    /// "Baseline vs incremental digs" section.
    Baseline,
    Incremental,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RunOutcome {
    Success,
    PartialFailure { error_count: usize },
    Failed(String),
}

/// One historical observation of a firmware entry, as returned by
/// `history` / surfaced via the `provenance` command — see the README's
/// "CLI commands" section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirmwareRevision {
    pub metadata: FirmwareMetadata,
    pub observed_at: DateTime<Utc>,
    pub run_id: Uuid,
}

/// Selector used by `catalog` / `provenance` / `unearth` to resolve one or
/// more stored entries — either the natural key (vendor + device family +
/// hardware + version, with `latest` as a version shorthand) or the
/// surrogate `id` (see the README's "Storage" section). `provenance`
/// requires enough specificity to
/// resolve to exactly one entry; `catalog` allows partial selectors that
/// match many.
#[derive(Debug, Clone, Default)]
pub struct FirmwareSelector {
    pub id: Option<Uuid>,
    pub vendor: Option<String>,
    pub device_family: Option<String>,
    pub hardware: Option<Vec<String>>,
    pub version: Option<String>,
    pub latest: bool,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Backend(String),
    #[error("selector did not resolve to a stored entry")]
    NotFound,
    #[error("selector was ambiguous: matched {0} entries, expected exactly one")]
    Ambiguous(usize),
}

/// The natural identity key (see the README's "Data model and identity
/// keys" section) — vendor, device family, hardware
/// targets, and version. `lookup`/`history` take this rather than a bare
/// `FirmwareRef`, because a `FirmwareRef` alone (vendor + device_family +
/// source_url) is discovered *before* a version is known; the engine only
/// has a full key once `VendorPlugin::metadata` has returned, so it builds
/// this from the freshly-observed `FirmwareMetadata` plus the originating
/// `FirmwareRef`.
pub struct FirmwareKey<'a> {
    pub vendor: &'a str,
    pub device_family: &'a str,
    pub hardware_targets: &'a [String],
    pub version_raw: &'a str,
}

impl<'a> FirmwareKey<'a> {
    pub fn from_ref_and_metadata(r: &'a FirmwareRef, meta: &'a FirmwareMetadata) -> Self {
        Self {
            vendor: &r.vendor,
            device_family: &r.device_family,
            hardware_targets: &meta.hardware_targets,
            version_raw: &meta.version.raw,
        }
    }

    /// Now that `FirmwareMetadata` carries `vendor`/`device_family` itself
    /// (see its doc comment in model.rs), a resolved entry is enough on its
    /// own to build a key — no separate `FirmwareRef` needed. This is what
    /// unblocks `provenance --id`/`unearth --id`: resolve via
    /// `MetadataStore::resolve_one`, then build the key straight from what
    /// came back.
    pub fn from_metadata(meta: &'a FirmwareMetadata) -> Self {
        Self {
            vendor: &meta.vendor,
            device_family: &meta.device_family,
            hardware_targets: &meta.hardware_targets,
            version_raw: &meta.version.raw,
        }
    }
}

/// A stored entry together with its surrogate id — the value `--id` takes
/// on `unearth`/`provenance`, and what `catalog` prints so it can be
/// copy-pasted.
#[derive(Debug, Clone)]
pub struct StoredFirmware {
    pub id: Uuid,
    pub metadata: FirmwareMetadata,
}

#[async_trait]
pub trait MetadataStore: Send + Sync {
    // --- diff-loop path, called once per FirmwareRef during a dig ---

    /// Exact match on the full identity key (vendor, device family,
    /// hardware, version — see the README's "Data model and identity keys"
    /// section), version included —
    /// answers "have we already stored precisely this version." This alone
    /// can't detect a version bump: a bump always misses here, since the
    /// version being looked up is the one that just arrived, which by
    /// definition has never been stored under that exact key before. Used
    /// to catch the narrower case of the same version being re-observed
    /// with a changed hash (see `latest_known` for the version-bump case).
    async fn lookup(&self, key: &FirmwareKey<'_>) -> Result<Option<FirmwareMetadata>, StoreError>;

    /// The highest-precedence version already known for this
    /// vendor/device_family/hardware combination, independent of whatever
    /// version just arrived — this is what makes detecting a version bump
    /// (and its `VersionDirection`) possible at all, since `lookup`'s exact
    /// key can never match a version it hasn't seen before. Returns `None`
    /// when this is the first version ever observed for this line, or when
    /// nothing stored has a derivable ordinal to compare against.
    async fn latest_known(
        &self,
        vendor: &str,
        device_family: &str,
        hardware_targets: &[String],
    ) -> Result<Option<FirmwareMetadata>, StoreError>;

    async fn upsert(
        &self,
        r: &FirmwareRef,
        meta: &FirmwareMetadata,
        run_id: Uuid,
    ) -> Result<(), StoreError>;

    // --- baseline tracking, per vendor ---
    async fn has_completed_baseline(&self, vendor_id: &str) -> Result<bool, StoreError>;
    async fn mark_baseline_complete(&self, vendor_id: &str) -> Result<(), StoreError>;
    /// Backs `--redig` (see the README's "Baseline vs incremental digs"
    /// and "CLI commands" sections): clears the flag so the vendor's next
    /// dig is treated as a fresh baseline.
    async fn clear_baseline(&self, vendor_id: &str) -> Result<(), StoreError>;

    // --- run bookkeeping ---
    async fn start_run(&self, vendor_id: &str, kind: RunKind) -> Result<Uuid, StoreError>;
    /// Only after this resolves for a `Baseline` run should the caller call
    /// `mark_baseline_complete` — see the correctness notes in the README's
    /// "Baseline vs incremental digs" section about not
    /// marking a baseline complete on a run that died mid-scrape.
    async fn complete_run(&self, run_id: Uuid, outcome: RunOutcome) -> Result<(), StoreError>;

    // --- history / reporting, not in the hot diff-loop path ---
    async fn history(&self, key: &FirmwareKey<'_>) -> Result<Vec<FirmwareRevision>, StoreError>;
    async fn all_current(&self, vendor_id: &str) -> Result<Vec<FirmwareMetadata>, StoreError>;

    // --- CLI addressing (catalog / unearth / provenance) ---
    async fn resolve_one(
        &self,
        selector: &FirmwareSelector,
    ) -> Result<Option<StoredFirmware>, StoreError>;
    async fn resolve_many(
        &self,
        selector: &FirmwareSelector,
    ) -> Result<Vec<StoredFirmware>, StoreError>;
}
