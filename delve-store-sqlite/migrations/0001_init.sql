-- See the README's "Storage" section for the schema rationale. Run via
-- sqlx::migrate! at startup.

PRAGMA journal_mode = WAL;

CREATE TABLE IF NOT EXISTS runs (
    id           BLOB PRIMARY KEY,
    vendor       TEXT NOT NULL,
    kind         TEXT NOT NULL,   -- 'baseline' | 'incremental'
    started_at   TEXT NOT NULL,
    completed_at TEXT,
    outcome      TEXT             -- null while in progress
);

CREATE TABLE IF NOT EXISTS baselines (
    vendor       TEXT PRIMARY KEY,
    completed_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS firmware_current (
    id              BLOB PRIMARY KEY,        -- surrogate UUID, for CLI addressing (unearth --id)
    vendor          TEXT NOT NULL,
    device_family   TEXT NOT NULL,
    hardware_key    TEXT NOT NULL,           -- canonicalized, sorted hardware_targets
    version_raw     TEXT NOT NULL,           -- display value, exactly as vendor published
    version_scheme  TEXT NOT NULL,           -- 'semver' | 'vendor_numeric' | 'opaque'
    version_ordinal TEXT,                    -- zero-padded, dot-joined, SQL-sortable; NULL if Opaque
    source_url      TEXT NOT NULL,
    sha256          BLOB,
    release_date    TEXT,
    metadata_json   TEXT NOT NULL,           -- full FirmwareMetadata, serde-serialized
    first_seen_run_id BLOB NOT NULL REFERENCES runs(id),
    last_seen_run_id  BLOB NOT NULL REFERENCES runs(id),
    UNIQUE (vendor, device_family, hardware_key, version_raw)
);

CREATE INDEX IF NOT EXISTS idx_firmware_current_vendor
    ON firmware_current (vendor, device_family);

CREATE TABLE IF NOT EXISTS firmware_revisions (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    vendor         TEXT NOT NULL,
    device_family  TEXT NOT NULL,
    hardware_key   TEXT NOT NULL,
    version_raw    TEXT NOT NULL,
    run_id         BLOB NOT NULL REFERENCES runs(id),
    observed_at    TEXT NOT NULL,
    metadata_json  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_firmware_revisions_entry
    ON firmware_revisions (vendor, device_family, hardware_key, version_raw);
