//! SQLite implementation of `delve_core::store::MetadataStore`. See the
//! README's "Storage" section for the schema rationale and the
//! concurrency notes (WAL mode, connection pooling).
//!
//! Deliberately kept in its own crate rather than folded into `delve-core`
//! so the core crate doesn't pull in `sqlx` — anything implementing
//! `MetadataStore` against a different backend later only needs to depend
//! on `delve-core`, not this crate.
//!
//! Uses runtime `sqlx::query` (not the `query!` macro) throughout, since
//! the macro requires a live database connection at compile time via
//! `DATABASE_URL` / `sqlx-cli prepare` — deferring that setup until this
//! scaffold is built out further.

use async_trait::async_trait;
use chrono::Utc;
use delve_core::model::{hardware_key, FirmwareMetadata, FirmwareRef, VersionScheme};
use delve_core::store::{
    FirmwareKey, FirmwareRevision, FirmwareSelector, MetadataStore, RunKind, RunOutcome, StoreError,
};
use sqlx::{sqlite::SqlitePoolOptions, QueryBuilder, Row, Sqlite, SqlitePool};
use uuid::Uuid;

pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Opens (creating if needed) the SQLite file at `path` and runs
    /// migrations. `busy_timeout` and WAL mode are set here (see the
    /// README's "Storage" section) so
    /// two vendor digs can run concurrently against one pool without
    /// `SQLITE_BUSY` errors under normal contention.
    /// Opens (creating if needed) the SQLite file at `path` and runs
    /// migrations. `busy_timeout` and WAL mode are set here (see the
    /// README's "Storage" section) so two vendor digs can run
    /// concurrently against one pool without `SQLITE_BUSY` errors under
    /// normal contention.
    ///
    /// SQLite's `mode=rwc` (below) creates the database *file* if it's
    /// missing, but it does not create missing parent directories — on a
    /// genuinely first run, `~/.config/delve/` (or wherever `database_path`
    /// points) usually doesn't exist yet, since nothing else creates it
    /// either. Without this, opening the file fails with SQLite error 14
    /// ("unable to open database file"), which is a confusing thing to hit
    /// on a first run through no fault of the user's.
    pub async fn open(path: &str) -> Result<Self, StoreError> {
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    StoreError::Backend(format!(
                        "failed to create database directory {}: {e}",
                        parent.display()
                    ))
                })?;
            }
        }
        Self::open_url(&format!("sqlite://{path}?mode=rwc")).await
    }

    /// Opens a store from a raw SQLite connection URL rather than a bare
    /// file path — lets tests use `sqlite::memory:` directly without
    /// `open`'s `?mode=rwc` wrapping, which isn't meaningful for an
    /// in-memory database.
    pub async fn open_url(url: &str) -> Result<Self, StoreError> {
        // An in-memory database only has one connection's worth of data —
        // pooling multiple connections against `:memory:` would silently
        // hand out independent, empty databases per connection. Only
        // relevant for tests (see the test module below); real `open()`
        // callers always get the full pool.
        let max_connections = if url.contains(":memory:") { 1 } else { 8 };
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect(url)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;

        sqlx::query("PRAGMA journal_mode = WAL;")
            .execute(&pool)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        sqlx::query("PRAGMA busy_timeout = 5000;")
            .execute(&pool)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;

        Ok(Self { pool })
    }

    fn version_scheme_str(scheme: &VersionScheme) -> &'static str {
        match scheme {
            VersionScheme::Semver => "semver",
            VersionScheme::VendorNumeric => "vendor_numeric",
            VersionScheme::Opaque => "opaque",
        }
    }

    /// Encodes a version ordinal as a zero-padded, dot-joined string so
    /// `ORDER BY version_ordinal` in SQL sorts correctly without pulling
    /// rows into Rust first (see the README's "Storage" section).
    fn encode_ordinal(ordinal: &Option<Vec<u64>>) -> Option<String> {
        ordinal.as_ref().map(|parts| {
            parts
                .iter()
                .map(|n| format!("{:020}", n))
                .collect::<Vec<_>>()
                .join(".")
        })
    }

    fn row_to_metadata(row: &sqlx::sqlite::SqliteRow) -> Result<FirmwareMetadata, StoreError> {
        let json: String = row
            .try_get("metadata_json")
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        serde_json::from_str(&json).map_err(|e| StoreError::Backend(e.to_string()))
    }
}

#[async_trait]
impl MetadataStore for SqliteStore {
    async fn lookup(&self, key: &FirmwareKey<'_>) -> Result<Option<FirmwareMetadata>, StoreError> {
        let hw_key = hardware_key(key.hardware_targets);
        let row = sqlx::query(
            "SELECT metadata_json FROM firmware_current
             WHERE vendor = ? AND device_family = ? AND hardware_key = ? AND version_raw = ?",
        )
        .bind(key.vendor)
        .bind(key.device_family)
        .bind(&hw_key)
        .bind(key.version_raw)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;

        row.as_ref().map(Self::row_to_metadata).transpose()
    }

    async fn latest_known(
        &self,
        vendor: &str,
        device_family: &str,
        hardware_targets: &[String],
    ) -> Result<Option<FirmwareMetadata>, StoreError> {
        let hw_key = hardware_key(hardware_targets);
        // version_ordinal DESC: SQLite sorts NULLs last in a DESC ordering,
        // so any row with a derivable ordinal is preferred over one without
        // (Opaque scheme) — matching the refusal to compare Opaque
        // versions against anything. The `rowid DESC` tiebreak is a
        // best-effort "most recently inserted" fallback for the case where
        // *no* matching row has an ordinal at all; it's insertion order,
        // not a real timestamp, and only ever matters when every candidate
        // is Opaque anyway — VersionDirection::between will still report
        // Unordered for those regardless of which one this picks.
        let row = sqlx::query(
            "SELECT metadata_json FROM firmware_current
             WHERE vendor = ? AND device_family = ? AND hardware_key = ?
             ORDER BY version_ordinal DESC, rowid DESC
             LIMIT 1",
        )
        .bind(vendor)
        .bind(device_family)
        .bind(&hw_key)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;

        row.as_ref().map(Self::row_to_metadata).transpose()
    }

    async fn upsert(
        &self,
        r: &FirmwareRef,
        meta: &FirmwareMetadata,
        run_id: Uuid,
    ) -> Result<(), StoreError> {
        let hw_key = hardware_key(&meta.hardware_targets);
        let metadata_json =
            serde_json::to_string(meta).map_err(|e| StoreError::Backend(e.to_string()))?;
        let ordinal = Self::encode_ordinal(&meta.version.ordinal);
        let scheme = Self::version_scheme_str(&meta.version.scheme);
        let observed_at = Utc::now();

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;

        // Revision log — always appended, never overwritten (see the
        // README's "Storage" section).
        sqlx::query(
            "INSERT INTO firmware_revisions
                (vendor, device_family, hardware_key, version_raw, run_id, observed_at, metadata_json)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&r.vendor)
        .bind(&r.device_family)
        .bind(&hw_key)
        .bind(&meta.version.raw)
        .bind(run_id.to_string())
        .bind(observed_at.to_rfc3339())
        .bind(&metadata_json)
        .execute(&mut *tx)
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;

        // Current-state upsert, in the same transaction so current and
        // history can't desync on a crash mid-write.
        sqlx::query(
            "INSERT INTO firmware_current
                (id, vendor, device_family, hardware_key, version_raw, version_scheme,
                 version_ordinal, source_url, sha256, release_date, metadata_json,
                 first_seen_run_id, last_seen_run_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (vendor, device_family, hardware_key, version_raw)
             DO UPDATE SET
                sha256 = excluded.sha256,
                release_date = excluded.release_date,
                metadata_json = excluded.metadata_json,
                last_seen_run_id = excluded.last_seen_run_id",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&r.vendor)
        .bind(&r.device_family)
        .bind(&hw_key)
        .bind(&meta.version.raw)
        .bind(scheme)
        .bind(&ordinal)
        .bind(r.source_url.as_str())
        .bind(meta.sha256.as_ref().map(|h| h.to_vec()))
        .bind(meta.release_date.map(|d| d.to_string()))
        .bind(&metadata_json)
        .bind(run_id.to_string())
        .bind(run_id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn has_completed_baseline(&self, vendor_id: &str) -> Result<bool, StoreError> {
        let row = sqlx::query("SELECT 1 FROM baselines WHERE vendor = ?")
            .bind(vendor_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(row.is_some())
    }

    async fn mark_baseline_complete(&self, vendor_id: &str) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO baselines (vendor, completed_at) VALUES (?, ?)
             ON CONFLICT (vendor) DO UPDATE SET completed_at = excluded.completed_at",
        )
        .bind(vendor_id)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn clear_baseline(&self, vendor_id: &str) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM baselines WHERE vendor = ?")
            .bind(vendor_id)
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn start_run(&self, vendor_id: &str, kind: RunKind) -> Result<Uuid, StoreError> {
        let run_id = Uuid::new_v4();
        let kind_str = match kind {
            RunKind::Baseline => "baseline",
            RunKind::Incremental => "incremental",
        };
        sqlx::query("INSERT INTO runs (id, vendor, kind, started_at) VALUES (?, ?, ?, ?)")
            .bind(run_id.to_string())
            .bind(vendor_id)
            .bind(kind_str)
            .bind(Utc::now().to_rfc3339())
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(run_id)
    }

    async fn complete_run(&self, run_id: Uuid, outcome: RunOutcome) -> Result<(), StoreError> {
        let outcome_str = match outcome {
            RunOutcome::Success => "success".to_string(),
            RunOutcome::PartialFailure { error_count } => format!("partial_failure:{error_count}"),
            RunOutcome::Failed(msg) => format!("failed:{msg}"),
        };
        sqlx::query("UPDATE runs SET completed_at = ?, outcome = ? WHERE id = ?")
            .bind(Utc::now().to_rfc3339())
            .bind(outcome_str)
            .bind(run_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn history(&self, key: &FirmwareKey<'_>) -> Result<Vec<FirmwareRevision>, StoreError> {
        let hw_key = hardware_key(key.hardware_targets);
        let rows = sqlx::query(
            "SELECT metadata_json, observed_at, run_id FROM firmware_revisions
             WHERE vendor = ? AND device_family = ? AND hardware_key = ? AND version_raw = ?
             ORDER BY observed_at ASC",
        )
        .bind(key.vendor)
        .bind(key.device_family)
        .bind(&hw_key)
        .bind(key.version_raw)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;

        rows.iter()
            .map(|row| {
                let metadata = Self::row_to_metadata(row)?;
                let observed_at: String = row
                    .try_get("observed_at")
                    .map_err(|e| StoreError::Backend(e.to_string()))?;
                let run_id: String = row
                    .try_get("run_id")
                    .map_err(|e| StoreError::Backend(e.to_string()))?;
                Ok(FirmwareRevision {
                    metadata,
                    observed_at: chrono::DateTime::parse_from_rfc3339(&observed_at)
                        .map_err(|e| StoreError::Backend(e.to_string()))?
                        .with_timezone(&Utc),
                    run_id: Uuid::parse_str(&run_id)
                        .map_err(|e| StoreError::Backend(e.to_string()))?,
                })
            })
            .collect()
    }

    async fn all_current(&self, vendor_id: &str) -> Result<Vec<FirmwareMetadata>, StoreError> {
        let rows = sqlx::query("SELECT metadata_json FROM firmware_current WHERE vendor = ?")
            .bind(vendor_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        rows.iter().map(Self::row_to_metadata).collect()
    }

    async fn resolve_one(
        &self,
        selector: &FirmwareSelector,
    ) -> Result<Option<FirmwareMetadata>, StoreError> {
        let matches = self.resolve_many(selector).await?;
        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.into_iter().next()),
            n => Err(StoreError::Ambiguous(n)),
        }
    }

    async fn resolve_many(
        &self,
        selector: &FirmwareSelector,
    ) -> Result<Vec<FirmwareMetadata>, StoreError> {
        // --id short-circuits every other filter — it addresses exactly one
        // row by its surrogate key.
        if let Some(id) = selector.id {
            let row = sqlx::query("SELECT metadata_json FROM firmware_current WHERE id = ?")
                .bind(id.to_string())
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| StoreError::Backend(e.to_string()))?;
            return Ok(match row {
                Some(r) => vec![Self::row_to_metadata(&r)?],
                None => vec![],
            });
        }

        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT metadata_json, vendor, device_family, hardware_key, version_ordinal \
             FROM firmware_current WHERE 1 = 1",
        );

        if let Some(vendor) = &selector.vendor {
            qb.push(" AND vendor = ").push_bind(vendor.clone());
        }
        if let Some(device_family) = &selector.device_family {
            qb.push(" AND device_family = ")
                .push_bind(device_family.clone());
        }
        if let Some(hardware) = &selector.hardware {
            qb.push(" AND hardware_key = ")
                .push_bind(hardware_key(hardware));
        }
        if let Some(version) = &selector.version {
            qb.push(" AND version_raw = ").push_bind(version.clone());
        }
        // version_ordinal DESC so, within each (vendor, device_family,
        // hardware_key) group, the highest-precedence version comes first —
        // the --latest pass below relies on that ordering.
        qb.push(" ORDER BY vendor, device_family, hardware_key, version_ordinal DESC");

        let rows = qb
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;

        if !selector.latest {
            return rows.iter().map(Self::row_to_metadata).collect();
        }

        // --latest: keep the first row per (vendor, device_family,
        // hardware_key) group that has a non-null version_ordinal. A group
        // where every row has an Opaque scheme (no derivable ordinal) is
        // skipped entirely — there is no reliable "latest" for it, matching
        // VersionKey::partial_cmp's None for two Opaque versions (see the
        // README's "Data model and identity keys" section).
        // We deliberately don't fall back to string or observation-order
        // comparison for those rows; silently guessing a "latest" would be
        // wrong in exactly the cases that section exists to guard against.
        let mut seen_groups: std::collections::HashSet<(String, String, String)> =
            std::collections::HashSet::new();
        let mut result = Vec::new();
        for row in &rows {
            let ordinal: Option<String> = row
                .try_get("version_ordinal")
                .map_err(|e| StoreError::Backend(e.to_string()))?;
            if ordinal.is_none() {
                continue;
            }
            let vendor: String = row
                .try_get("vendor")
                .map_err(|e| StoreError::Backend(e.to_string()))?;
            let device_family: String = row
                .try_get("device_family")
                .map_err(|e| StoreError::Backend(e.to_string()))?;
            let hw: String = row
                .try_get("hardware_key")
                .map_err(|e| StoreError::Backend(e.to_string()))?;
            if seen_groups.insert((vendor, device_family, hw)) {
                result.push(Self::row_to_metadata(row)?);
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use delve_core::model::VersionKey;
    use delve_core::store::{FirmwareSelector, RunKind, RunOutcome};

    async fn test_store() -> SqliteStore {
        SqliteStore::open_url("sqlite::memory:")
            .await
            .expect("in-memory store should open")
    }

    #[tokio::test]
    async fn open_creates_missing_parent_directories() {
        // Regression test: on a genuine first run, database_path's parent
        // directory (e.g. ~/.config/delve/) usually doesn't exist yet,
        // since nothing else creates it either. SQLite's mode=rwc creates
        // the file but not the directory — without the fix in `open`,
        // this fails with SQLITE_CANTOPEN ("unable to open database file").
        let dir = std::env::temp_dir().join(format!("delve-test-{}", uuid::Uuid::new_v4()));
        let nested_path = dir.join("nested").join("delve.sqlite");
        assert!(
            !nested_path.parent().unwrap().exists(),
            "test setup: directory must not pre-exist"
        );

        let store = SqliteStore::open(nested_path.to_str().unwrap()).await;
        assert!(
            store.is_ok(),
            "open() must create missing parent directories, got: {:?}",
            store.err()
        );
        assert!(nested_path.parent().unwrap().exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn firmware_ref(vendor: &str, device_family: &str, url: &str) -> FirmwareRef {
        FirmwareRef {
            vendor: vendor.into(),
            device_family: device_family.into(),
            source_url: url.parse().unwrap(),
            discovered_at: Utc::now(),
        }
    }

    fn metadata(
        vendor: &str,
        device_family: &str,
        version: &str,
        ordinal: Option<Vec<u64>>,
        hw: &[&str],
        sha_byte: u8,
    ) -> FirmwareMetadata {
        FirmwareMetadata {
            vendor: vendor.into(),
            device_family: device_family.into(),
            // Test fixture default; real values come from the engine
            // copying the originating FirmwareRef's source_url — see
            // engine.rs's run_loop.
            source_url: "https://example.test/fixture".parse().unwrap(),
            version: VersionKey {
                raw: version.into(),
                scheme: if ordinal.is_some() {
                    VersionScheme::Semver
                } else {
                    VersionScheme::Opaque
                },
                ordinal,
            },
            release_date: None,
            sha256: Some([sha_byte; 32]),
            signature: None,
            hardware_targets: hw.iter().map(|s| s.to_string()).collect(),
            release_notes_url: None,
        }
    }

    #[tokio::test]
    async fn upsert_then_lookup_round_trips() {
        let store = test_store().await;
        let r = firmware_ref("acme", "widget", "https://example.test/a");
        let meta = metadata(
            "acme",
            "widget",
            "1.0.0",
            Some(vec![1, 0, 0]),
            &["rev-a"],
            0xAB,
        );
        let run_id = store.start_run("acme", RunKind::Baseline).await.unwrap();

        store.upsert(&r, &meta, run_id).await.unwrap();

        let hw = vec!["rev-a".to_string()];
        let key = FirmwareKey {
            vendor: "acme",
            device_family: "widget",
            hardware_targets: &hw,
            version_raw: "1.0.0",
        };
        let found = store
            .lookup(&key)
            .await
            .unwrap()
            .expect("entry should be found");
        assert_eq!(found.version.raw, "1.0.0");
        assert_eq!(found.sha256, Some([0xAB; 32]));
    }

    #[tokio::test]
    async fn source_url_round_trips_through_resolve_one() {
        // The store just faithfully persists whatever FirmwareMetadata it's
        // given — it doesn't derive source_url from the FirmwareRef param
        // to upsert (that consistency is the engine's job, tested in
        // delve-core's engine.rs). Deliberately using a distinct URL from
        // the FirmwareRef's own to make that separation clear here.
        let store = test_store().await;
        let r = firmware_ref("acme", "widget", "https://example.test/discovery-page");
        let mut meta = metadata(
            "acme",
            "widget",
            "1.0.0",
            Some(vec![1, 0, 0]),
            &["rev-a"],
            1,
        );
        meta.source_url = "https://example.test/actual-firmware-image"
            .parse()
            .unwrap();
        let run_id = store.start_run("acme", RunKind::Baseline).await.unwrap();
        store.upsert(&r, &meta, run_id).await.unwrap();

        let selector = FirmwareSelector {
            vendor: Some("acme".into()),
            device_family: Some("widget".into()),
            version: Some("1.0.0".into()),
            ..Default::default()
        };
        let found = store
            .resolve_one(&selector)
            .await
            .unwrap()
            .expect("entry should resolve");
        assert_eq!(
            found.source_url.as_str(),
            "https://example.test/actual-firmware-image"
        );
    }

    #[tokio::test]
    async fn lookup_returns_none_for_unknown_entry() {
        let store = test_store().await;
        let hw: Vec<String> = vec![];
        let key = FirmwareKey {
            vendor: "nope",
            device_family: "nope",
            hardware_targets: &hw,
            version_raw: "0.0.0",
        };
        assert!(store.lookup(&key).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn upsert_is_idempotent_on_the_same_natural_key_and_updates_current() {
        let store = test_store().await;
        let r = firmware_ref("acme", "widget", "https://example.test/a");
        let run1 = store.start_run("acme", RunKind::Baseline).await.unwrap();
        store
            .upsert(
                &r,
                &metadata(
                    "acme",
                    "widget",
                    "1.0.0",
                    Some(vec![1, 0, 0]),
                    &["rev-a"],
                    1,
                ),
                run1,
            )
            .await
            .unwrap();

        let run2 = store.start_run("acme", RunKind::Incremental).await.unwrap();
        // Same natural key, different hash — simulates the entry being
        // re-observed with a changed hash under the same version string.
        store
            .upsert(
                &r,
                &metadata(
                    "acme",
                    "widget",
                    "1.0.0",
                    Some(vec![1, 0, 0]),
                    &["rev-a"],
                    2,
                ),
                run2,
            )
            .await
            .unwrap();

        let hw = vec!["rev-a".to_string()];
        let key = FirmwareKey {
            vendor: "acme",
            device_family: "widget",
            hardware_targets: &hw,
            version_raw: "1.0.0",
        };
        let current = store.lookup(&key).await.unwrap().unwrap();
        assert_eq!(
            current.sha256,
            Some([2u8; 32]),
            "current row must reflect the latest observation"
        );

        let history = store.history(&key).await.unwrap();
        assert_eq!(
            history.len(),
            2,
            "both observations must be preserved in the revision log, not just the latest"
        );
    }

    #[tokio::test]
    async fn baseline_tracking_round_trips_through_mark_and_clear() {
        let store = test_store().await;
        assert!(!store.has_completed_baseline("acme").await.unwrap());

        store.mark_baseline_complete("acme").await.unwrap();
        assert!(store.has_completed_baseline("acme").await.unwrap());

        store.clear_baseline("acme").await.unwrap();
        assert!(
            !store.has_completed_baseline("acme").await.unwrap(),
            "clear_baseline (backing --redig) must actually unset it"
        );
    }

    #[tokio::test]
    async fn baseline_tracking_is_independent_per_vendor() {
        let store = test_store().await;
        store.mark_baseline_complete("acme").await.unwrap();
        assert!(store.has_completed_baseline("acme").await.unwrap());
        assert!(
            !store.has_completed_baseline("other-vendor").await.unwrap(),
            "one vendor's baseline must not leak to another — see the README's \"Baseline vs incremental digs\" section"
        );
    }

    #[tokio::test]
    async fn resolve_one_finds_a_uniquely_matching_entry_by_natural_key() {
        let store = test_store().await;
        let r = firmware_ref("acme", "widget", "https://example.test/a");
        let run_id = store.start_run("acme", RunKind::Baseline).await.unwrap();
        store
            .upsert(
                &r,
                &metadata(
                    "acme",
                    "widget",
                    "1.0.0",
                    Some(vec![1, 0, 0]),
                    &["rev-a"],
                    1,
                ),
                run_id,
            )
            .await
            .unwrap();

        let selector = FirmwareSelector {
            vendor: Some("acme".into()),
            device_family: Some("widget".into()),
            version: Some("1.0.0".into()),
            ..Default::default()
        };
        let found = store.resolve_one(&selector).await.unwrap();
        assert!(found.is_some());
    }

    #[tokio::test]
    async fn resolve_one_errors_ambiguous_when_selector_matches_multiple_entries() {
        let store = test_store().await;
        let r = firmware_ref("acme", "widget", "https://example.test/a");
        let run_id = store.start_run("acme", RunKind::Baseline).await.unwrap();
        store
            .upsert(
                &r,
                &metadata(
                    "acme",
                    "widget",
                    "1.0.0",
                    Some(vec![1, 0, 0]),
                    &["rev-a"],
                    1,
                ),
                run_id,
            )
            .await
            .unwrap();
        store
            .upsert(
                &r,
                &metadata(
                    "acme",
                    "widget",
                    "1.1.0",
                    Some(vec![1, 1, 0]),
                    &["rev-a"],
                    2,
                ),
                run_id,
            )
            .await
            .unwrap();

        // No version given — matches both rows just inserted.
        let selector = FirmwareSelector {
            vendor: Some("acme".into()),
            device_family: Some("widget".into()),
            ..Default::default()
        };

        match store.resolve_one(&selector).await {
            Err(StoreError::Ambiguous(n)) => assert_eq!(n, 2),
            other => panic!("expected Err(Ambiguous(2)), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_one_returns_none_for_no_match() {
        let store = test_store().await;
        let selector = FirmwareSelector {
            vendor: Some("nobody".into()),
            ..Default::default()
        };
        assert!(store.resolve_one(&selector).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn resolve_many_filters_by_hardware_target() {
        let store = test_store().await;
        let r = firmware_ref("acme", "widget", "https://example.test/a");
        let run_id = store.start_run("acme", RunKind::Baseline).await.unwrap();
        store
            .upsert(
                &r,
                &metadata(
                    "acme",
                    "widget",
                    "1.0.0",
                    Some(vec![1, 0, 0]),
                    &["rev-a"],
                    1,
                ),
                run_id,
            )
            .await
            .unwrap();
        store
            .upsert(
                &r,
                &metadata(
                    "acme",
                    "widget",
                    "1.0.0-revb",
                    Some(vec![1, 0, 0]),
                    &["rev-b"],
                    2,
                ),
                run_id,
            )
            .await
            .unwrap();

        let selector = FirmwareSelector {
            vendor: Some("acme".into()),
            device_family: Some("widget".into()),
            hardware: Some(vec!["rev-a".into()]),
            ..Default::default()
        };
        let matches = store.resolve_many(&selector).await.unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].version.raw, "1.0.0");
    }

    #[tokio::test]
    async fn resolve_many_with_latest_picks_highest_ordinal_not_highest_string() {
        let store = test_store().await;
        let r = firmware_ref("acme", "widget", "https://example.test/a");
        let run_id = store.start_run("acme", RunKind::Baseline).await.unwrap();
        store
            .upsert(
                &r,
                &metadata(
                    "acme",
                    "widget",
                    "1.0.0",
                    Some(vec![1, 0, 0]),
                    &["rev-a"],
                    1,
                ),
                run_id,
            )
            .await
            .unwrap();
        store
            .upsert(
                &r,
                &metadata(
                    "acme",
                    "widget",
                    "1.10.0",
                    Some(vec![1, 10, 0]),
                    &["rev-a"],
                    2,
                ),
                run_id,
            )
            .await
            .unwrap();
        store
            .upsert(
                &r,
                &metadata(
                    "acme",
                    "widget",
                    "1.2.0",
                    Some(vec![1, 2, 0]),
                    &["rev-a"],
                    3,
                ),
                run_id,
            )
            .await
            .unwrap();

        let selector = FirmwareSelector {
            vendor: Some("acme".into()),
            device_family: Some("widget".into()),
            latest: true,
            ..Default::default()
        };
        let matches = store.resolve_many(&selector).await.unwrap();
        assert_eq!(
            matches.len(),
            1,
            "one group must resolve to exactly one latest entry"
        );
        assert_eq!(
            matches[0].version.raw, "1.10.0",
            "1.10.0 must beat 1.2.0 by numeric ordinal, not lexicographic string order"
        );
    }

    #[tokio::test]
    async fn resolve_many_with_latest_skips_groups_with_no_derivable_ordinal() {
        let store = test_store().await;
        let r = firmware_ref("acme", "widget", "https://example.test/a");
        let run_id = store.start_run("acme", RunKind::Baseline).await.unwrap();
        // Opaque scheme, no ordinal — a --latest resolution can't be trusted here.
        store
            .upsert(
                &r,
                &metadata("acme", "widget", "build-x", None, &["rev-a"], 1),
                run_id,
            )
            .await
            .unwrap();

        let selector = FirmwareSelector {
            vendor: Some("acme".into()),
            device_family: Some("widget".into()),
            latest: true,
            ..Default::default()
        };
        let matches = store.resolve_many(&selector).await.unwrap();
        assert!(
            matches.is_empty(),
            "a group with no derivable ordinal must not resolve --latest by guessing"
        );
    }

    #[tokio::test]
    async fn resolve_many_with_latest_handles_each_device_family_independently() {
        let store = test_store().await;
        let run_id = store.start_run("acme", RunKind::Baseline).await.unwrap();
        let r_widget = firmware_ref("acme", "widget", "https://example.test/a");
        let r_gadget = firmware_ref("acme", "gadget", "https://example.test/b");

        store
            .upsert(
                &r_widget,
                &metadata(
                    "acme",
                    "widget",
                    "1.0.0",
                    Some(vec![1, 0, 0]),
                    &["rev-a"],
                    1,
                ),
                run_id,
            )
            .await
            .unwrap();
        store
            .upsert(
                &r_widget,
                &metadata(
                    "acme",
                    "widget",
                    "2.0.0",
                    Some(vec![2, 0, 0]),
                    &["rev-a"],
                    2,
                ),
                run_id,
            )
            .await
            .unwrap();
        store
            .upsert(
                &r_gadget,
                &metadata(
                    "acme",
                    "gadget",
                    "3.0.0",
                    Some(vec![3, 0, 0]),
                    &["rev-x"],
                    3,
                ),
                run_id,
            )
            .await
            .unwrap();
        store
            .upsert(
                &r_gadget,
                &metadata(
                    "acme",
                    "gadget",
                    "3.5.0",
                    Some(vec![3, 5, 0]),
                    &["rev-x"],
                    4,
                ),
                run_id,
            )
            .await
            .unwrap();

        let selector = FirmwareSelector {
            vendor: Some("acme".into()),
            latest: true,
            ..Default::default()
        };
        let mut matches = store.resolve_many(&selector).await.unwrap();
        matches.sort_by(|a, b| a.device_family.cmp(&b.device_family));

        assert_eq!(
            matches.len(),
            2,
            "two independent device families must each get their own latest entry"
        );
        assert_eq!(matches[0].version.raw, "3.5.0"); // gadget
        assert_eq!(matches[1].version.raw, "2.0.0"); // widget
    }

    #[tokio::test]
    async fn run_bookkeeping_accepts_a_full_start_complete_cycle() {
        let store = test_store().await;
        let run_id = store.start_run("acme", RunKind::Incremental).await.unwrap();
        // Mainly guards against complete_run erroring on a run_id that
        // start_run just returned, and against a foreign-key mismatch
        // between runs and firmware_revisions/firmware_current.
        store
            .complete_run(run_id, RunOutcome::Success)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn all_current_only_returns_entries_for_the_requested_vendor() {
        let store = test_store().await;
        let run_id = store.start_run("acme", RunKind::Baseline).await.unwrap();
        store
            .upsert(
                &firmware_ref("acme", "widget", "https://example.test/a"),
                &metadata(
                    "acme",
                    "widget",
                    "1.0.0",
                    Some(vec![1, 0, 0]),
                    &["rev-a"],
                    1,
                ),
                run_id,
            )
            .await
            .unwrap();
        store
            .upsert(
                &firmware_ref("other-vendor", "widget", "https://example.test/b"),
                &metadata(
                    "other-vendor",
                    "widget",
                    "1.0.0",
                    Some(vec![1, 0, 0]),
                    &["rev-a"],
                    2,
                ),
                run_id,
            )
            .await
            .unwrap();

        let acme_entries = store.all_current("acme").await.unwrap();
        assert_eq!(acme_entries.len(), 1);
        assert_eq!(acme_entries[0].vendor, "acme");
    }

    #[tokio::test]
    async fn latest_known_picks_the_highest_ordinal_not_the_most_recently_inserted() {
        let store = test_store().await;
        let r = firmware_ref("acme", "widget", "https://example.test/a");
        let run_id = store.start_run("acme", RunKind::Baseline).await.unwrap();
        // Insert a high version first, then a lower one — latest_known must
        // still report the high one, i.e. it isn't just "most recent row".
        store
            .upsert(
                &r,
                &metadata(
                    "acme",
                    "widget",
                    "5.0.0",
                    Some(vec![5, 0, 0]),
                    &["rev-a"],
                    1,
                ),
                run_id,
            )
            .await
            .unwrap();
        store
            .upsert(
                &r,
                &metadata(
                    "acme",
                    "widget",
                    "1.0.0",
                    Some(vec![1, 0, 0]),
                    &["rev-a"],
                    2,
                ),
                run_id,
            )
            .await
            .unwrap();

        let found = store
            .latest_known("acme", "widget", &["rev-a".to_string()])
            .await
            .unwrap();
        assert_eq!(found.unwrap().version.raw, "5.0.0");
    }

    #[tokio::test]
    async fn latest_known_returns_none_for_a_line_with_no_entries_yet() {
        let store = test_store().await;
        let found = store
            .latest_known("acme", "widget", &["rev-a".to_string()])
            .await
            .unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn latest_known_is_scoped_to_vendor_device_family_and_hardware() {
        let store = test_store().await;
        let run_id = store.start_run("acme", RunKind::Baseline).await.unwrap();
        store
            .upsert(
                &firmware_ref("acme", "widget", "https://example.test/a"),
                &metadata(
                    "acme",
                    "widget",
                    "9.0.0",
                    Some(vec![9, 0, 0]),
                    &["rev-a"],
                    1,
                ),
                run_id,
            )
            .await
            .unwrap();

        // Different device family — must not be picked up as "the latest"
        // for "gadget", which has no entries of its own yet.
        let found = store
            .latest_known("acme", "gadget", &["rev-a".to_string()])
            .await
            .unwrap();
        assert!(
            found.is_none(),
            "latest_known must not leak across device families"
        );

        // Different hardware target on the same device family — same story.
        let found = store
            .latest_known("acme", "widget", &["rev-b".to_string()])
            .await
            .unwrap();
        assert!(
            found.is_none(),
            "latest_known must not leak across hardware targets"
        );
    }
}
