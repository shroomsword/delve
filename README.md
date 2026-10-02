# delve

[![CI](https://github.com/shroomsword/delve/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/shroomsword/delve/actions/workflows/ci.yml?query=branch%3Amain)
[![Release](https://github.com/shroomsword/delve/actions/workflows/release.yml/badge.svg)](https://github.com/shroomsword/delve/actions/workflows/release.yml)

An embedded firmware scraper and notification framework. `delve` discovers
firmware releases across embedded device vendors, tracks their metadata
over time, and notifies configured subscribers when something new or
changed shows up. Vendor support is pluggable — the core engine has no
built-in knowledge of any specific vendor's portal, API, or firmware
format.

**Status: early development.** The workspace builds against real
crates.io dependencies, and CI runs `rustfmt`, `clippy`, a RustSec
advisory check, the test suite, release builds and a warning-free
`cargo doc` on every pull request and every push to `main`,
with all features enabled and the lockfile enforced (`--locked`). Tests
and builds cover Linux x86_64, macOS (Apple Silicon and Intel) and
Windows x86_64; on Intel macOS the tests are compiled but not run.

- **`vendor-unifi` works end to end.** Its `discover`, `metadata` and
  `fetch` work against Ubiquiti's firmware update API, confirmed by live
  tests and an end-to-end CLI run. It covers UniFi network-device
  firmware only — see [Vendor: UniFi](#vendor-unifi). It's the only vendor
  with `tos_reviewed: true`, so it's the only one `dig` currently runs.
- **`vendor-cisco` is unverified.** Its `discover`/`metadata` are
  implemented against Cisco's Software Suggestion API with real control
  flow (OAuth2 auth, rate limiting, error handling), but the endpoints and
  response shapes were written without network access and haven't been
  checked against Cisco's live API. `fetch` (binary download) is not
  implemented. Treat it as a first draft — see [Known gaps](#known-gaps).

## Contents

- [Goals and non-goals](#goals-and-non-goals)
- [Workspace layout](#workspace-layout)
- [Plugin architecture](#plugin-architecture)
- [Data model and identity keys](#data-model-and-identity-keys)
- [Baseline vs incremental digs](#baseline-vs-incremental-digs)
- [Storage](#storage)
- [Transport and proxying](#transport-and-proxying)
- [Credentials](#credentials)
- [Rate limiting](#rate-limiting)
- [HTTP client hardening](#http-client-hardening)
- [Notifications](#notifications)
- [CLI commands](#cli-commands)
- [Configuration reference](#configuration-reference)
- [Compliance: ToS and robots.txt](#compliance-tos-and-robotstxt)
- [Vendor: UniFi](#vendor-unifi)
- [Naming](#naming)
- [Known gaps](#known-gaps)
- [Test coverage](#test-coverage)
- [Not yet scaffolded](#not-yet-scaffolded)

## Goals and non-goals

**Goals:**
- Vendor support added or removed without touching core engine code
- Safe to run unattended on a schedule (cron, systemd timer, CI)
- A vendor's first-ever dig establishes a baseline silently; only
  *subsequent* digs notify
- Optional traffic anonymization via Tor/SOCKS5, configurable per vendor
- A full historical record of every observed firmware revision, kept
  permanently
- Firmware binaries fetched on demand via an explicit command, never
  automatically

**Non-goals (at least for now):**
- Firmware binary analysis/unpacking — this is a metadata tracker, not a
  firmware reverse-engineering tool
- A hosted/multi-tenant service — this is a CLI tool with a local SQLite
  store
- A stable Rust ABI for out-of-tree dynamic plugins — static, compiled-in
  plugins are the only supported tier right now (see
  [Plugin architecture](#plugin-architecture))

## Workspace layout

```
delve-core/            traits, engine, data model — no vendor knowledge
delve-cli/              binary: config loading, plugin registry, commands
delve-store-sqlite/     MetadataStore impl against SQLite (schema in migrations/)
delve-plugin-testkit/   conformance checks and a mock HTTP server for vendor crates' tests
delve-vendors/
  vendor-cisco/          discover/metadata implemented but unverified — see below
  vendor-unifi/          UniFi network-device firmware, verified against the live API
delve-subscribers/
  subscriber-log/        always-on audit-trail subscriber
  subscriber-webhook/     POSTs events to a configured URL, feature-gated
  subscriber-email/       sends one email per event over SMTP, feature-gated
```

`delve-store-sqlite` is its own crate rather than folded into `delve-core`,
so `delve-core` doesn't pull in `sqlx` as a dependency — anything
implementing `MetadataStore` against a different backend later only needs
to depend on `delve-core`.

Build the CLI with a vendor and the webhook and email subscribers:

```
cargo build --features "vendor-cisco,subscriber-webhook,subscriber-email" -p delve-cli
```

`--features all-vendors` enables every vendor crate.

## Plugin architecture

### Static plugins (the only tier implemented)

Vendor crates self-register at link time via the `inventory` crate, gated
behind a Cargo feature per vendor:

```rust
inventory::submit! {
    PluginDescriptor {
        id: "cisco",
        factory: || Box::new(CiscoPlugin::new()),
    }
}
```

```toml
[features]
default = []
vendor-cisco = ["dep:vendor-cisco"]
vendor-unifi = ["dep:vendor-unifi"]
all-vendors = ["vendor-cisco", "vendor-unifi"]
```

Each vendor crate also needs a feature-gated `use` in `delve-cli/src/main.rs`:

```rust
#[cfg(feature = "vendor-unifi")]
use vendor_unifi as _;
```

Nothing else in the CLI names a vendor crate, and rustc doesn't link a
dependency that's never referenced — without this line the crate's
`inventory::submit!` never runs, the plugin silently never registers, and
`dig --vendor <id>` reports "unknown vendor". (Every vendor was missing
this until `vendor-unifi` was added.)

`PluginRegistry::discover()` walks every registered `PluginDescriptor` at
startup and instantiates each one — this picks up exactly the vendors
compiled in via Cargo features; there's no separate enable/disable step
beyond the feature flags that pulled the vendor crate in as a dependency.

This gets you: no ABI concerns, full type safety, LTO-friendly binaries,
and a clean way to ship a minimal build with only the vendors you need.
The cost is a rebuild to add a vendor — acceptable for a framework under
your own control, not a plugin marketplace.

### Dynamic plugins (deferred, not implemented)

If out-of-tree third-party plugins become a real requirement later, two
approaches are worth considering when that day comes:

- **`abi_stable`** — `#[repr(C)]`-safe trait objects over `libloading`,
  Rust-to-Rust only.
- **WASM via `wasmtime`** — plugins compiled to `wasm32-wasip1`, with the
  `VendorPlugin` surface defined as a WIT interface. The stronger
  candidate if this tier is ever built, since it sandboxes plugins —
  relevant because vendor-scraping code may be flaky, third-party, or
  touching poorly-secured portals, and sandboxing limits the blast radius
  of a bad or malicious plugin.

The `VendorPlugin` trait is deliberately narrow (see below) specifically
so it doesn't preclude adding a dynamic tier later without a redesign.

### The `VendorPlugin` trait

```rust
#[async_trait]
pub trait VendorPlugin: Send + Sync {
    fn vendor_id(&self) -> &'static str;
    fn capabilities(&self) -> PluginCapabilities;

    async fn discover(&self, ctx: &ScrapeContext) -> Result<Vec<FirmwareRef>, PluginError>;
    async fn metadata(&self, ctx: &ScrapeContext, r: &FirmwareRef) -> Result<FirmwareMetadata, PluginError>;
    async fn fetch(&self, ctx: &ScrapeContext, r: &FirmwareRef, sink: &mut dyn ArtifactSink) -> Result<(), PluginError>;
}
```

- `discover` enumerates candidate firmware artifacts from a vendor's
  sources (portal API, FTP tree, git releases, whatever) — called on
  every dig, baseline or incremental alike.
- `metadata` pulls version/hash/release-date/hardware-target information
  for one candidate, without downloading the full image where the source
  allows it.
- `fetch` downloads (and the plugin may verify) the actual binary. This is
  **only ever called from the manual `unearth` command**, never from a
  scheduled dig — see [Baseline vs incremental digs](#baseline-vs-incremental-digs).

Kept intentionally narrow: vendor-specific quirks (auth flows, pagination,
container formats) stay inside each plugin crate, never leak into this
trait's signature.

`PluginCapabilities.tos_reviewed` gates whether `dig` will run a vendor at
all — see [Compliance](#compliance-tos-and-robotstxt).

### Testing a vendor plugin

`delve-plugin-testkit` holds the checks every vendor plugin must pass, so
each vendor crate is held to the same contract and none needs the network
in CI. A vendor crate adds it as a **dev-dependency** and calls it from its
own tests. The checks panic with a message naming the vendor and the broken
rule, like `assert!`.

A vendor's test builds a `MockServer` that serves its wire format from
fixtures, points the plugin at it, and creates a context with
`delve_plugin_testkit::context(interval)`. Then:

| Check | Rule it enforces |
|---|---|
| `discover_conforms` | `discover()` succeeds with at least one ref; each ref's `vendor` equals `vendor_id()`, its `device_family` is non-blank, and its `source_url` is unique |
| `metadata_conforms` | `metadata()` succeeds for every ref; the version is non-blank; an `Opaque` version has no ordinal and an ordinal is never empty; no hardware target is blank |
| `assert_paced` | `ctx.throttle()` is called before every request: no two requests reach the server closer together than the rate limit (less 20% for jitter). It fails if the plugin made too few requests to tell, so give it several, such as one per model |
| `discover_skips_malformed_records` | against a server that includes records the plugin can't parse, `discover()` skips them and returns the valid ones instead of failing the dig |

`vendor-unifi`'s tests are the reference: they serve its fixtures from a
`MockServer` and run all four checks. `vendor-cisco` doesn't use the
testkit yet, because its `discover`/`metadata` still need the live
verification described under [Known gaps](#known-gaps), and its OAuth flow
needs a mock token endpoint first.

## Data model and identity keys

```rust
pub struct FirmwareRef {
    pub vendor: String,
    pub device_family: String,
    pub source_url: Url,
    pub discovered_at: DateTime<Utc>,
}

pub struct FirmwareMetadata {
    pub vendor: String,
    pub device_family: String,
    pub source_url: Url,
    pub version: VersionKey,
    pub release_date: Option<NaiveDate>,
    pub sha256: Option<[u8; 32]>,
    pub signature: Option<SignatureInfo>,
    pub hardware_targets: Vec<String>,
    pub release_notes_url: Option<Url>,
}
```

`FirmwareRef` is what `discover` produces — a candidate, before its
metadata has been pulled. `FirmwareMetadata` is everything known about one
release short of the binary itself; `MetadataStore::upsert` persists it.

**`vendor`/`device_family`/`source_url` are duplicated onto
`FirmwareMetadata` from `FirmwareRef`.** This looks redundant, and it is —
deliberately. `MetadataStore::resolve_one`/`resolve_many` (what backs
`catalog`, `provenance`, and `unearth`) return a `StoredFirmware` — the
surrogate id plus the `FirmwareMetadata`, which `catalog` prints. Without
these three fields also living there, a lookup by surrogate id alone
(`unearth --id <uuid>`) couldn't recover which vendor owns the entry or
rebuild the `FirmwareRef` that `fetch()` needs. The engine populates all
three from the originating `FirmwareRef` immediately after a plugin's
`metadata()` call returns, so plugin authors never set them themselves and
they're guaranteed to match.

### Version ordering vs. chronological ordering

Two genuinely different concepts, kept deliberately separate:

- **Chronological**: `release_date` — when the vendor published it.
- **Logical precedence**: `VersionKey` — whether one version supersedes
  another.

Vendors sometimes disagree with themselves: a backported patch to an older
branch, a re-release, a yanked-and-republished version with an unchanged
date. Conflating "newer" with "more recently observed" produces wrong
answers in exactly these cases, so the codebase never does.

```rust
pub enum VersionScheme {
    Semver,
    VendorNumeric,   // a consistent but non-semver scheme, e.g. "17.9.4a"
    Opaque,          // no reliable ordering can be derived
}

pub struct VersionKey {
    pub raw: String,               // exactly as the vendor published it
    pub scheme: VersionScheme,
    pub ordinal: Option<Vec<u64>>, // comparable tuple; None for Opaque
}
```

Each plugin is responsible for producing the right `scheme`/`ordinal` for
its vendor's format. A plugin that doesn't know the scheme well enough can
fall back to `Opaque` and still get correct equality-based change
detection, without asserting a false direction. `VersionKey::partial_cmp`
returns `None` whenever either side lacks an ordinal — there is no
fallback to string comparison, because that would silently produce a wrong
answer for non-lexicographic schemes (`"1.10.0"` must sort after
`"1.2.0"`, not before it).

`VersionDirection::between(old, new)` turns that comparison into
`Newer`/`Older`/`Unordered`, surfaced on `FirmwareEvent::UpdatedRelease` —
see [Notifications](#notifications) for why the distinction matters to a
subscriber.

### Identity key

A firmware entry's identity is the tuple `(vendor, device_family,
hardware_targets, version)`. `hardware_targets` is part of it because some
vendors reuse a version string across hardware revisions of "the same"
device family that actually ship different binaries — without hardware in
the key, those would collide.

`hardware_targets: Vec<String>` is canonicalized via `hardware_key()` into
a single sorted, delimited string (order-independent — `["rev-b",
"rev-a"]` and `["rev-a", "rev-b"]` produce the same key) for use as part
of the SQL identity key. The real `Vec<String>` stays on `FirmwareMetadata`
for display and querying; the canonicalized form is purely a storage-key
concern.

## Baseline vs incremental digs

Tracked **per vendor**, not globally, so adding a new vendor plugin later
gets its own silent baseline dig rather than immediately firing
notifications for everything it finds on day one.

```rust
pub enum RunKind { Baseline, Incremental }
```

`dig_vendor` determines which kind of run this is via
`store.has_completed_baseline(vendor_id)`, then for every candidate the
plugin discovers:

1. Calls `metadata()`, then populates `vendor`/`device_family`/`source_url`
   (see [Data model](#data-model-and-identity-keys)).
2. Always persists the observation via `upsert` — baseline or incremental,
   this always happens.
3. On a **baseline** run, stops there — no notification is ever published.
   This is what makes a vendor's first-ever dig silent instead of firing
   an event for every firmware version that vendor has ever shipped.
4. On an **incremental** run, decides what happened using two different
   questions against the store:
   - `lookup(exact_key)` — "have we stored *precisely* this version
     before" — catches a vendor silently rebuilding an already-published
     version number under the same version string but a different hash.
   - `latest_known(vendor, device_family, hardware_targets)` — "what
     version did we know about for this line *before* this observation,"
     independent of whatever version just arrived — this is what actually
     detects an ordinary version bump and lets `VersionDirection` report a
     real `Newer`/`Older` instead of always `Unordered`.

   These are genuinely different questions, and an earlier version of this
   engine used only the first one, which meant a version bump (which by
   definition can never match an exact-version lookup, since that version
   has never been stored before) was indistinguishable from a first-ever
   release — `NewRelease` fired correctly, but `version_direction` on
   `UpdatedRelease` was dead code, since `old.version` and `fresh.version`
   were guaranteed equal whenever `lookup` found something at all. Fixed
   by splitting into the two questions above; see
   [Test coverage](#test-coverage) for how this was caught.

**Correctness notes:**

- A vendor's baseline is only marked complete *after* the full
  discover-and-store loop succeeds. A baseline run that dies mid-scrape
  must **not** be marked complete — otherwise the next run would treat
  whatever the failed run didn't reach as "new," and fire a notification
  storm for firmware that was actually present the whole time, just not
  yet observed.
- `dig --vendor <id> --redig` clears that vendor's baseline flag before
  running, so its next dig is treated as fresh — persists everything,
  suppresses notifications, same as a genuine first-ever dig. Useful after
  suspected store corruption, or a vendor data-model change large enough
  that the resulting diffs shouldn't generate a notification storm.

## Storage

### The `MetadataStore` trait

```rust
#[async_trait]
pub trait MetadataStore: Send + Sync {
    async fn lookup(&self, key: &FirmwareKey<'_>) -> Result<Option<FirmwareMetadata>, StoreError>;
    async fn latest_known(&self, vendor: &str, device_family: &str, hardware_targets: &[String]) -> Result<Option<FirmwareMetadata>, StoreError>;
    async fn upsert(&self, r: &FirmwareRef, meta: &FirmwareMetadata, run_id: Uuid) -> Result<(), StoreError>;

    async fn has_completed_baseline(&self, vendor_id: &str) -> Result<bool, StoreError>;
    async fn mark_baseline_complete(&self, vendor_id: &str) -> Result<(), StoreError>;
    async fn clear_baseline(&self, vendor_id: &str) -> Result<(), StoreError>;

    async fn start_run(&self, vendor_id: &str, kind: RunKind) -> Result<Uuid, StoreError>;
    async fn complete_run(&self, run_id: Uuid, outcome: RunOutcome) -> Result<(), StoreError>;

    async fn history(&self, key: &FirmwareKey<'_>) -> Result<Vec<FirmwareRevision>, StoreError>;
    async fn all_current(&self, vendor_id: &str) -> Result<Vec<FirmwareMetadata>, StoreError>;
    async fn resolve_one(&self, selector: &FirmwareSelector) -> Result<Option<FirmwareMetadata>, StoreError>;
    async fn resolve_many(&self, selector: &FirmwareSelector) -> Result<Vec<FirmwareMetadata>, StoreError>;
}
```

`upsert` is append-preserving, not a true overwrite: every observed
revision is retained in a permanent revision log; `lookup`/`latest_known`/
`all_current` return only the latest per entry. The revision insert and
the current-state upsert happen in one transaction, so a crash mid-write
can't desync current state from history.

### SQLite backend

Single-file, no server process, comfortably handles the expected volume
(thousands to low tens-of-thousands of entries), durable by default.
`SqliteStore::open` creates `database_path`'s parent directory
automatically if it doesn't exist yet — on a genuine first run it usually
doesn't, since nothing else creates it either, and without this the SQLite
open call fails with `SQLITE_CANTOPEN` ("unable to open database file")
before SQLite itself even gets a chance to create the file.

```sql
CREATE TABLE firmware_current (
    id              BLOB PRIMARY KEY,        -- surrogate UUID, for CLI addressing
    vendor          TEXT NOT NULL,
    device_family   TEXT NOT NULL,
    hardware_key    TEXT NOT NULL,           -- canonicalized, sorted hardware_targets
    version_raw     TEXT NOT NULL,
    version_scheme  TEXT NOT NULL,           -- 'semver' | 'vendor_numeric' | 'opaque'
    version_ordinal TEXT,                    -- zero-padded, dot-joined, SQL-sortable; NULL if Opaque
    source_url      TEXT NOT NULL,
    sha256          BLOB,
    release_date    TEXT,
    metadata_json   TEXT NOT NULL,           -- full FirmwareMetadata, serde-serialized
    first_seen_run_id BLOB NOT NULL,
    last_seen_run_id  BLOB NOT NULL,
    UNIQUE (vendor, device_family, hardware_key, version_raw)
);

CREATE TABLE firmware_revisions ( ... );  -- append-only log, same natural key + run_id + observed_at
CREATE TABLE runs ( ... );                -- id, vendor, kind, started_at, completed_at, outcome
CREATE TABLE baselines ( vendor TEXT PRIMARY KEY, completed_at TEXT NOT NULL );
```

A few deliberate choices worth knowing about:

- **A surrogate `id` (UUID) plus a `UNIQUE` natural key**, not the natural
  key as primary key. Typing four fields on a command line to address one
  row is clunky — `unearth --id <uuid>` needs just the one, copy-pasted
  from a `catalog` listing.
- **`version_ordinal` stored as a zero-padded, dot-joined string** so
  `ORDER BY version_ordinal` sorts correctly directly in SQL, without
  pulling every row into Rust just to sort them.
- **`metadata_json` duplicates what the indexed columns already hold.**
  The typed columns are what you index and query on; the JSON blob is what
  actually deserializes back into `FirmwareMetadata` on read. This means
  schema evolution on a less-critical field doesn't require a migration
  every time a plugin author adds new metadata.
- **Retention is permanent — no pruning.** Simple by design. Since it's
  kept forever, plugin authors should avoid stuffing large blobs (full
  release-notes HTML, say) into stored metadata; prefer a
  `release_notes_url` reference instead.
- **Concurrency**: WAL mode plus a shared `sqlx::SqlitePool` (not one
  connection per vendor task) so two vendor digs can run in parallel
  without `SQLITE_BUSY` errors under normal contention. `busy_timeout` is
  set so transient lock contention retries instead of erroring
  immediately — each transaction here is short (one upsert plus one
  revision insert), so this is cheap insurance, not a real bottleneck.
- **`latest_known`'s query** is `ORDER BY version_ordinal DESC, rowid DESC
  LIMIT 1`. SQLite sorts `NULL` last in a `DESC` ordering, so a row with a
  derivable ordinal is always preferred over an `Opaque` one — matching
  the refusal to compare `Opaque` versions described above. `rowid` is
  only a tiebreak for the all-`Opaque` case (insertion order, not a real
  timestamp) and only matters when `VersionDirection::between` was already
  going to report `Unordered` regardless of which row got picked.

### Binaries are never stored automatically

Scheduled digs persist metadata only — `discover` and `metadata`, never
`fetch`. Downloading a binary is a separate, explicit, manual operation via
`unearth` — see [CLI commands](#cli-commands).

## Transport and proxying

`ScrapeContext` carries transport configuration so plugins never construct
their own HTTP clients or think about proxying at all — they call
`ctx.http_client()` and get whatever the run was configured to use.

```rust
pub enum Transport {
    Direct,
    Socks5 { addr: SocketAddr },
    Tor(TorConfig),
}

pub enum CircuitIsolation {
    Shared,      // one Tor circuit for the whole run
    PerVendor,   // new circuit per vendor (default)
    PerRequest,  // new circuit per request — safest, slowest
}
```

- **Tor**: talks to an external `tor` daemon's SOCKS port via `reqwest`'s
  `socks` feature, using `socks5h://` (not `socks5://`) so DNS resolution
  happens over Tor too — otherwise the hostname being scraped leaks via a
  local DNS query, defeating a good chunk of the point. An embedded,
  daemon-free option (`arti-client`) is not implemented. `TorMode::Embedded`
  exists in the types, but building a context with it fails with
  `TransportError::EmbeddedTorUnavailable` instead of panicking, and the
  config loader can't select it. Findings from trying it (October 2026,
  `arti-client` 0.46):
  - It works: an in-process client bootstrapped in under 4 seconds and
    fetched `check.torproject.org/api/ip` over HTTPS through a Tor circuit
    (`IsTor: true`), with no `tor` daemon installed.
  - It is still a 0.x library with monthly breaking releases, and needs
    Rust 1.91 or newer.
  - It can't be added to this workspace as is: `arti-client` depends on
    `rusqlite`, which links `libsqlite3-sys` 0.34 or newer, while `sqlx` 0.8
    (used by `delve-store-sqlite`) pins `libsqlite3-sys` 0.30. Cargo allows
    one crate that links `sqlite3`, so resolution fails. `sqlx` 0.9 allows
    0.30.1 up to but not including 0.38, which resolves it (checked with
    `cargo check -p delve-store-sqlite`; the store code compiled unchanged).
  - The embedding program has to pick the `rustls` crypto provider itself
    (for example `rustls::crypto::ring::default_provider().install_default()`),
    or the first TLS use panics.
  - `ScrapeContext` hands plugins a `reqwest::Client`, which can't use an
    `arti-client` stream directly. The likely route is an in-process SOCKS5
    listener backed by `TorClient::connect`, used through the existing
    `socks5h://` path. That is not built.
- **Default circuit isolation is `PerVendor`**: balances not looking like
  a single abusive client to any one vendor against exhausting the exit
  node pool with a new circuit on every single request.
- **Per-vendor override**: some vendor portals/CDNs block Tor exit nodes
  outright, so `Transport` is configurable per vendor with a global
  default — see [Configuration reference](#configuration-reference).

## Credentials

`[vendors.credentials.<vendor_id>]` holds arbitrary key/value pairs — each
plugin documents which keys it needs (`vendor-cisco` expects
`client_id`/`client_secret` — an OAuth2 `client_credentials` pair from a
Cisco API Console application, which is how Cisco's actual Support APIs
authenticate; not a username/password). Deliberately untyped: different
vendors need different shapes (a client id/secret pair, a single API
token, a client cert path), and the framework has no way to know which in
advance.

A value of the form `env:VAR_NAME` is resolved from the process
environment at load time, so a config file can be committed or shared
without embedding secrets in it directly:

```toml
[vendors.credentials.cisco]
client_id = "your-api-console-client-id"
client_secret = "env:CISCO_CLIENT_SECRET"   # read from $CISCO_CLIENT_SECRET at load time
```

A referenced env var that isn't set is a hard config-load error, not a
silent empty string — that would just turn into a more confusing failure
later, inside whatever plugin tried to use it.

A vendor plugin reads these off `ScrapeContext`: `ctx.credential("client_id")`
returns `Option<&str>`; `ctx.require_credential("client_id")` fails
immediately with a clear `PluginError::Rejected` naming the missing key,
rather than letting a missing credential surface later as an opaque HTTP
401 from inside a request.

Non-secret plugin options go under `[vendors.settings.<vendor_id>]`
instead, with each value a string or a list of strings (for example
`vendor-unifi`'s `models`, see [Vendor: UniFi](#vendor-unifi)). They're
used as written, with no `env:` indirection. Plugins read them with
`ctx.setting(key)`, or `ctx.setting_list(key)`, which fails with a clear
error when a list setting was given as a single string.

## Rate limiting

`ScrapeContext::throttle()` enforces a minimum interval between requests
within one vendor's dig — the concrete enforcement behind the per-vendor
robots.txt/ToS crawl-delay review described in
[Compliance](#compliance-tos-and-robotstxt). Reviewing a vendor's terms
tells you what rate is acceptable; this is what actually keeps requests to
it.

```toml
[transport]
min_request_interval_ms = 1000   # default if unset — one request/sec

[transport.rate_limit_overrides]
cisco = 3000   # cisco's robots.txt specifies a slower crawl-delay
```

**This is opt-in per plugin, not automatic.** The framework can't
intercept every HTTP call a plugin makes — plugins hold the raw
`reqwest::Client` via `ctx.http_client()` and issue requests however their
`discover`/`metadata`/`fetch` logic needs to. So it's on each plugin
author to call `ctx.throttle().await` immediately before every outgoing
request, the same way `require_credential` is opt-in rather than
intercepted. `vendor-cisco`'s stub demonstrates the pattern in all three
trait methods — skipping the call means skipping the rate limit entirely
for that request.

The limiter is shared per vendor (one `ScrapeContext` per vendor per dig),
so concurrent requests fired by a plugin's own `discover()` — e.g.
paginating several pages at once — are still serialized to the configured
rate, not just requests made one after another in sequence.

## HTTP client hardening

Every `ScrapeContext`'s HTTP client sets a real `User-Agent` and finite
request/connect timeouts. Without these: a hung vendor connection could
block that vendor's entire dig indefinitely, and a default `reqwest` UA
string is exactly the kind of thing that gets scraper traffic silently
dropped or rate-limited.

```toml
[transport]
user_agent = "delve/0.1.0 (contact: ops@example.com)"
request_timeout_secs = 30   # default if unset
connect_timeout_secs = 10   # default if unset
```

All three are optional — unset falls back to
`delve_core::context::HttpClientConfig::default()`, which uses
`default_user_agent()` (`delve/<version>`, no contact info) and 30s/10s
timeouts. **The default UA is deliberately not production-ready** — it
identifies the tool but gives a vendor no way to reach out about it. Set a
real one with contact info before running against an actual vendor site;
this is the same honesty-about-what-this-is discipline as the
`tos_reviewed` gate, not just a technical nicety.

These settings are shared across every vendor in a run — unlike
`Transport`, there's no per-vendor override for UA/timeouts, since there's
no real scenario yet where one vendor needs different HTTP behavior than
another.

## Notifications

Fully decoupled from plugins — a `VendorPlugin` only ever produces
`FirmwareMetadata`. The engine's `EventBus` owns the decision of what's
new or changed and fans events out to subscribers.

```rust
pub enum FirmwareEvent {
    NewRelease { firmware: FirmwareMetadata, first_seen: DateTime<Utc> },
    UpdatedRelease {
        firmware: FirmwareMetadata,
        previous: FirmwareMetadata,
        changed_fields: Vec<FieldDiff>,
        version_direction: VersionDirection,
    },
}
```

- Built-in subscribers are feature-gated crates under `delve-subscribers/`:
  `subscriber-log` (always on — an audit trail independent of whether
  webhook/email delivery succeeds; each line names the vendor, device
  family, hardware and version), `subscriber-webhook` (POSTs a JSON
  payload) and `subscriber-email` (see [Email](#email)).
- Fan-out is concurrent; one slow or broken subscriber never blocks
  another — failures are logged and swallowed at the bus level, since a
  webhook being down shouldn't fail the whole dig.
- `version_direction` on `UpdatedRelease` matters because a downgrade is a
  meaningfully different signal to a subscriber than an upgrade — it can
  mean a vendor pulled a bad release, or that you're scraping a beta
  channel that doesn't publish monotonically. See
  [Baseline vs incremental digs](#baseline-vs-incremental-digs) for how
  this is actually computed (and a bug that once made it dead code).
- What counts as "changed" (hash/version only, vs. also release notes or
  hardware-target list) is currently hardcoded to hash-or-version, since
  that's what's unambiguously notification-worthy by default; widening
  this to a config knob is a reasonable future addition, not yet built.


### Email

`subscriber-email` sends a plain-text message over SMTP for each event,
built with the `subscriber-email` Cargo feature and configured under
`[subscribers.email]`:

```toml
[subscribers.email]
host = "smtp.example.com"
port = 587                              # default for the tls mode: 587, 465, or 25
tls = "starttls"                        # "starttls" (default) | "implicit" | "none"
from = "Delve <delve@example.com>"      # a bare address or "Name <address>"
to = ["ops@example.com", "sec@example.com"]
username = "delve"                      # username and password go together, or neither
password = "env:DELVE_SMTP_PASSWORD"    # "env:VAR_NAME" reads the environment, like vendor credentials
```

- `host`, `from` and `to` are required. Unknown fields are rejected, so a
  mistyped `passwrod` is an error and not a silently unauthenticated send.
- `tls = "starttls"` upgrades the connection and **fails** if the server
  won't, rather than falling back to plain text. `"implicit"` is TLS from
  the first byte (port 465). `"none"` is for a local relay only: credentials
  and messages cross the network in the clear.
- The subject names what happened and which device, for example `[delve]
  New firmware: unifi USW (USMINI) v2.1.6+762`, `[delve] Firmware updated:
  unifi USW (USMINI) v2.1.3+755 -> v2.1.6+762`, or `[delve] Firmware
  DOWNGRADED: ...` when `version_direction` is `Older`. The hardware goes in
  parentheses because a family can cover many models (UniFi's `USW` is every
  switch); it is left out when it would only repeat the family. A
  rebuild under the same version says so. The body lists the vendor, the
  product name when the plugin has one, device family, hardware, version,
  release date, SHA-256, source and release-notes URLs, and for updates the
  changed fields. The subject is one line, with
  control characters from vendor data replaced by spaces.
- A bad address, a half-set login, or an unset `env:` variable fails the
  `dig` at startup instead of running without the notifications you asked
  for. A server that is down fails only that message: the failure is logged
  and the dig carries on, like any other subscriber.
- **One email per event, no batching.** A dig that finds many releases at
  once sends many emails. A vendor's first dig is a silent baseline, so this
  mostly happens after adding models to a vendor without running
  `dig --vendor <id> --redig` once (see [Tracking only some
  models](#tracking-only-some-models)), or when a vendor publishes many
  releases at once.
- Without the Cargo feature, a `[subscribers.email]` section still parses
  but `dig` only logs a warning that it isn't compiled in.

#### Choosing an email service

Delve doesn't send mail itself and ships no sending account, so each person
running it supplies SMTP credentials. Use a service built for sending
program-generated mail, with credentials that can only send, and not a
personal mailbox (Gmail and Outlook need app passwords or OAuth, and are the
wrong fit for automated mail). Keep the secrets in environment variables with
`env:`. Providers generally require you to verify the `from` address or
domain before they will send, so follow the provider's setup guide for that.

The settings below come from each provider's documentation (checked October
2026). Delve's tests never send through any of them, so try one message
before relying on it.

**Postmark**: the Server API Token is both the username and the password.

```toml
[subscribers.email]
host = "smtp.postmarkapp.com"
port = 587                              # STARTTLS only; 25 and 2525 also work
from = "delve@example.com"
to = ["ops@example.com"]
username = "env:POSTMARK_SERVER_TOKEN"
password = "env:POSTMARK_SERVER_TOKEN"
```

**Resend**: the username is the literal `resend` and the password is an API key.

```toml
[subscribers.email]
host = "smtp.resend.com"
port = 587                              # or tls = "implicit" with port 465
from = "delve@example.com"
to = ["ops@example.com"]
username = "resend"
password = "env:RESEND_API_KEY"
```

**Amazon SES**: the endpoint and the credentials are specific to one AWS
region.

```toml
[subscribers.email]
host = "email-smtp.us-east-1.amazonaws.com"   # your region's SMTP endpoint
port = 587                                    # or tls = "implicit" with port 465
from = "delve@example.com"
to = ["ops@example.com"]
username = "env:SES_SMTP_USERNAME"
password = "env:SES_SMTP_PASSWORD"
```

Create the credentials in the SES console under "SMTP settings" > "Create
SMTP credentials". The SMTP password is derived for you and is not your AWS
secret access key. A new SES account starts in a sandbox that only sends to
verified addresses (at most 200 messages a day and 1 a second) until you
request production access.

## CLI commands

Styled as an archaeological dig: `dig` scrapes a vendor site (the
excavation), `catalog` lists what's been found, `provenance` traces one
find's documented history, `unearth` pulls the actual physical artifact
out.

### `dig`

Performs the actual scraping. Iterates enabled vendors (all, or filtered
via `--vendor`), automatically determining baseline vs. incremental per
vendor — see [Baseline vs incremental digs](#baseline-vs-incremental-digs).

```
delve dig                          # all enabled vendors
delve dig --vendor cisco           # just one vendor
delve dig --vendor cisco --redig   # clear cisco's baseline first, then dig fresh
delve dig --vendor cisco --allow-unreviewed   # dev only: run despite tos_reviewed: false
```

`--allow-unreviewed` is a deliberate escape hatch for exercising a plugin's
`discover`/`metadata` against a real site before its ToS review is recorded.
It logs a loud warning, writes to the real store like any other dig, and
only works together with `--vendor` (the command errors otherwise), so an
unfiltered or scheduled `dig` can never pick it up. It doesn't change what
the plugin reports for `tos_reviewed` — read the vendor's ToS and
`robots.txt` first, as described in [Compliance](#compliance-tos-and-robotstxt).

### `catalog`

Prints currently-known firmware attributes — no downloading. Same selector
flags as `unearth` (`--vendor`, `--device-family`, `--hardware`,
`--version`/`--latest`, `--id`), narrowing what's printed rather than
what's fetched.

```
delve catalog --vendor cisco
delve catalog --vendor cisco --device-family isr4000 --long
```

Default output is a concise table of the most useful fields; `--long`/`-l`
prints every field for each matched entry.

### `provenance`

Shows the revision log for one firmware entry over time — every time that
exact `(vendor, device_family, hardware, version)` combination was
re-observed across digs, not just its current state. Requires enough
specificity to resolve to exactly one entry (or `--id`).

```
delve provenance --vendor cisco --device-family isr4000 --version 17.9.4a
delve provenance --id <uuid>
```

Useful for questions like "did this hash actually change between two
dates, or did we just re-scrape the same file twice" — and for an audit
trail independent of whether a notification fired (baseline-dig
observations are silent but still land in the revision log).

### `unearth`

The only command that touches binary bytes. Direct pass-through to the
owning plugin's `fetch()` — no engine, `EventBus`, or diff logic involved.

```
delve unearth --vendor cisco --device-family isr4000 --version 17.9.4a --out ./downloads/
delve unearth --id <uuid> --out ./downloads/
```

After download, the sha256 is verified against the stored hash by
default and fails loudly on mismatch, since this is scraped data from
possibly-unofficial sources; `--no-verify` skips that check for cases
where a vendor legitimately replaced a binary post-hoc and the stored hash
is stale.

## Configuration reference

TOML, loaded from `~/.config/delve/config.toml` by default (respecting
`$XDG_CONFIG_HOME`), or from an explicit `--config <path>` override on any
command. A missing file at the *default* path is not an error — a first
run works with no setup at all; a missing file at an explicitly-given
`--config` path is an error, since the user asked for it by name.

`database_path` defaults to a *different* directory than the config file —
the SQLite database is application data, not configuration, so it
defaults under `$XDG_DATA_HOME` (`~/.local/share/delve/delve.sqlite` on
Linux) rather than alongside `config.toml`. The database's parent
directory is created automatically if missing (see the "Storage" section);
the config directory is not, since `delve` only ever reads `config.toml`,
never writes it.

```toml
database_path = "/custom/path/delve.sqlite"   # default: ~/.local/share/delve/delve.sqlite ($XDG_DATA_HOME) — parent dirs are created automatically

[vendors]
enabled = ["cisco"]                             # narrows compiled-in vendors; empty = all

[vendors.credentials.cisco]
client_id = "your-api-console-client-id"
client_secret = "env:CISCO_CLIENT_SECRET"

[vendors.settings.unifi]                        # non-secret, per-plugin settings
models = ["U7PG2", "USMINI"]                    # see "Tracking only some models"

[transport]
default = "direct"                              # "direct" | "tor" | { socks5 = { addr = "..." } }
user_agent = "delve/0.1.0 (contact: ops@example.com)"
request_timeout_secs = 30
connect_timeout_secs = 10
min_request_interval_ms = 1000

[transport.overrides]
cisco = "direct"                                # cisco's portal blocks Tor exits

[transport.rate_limit_overrides]
cisco = 3000                                     # cisco's robots.txt specifies a slower crawl-delay

[subscribers.webhook]
url = "https://example.com/hooks/delve"

[subscribers.email]                              # needs the subscriber-email feature; see "Email"
host = "smtp.example.com"
from = "delve@example.com"
to = ["ops@example.com"]
```

Every field is optional; the framework falls back to a sensible default
for anything unset (see the relevant section above for each default
value).

## Compliance: ToS and robots.txt

Two distinct checks, done per vendor before a plugin ships, and not
enforced by the framework itself — the framework has no way to know a
vendor's terms; this is a human review step:

- **`robots.txt`** — the crawling convention published at
  `https://vendor.com/robots.txt`. Not legally binding on its own, but
  ignoring it risks IP/user-agent blocking, and it's the first thing to
  check per vendor. Its `Crawl-delay` (where published) is what should
  drive that vendor's `[transport.rate_limit_overrides]` entry — see
  [Rate limiting](#rate-limiting).
- **Terms of Service** — a vendor's actual legal terms, which sometimes
  explicitly prohibit automated access regardless of what `robots.txt`
  says. Worth an actual read per vendor before enabling scheduled scraping
  against it.

`PluginCapabilities.tos_reviewed` is where this review is recorded in
code: `dig` skips any vendor whose plugin reports `tos_reviewed: false`
entirely, rather than running it and hoping someone remembers to check
first. `vendor-cisco` deliberately leaves this `false` — see its doc
comment — until Cisco's actual robots.txt and ToS have been read.
To exercise `discover`/`metadata` against a real site while the flag is
`false`, use `dig --vendor <id> --allow-unreviewed` (see [`dig`](#dig)).

## Vendor: UniFi

`vendor-unifi` (vendor id `unifi`) tracks Ubiquiti UniFi firmware from
`https://fw-update.ui.com/api/firmware` — the API UniFi devices and
controllers themselves poll for updates. It needs no credentials. Unlike
`vendor-cisco`, every endpoint and field it uses was checked against live
responses (September 2026), and its test fixtures are real records from
those responses. The API is **undocumented**, though, so Ubiquiti can
change it without notice; `vendor-unifi/src/api.rs`'s module doc comment
lists exactly what was verified.

### Scope: `unifi-firmware` only

The plugin tracks the API's `unifi-firmware` product only: **UniFi network
devices** — access points, switches, gateways, and older Cloud Keys
(about 200 models). The same API lists many other Ubiquiti products under
different names, and **none of these are tracked yet**:

- UniFi OS consoles: `unifi-dream` (Dream Machines, Cloud Gateways),
  `unifi-nvr`, `unifi-drive` (UNAS), `unifi-cloudkey` (newer Cloud Keys)
- Protect cameras (`uvc`) and other Protect devices
- Access, Talk, Connect and other UniFi application devices
- Non-UniFi lines (airMAX, airFiber, EdgeRouter, EdgeSwitch, UISP, ...)

Adding one is mostly a matter of widening the product filter in `api.rs`,
but each has its own version quirks and should be checked the same way
`unifi-firmware` was before it's enabled.

### How API fields map to `FirmwareMetadata`

| Field | API source |
|---|---|
| `device_family` | the model's product line, e.g. `UAP` for `U7PG2` (UAP-AC-Pro) and `USW` for `USMINI` (USW-Flex-Mini), or the model code itself when it has no line — see [Product lines](#product-lines) |
| `hardware_targets` | `[platform]` — always the model code, e.g. `U7PG2` |
| `display_name` | the model's product name, e.g. "Switch Flex Mini" — see [Product lines](#product-lines); `None` for the two codes nobody has named |
| `version` | `version` (e.g. `v6.6.77+15402`); ordinal from `version_major`/`minor`/`patch`/`build` — see `version.rs` |
| `release_date` | `release_date` when present (rare), otherwise `created` (upload time) |
| `sha256` | `sha256_checksum` — verified to match the downloaded file |
| `release_notes_url` | `_links.changelog` — never present for `unifi-firmware` today |
| `source_url` | the record's own API URL (`_links.self`) |
| download (`fetch`) | `_links.data`, a direct file URL needing no login |

### Product lines

`device_family` is a product line, so `catalog --device-family USW` selects
every switch in one go. The model code stays in `hardware_targets`, so two
models in one line are still distinct entries.

```
delve catalog --vendor unifi --device-family USW            # every switch
delve catalog --vendor unifi --device-family UAP --latest   # newest firmware for each older access point
```

| Line | Covers | Models |
|---|---|---|
| `USW` | switches | 85 |
| `UAP` | access points before Wi-Fi 6: the AC generation, nanoHD, FlexHD, BeaconHD, XG and older | 26 |
| `U6` | access points named "U6 ..." (Wi-Fi 6) | 15 |
| `U7` | access points named "U7 ..." | 15 |
| `E7` | access points named "E7 ..." | 7 |
| `UXG` | gateways named "Gateway ..." (Lite, Max, Pro, Enterprise, Fiber) | 5 |
| `USG` | Security Gateways | 3 |
| `UCK` | Cloud Keys | 3 |
| `UDM` | Dream Machine and Dream Machine Pro | 2 |
| `UX` | Express | 2 |

The counts are model codes in the release channel in October 2026.

**Where the mapping comes from.** The firmware API has no product-line
field, and the model codes don't say what a device is: `USWDA23` is a UPS,
`UDMA69B` is the Express 7 and not a Dream Machine, and `UDMB` is an access
point. So the table in `vendor-unifi/src/product_line.rs` is built from
Ubiquiti's own published device list
(`https://static.ui.com/fingerprint/ui/public.json`), looking each code up by
name or hex system ID and using the device type and product name. The
module's doc comment has the exact rules. As a cross-check, models that share
one firmware image (the API's `models` field) must be in one line, and none
of the nine such groups spans two.

**Models without a line.** 34 of the 197 codes are deliberately left out and
keep their model code as `device_family`, as before: power products (UPS,
SmartPower, power distribution), bridges, LTE and U5G devices, travel
routers, a cable modem, `U7UKU` (a "Swiss Army Knife" that fits none of
the lines above), and two codes Ubiquiti's list doesn't know (`USMULT` and
`UXGPROV2`). A wrong guess would silently put a device in the wrong line,
while leaving it out loses nothing.

**Product names.** Each model also carries a product name (`USMINI` is
"Switch Flex Mini", `U7PG2` is "Access Point AC Pro"), taken from the same
Ubiquiti device list, for 195 of the 197 codes. It is display text only: it
is not part of the identity key and is not compared when deciding what
changed. It shows in `catalog --long`, in the log line (`name=`), and as a
`Product:` line in the email body.

It is deliberately **left out of the email subject and the default `catalog`
table**. Tried there, it made UniFi subjects a median 93 characters long (up
to 109) against 74 (at most 78) without it, so 177 of 187 models would be
folded or cut off in an inbox list, and the version moved past column 60 for
129 of them, which is where a preview cuts it off. In `catalog`, a NAME column
widened the table from 134 to 171 characters. The model code stays everywhere
it was.

**Keeping it current.** The ignored live test
`product_line::tests::live_every_model_code_is_mapped_or_known_unmapped`
fails with the list of any model code Ubiquiti has added that is in neither
the table nor the known-unmapped list. Run it with
`cargo test -p vendor-unifi -- --ignored`.

#### Upgrading an existing database

Moving a model into a line changes its stored identity key, so after
upgrading:

1. **Run `delve dig --vendor unifi --redig` once.** Without it, every model
   that moved looks new, and the dig announces its whole history. In a test
   with 2 mapped models that was 51 notifications. With `--redig` it was 0.
   Models without a line are unaffected.
2. **Optionally remove the old entries.** The old rows stay in the store,
   so `catalog` shows each moved model twice, once under its model code and
   once under its line (127 entries instead of 76 in the same test). The
   history of observations is kept either way. To delete only the old
   entries, after the `--redig` dig and with a backup of the database:

   ```sql
   DELETE FROM firmware_current
   WHERE vendor = 'unifi'
     AND device_family = hardware_key
     AND EXISTS (
       SELECT 1 FROM firmware_current AS n
       WHERE n.vendor = 'unifi'
         AND n.hardware_key = firmware_current.hardware_key
         AND n.version_raw = firmware_current.version_raw
         AND n.device_family <> firmware_current.device_family);
   ```

   It only removes an old-style row when a copy of the same model and
   version exists under a line, so it can't remove the only copy. A fresh
   database needs none of this.

### Requests per dig

By default, `discover()` makes **one** request: the API ignores `offset`,
so there is no real pagination, and a single request with a large `limit`
returns all ~3,400 release records (about 3 MB, or about 530 KB on the
wire, since responses are gzip-compressed). It caches the full records in
memory, and `metadata()` answers from that cache instead of requesting
each record, removing each record as it goes so the cache empties as the
dig stores entries — otherwise a dig would take about an hour at the default one
request per second. A response that fills the whole `limit` is treated as
possibly truncated and fails the dig rather than silently missing records.
`unearth` makes two requests: the record (the cache is empty in a fresh
process), then the file.

### Tracking only some models

To track only the devices you own, list their model codes (the API's
`platform`, the same value `catalog` shows as `DEVICE_FAMILY`):

```toml
[vendors.settings.unifi]
models = ["U7PG2", "USMINI", "UXGPRO"]
```

`discover()` then makes one request per model instead of one for
everything. A model with no release firmware fails the dig, since that
almost always means a mistyped code, which would otherwise silently track
nothing.

**Changing the list changes what a dig sees:**
- **Adding a model** after the first dig makes its whole release history
  look new, so the next dig notifies about every version it has ever had.
  Run `delve dig --vendor unifi --redig` once after adding models to store
  them silently instead. That dig is silent for every model, so it also
  won't notify about a genuine new release that happens to land in it.
- **Removing a model** stops updating it, but its entries stay in the
  store and in `catalog`.

**Choosing between the two modes.** Measured against the live API
(September 2026) with the default one-request-per-second rate limit, using
the release binary on a fresh database. The model sets are spread evenly
across the ~200 model codes; transfer sizes are estimates from gzipping
each response's JSON.

| Models | Requests | Records | Transfer | Dig time | Peak memory |
|---|---|---|---|---|---|
| All (default) | 1 | 3,374 | ~546 KB | 2.4 s | 43 MiB |
| 1 | 1 | 16 | ~3 KB | 0.2 s | 27 MiB |
| 5 | 5 | 109 | ~20 KB | 4.2 s | 27 MiB |
| 20 | 20 | 338 | ~62 KB | 19.2 s | 27 MiB |
| 50 | 50 | 829 | ~153 KB | 49.5 s | 27 MiB |

Incremental digs took the same time as the baseline digs shown here.
Per-model digs always transfer less and use less memory, but they're
throttled to one request per second, so a dig takes about a second per
model. Past 3 models, a per-model dig is slower than the single request
for everything. List models when you want to track only those devices
(a smaller store, `catalog` output and notifications), not for speed.

### Terms of service

`tos_reviewed` is `true`, set by the project owner in September 2026 after
this review:

- **robots.txt:** neither API host (`fw-update.ui.com`,
  `fw-download.ubnt.com`) has one, and `www.ui.com`'s allows all crawlers.
- **Terms of Service** (`https://www.ui.com/legal/termsofservice/`): no
  clause about automated access or scraping. They do limit use to
  "personal use … to use, manage and monitor Your Products", forbid
  redistributing or republishing their content, and forbid interfering
  with their servers.

Tracking metadata and downloading images for your own use fit within
that. **Redistributing downloaded images does not**, and the default one
request per second plus one list request per dig keeps load on
Ubiquiti's servers negligible. Re-check the terms if they change or if
the plugin's use changes.

### Notes to revisit

- **Beta firmware.** Only the `release` channel is tracked (`api::CHANNEL`).
  The API also has `beta-public`. Tracking it needs a way to configure
  channels, and a decision on identity: some versions appear in both
  channels with identical files, and `select_records` currently keeps just
  the newest record per model and version.
- **Request efficiency.** One request per dig (~530 KB gzip-compressed,
  ~3 MB decoded) is fine today. `/api/firmware-latest` returns only the
  newest version per model (~200 records) and is deliberately not used: it
  would miss old versions Ubiquiti re-publishes, and `dig` exists to notice
  exactly those. Listing `models` is the way to transfer less (see
  "Choosing between the two modes" above).

## Naming

`delve` — an archaeology-adjacent theme: "to dig into" doubling as "to
investigate deeply," which also drove the CLI command names above (`dig`,
`catalog`, `provenance`, `unearth`). `provenance` in particular is more
than a theme fit — it's the actual archival/museum term for a documented
chain of origin, which is a more precise name for what that command shows
than a generic "history" would have been.

## Known gaps

1. **`vendor-cisco`'s API integration is unverified.** `discover`/
   `metadata` are implemented against Cisco's Software Suggestion API —
   real OAuth2 `client_credentials` auth, real rate limiting and error
   handling, real (tested) IOS-XE version parsing — but the actual
   endpoint URLs and JSON response field names in `api.rs`/`auth.rs` were
   written with no network access to check them against Cisco's live API,
   and are marked `// VERIFY:` throughout both files. Specifically still
   open:
   - The OAuth2 token endpoint (`auth.rs::TOKEN_URL`) and token response
     field names.
   - The suggestion list/detail endpoint paths (`api.rs::suggestions_list_url`,
     `suggestion_detail_url`) and whether the per-ID detail endpoint
     exists at all — `metadata()` currently assumes it does, to avoid
     re-fetching an entire device family's release list for every single
     version.
   - The actual checksum type Cisco's API returns, if any — `sha256` is
     left `None` throughout rather than populated from an unconfirmed
     field, since Cisco's download pages have historically shown MD5/SHA512,
     not SHA256, and a wrong-but-present hash would silently break
     `unearth`'s default verification.
   - `fetch()` isn't implemented at all — getting an actual downloadable
     binary URL out of Cisco likely needs a separate API call beyond the
     Suggestion API, and guessing at that flow blind risks silently
     downloading the wrong file, which is worse than a clear
     `Unimplemented` error.

   None of this blocks iterating on the plugin's structure, error
   handling, or the rest of the framework — but don't point it at a real
   Cisco account and expect it to work without checking each `// VERIFY:`
   marker first.

2. **`vendor-cisco` doesn't have `tos_reviewed: true`.** See
   [Compliance](#compliance-tos-and-robotstxt) (`vendor-unifi`'s review is
   recorded in [Vendor: UniFi](#vendor-unifi)). Don't flip
   `vendor-cisco`'s flag without actually reading Cisco's robots.txt and
   ToS first. To exercise `discover`/`metadata` against a real site while
   the flag is `false`, use `dig --vendor cisco --allow-unreviewed`.

## Test coverage

`delve-core` and `delve-store-sqlite` both have unit/integration test
modules (`cargo test --workspace`). Writing these tests surfaced a real
bug in the engine's diff logic — the `lookup`-vs-`latest_known` split
described in [Baseline vs incremental digs](#baseline-vs-incremental-digs)
exists because of it, not as a design decision made up front.

- **`delve-core/src/model.rs`**: `hardware_key` canonicalization,
  `VersionKey` equality/ordering, `VersionDirection` — including the
  Opaque-vs-Opaque case that must report `Unordered` rather than guess.
- **`delve-core/src/engine.rs`**: the full baseline/incremental dig loop
  against mock `VendorPlugin`/`MetadataStore`/`Subscriber`
  implementations — baseline silence, `NewRelease`/`UpdatedRelease`/
  no-event cases, `--redig`, the partial-failure correctness note (a dig
  that dies mid-scrape must not mark the baseline complete), and that
  `source_url` gets populated from the discovering `FirmwareRef` rather
  than whatever the plugin's `metadata()` response happened to contain.
- **`delve-store-sqlite/src/lib.rs`**: real SQLite-backed tests (in-memory
  DB) — upsert/lookup round-tripping, revision history accumulation,
  per-vendor baseline isolation, `resolve_one`'s `Ambiguous`/`NotFound`
  cases, `resolve_many`'s `--latest` resolution including the
  numeric-vs-lexicographic ordering case and groups with no derivable
  ordinal, and that `source_url` round-trips through storage independent
  of the originating `FirmwareRef`'s own URL.
- **`delve-core/src/context.rs`**: `ScrapeContext::credential`/`require_credential`
  — present/missing cases and the error message naming the missing key —
  plus `HttpClientConfig` defaults and successful client construction with
  a custom UA/timeouts, `resolve_rate_limit`'s default/override behavior,
  and `throttle()`'s actual timing behavior (first call never waits, an
  immediate second call waits out the interval, no wait once enough time
  has passed, concurrent callers still serialize to the configured rate) —
  tested with `tokio::time::pause`/`advance` rather than real sleeping.
- **`delve-cli/src/config.rs`**: TOML parsing defaults, `[transport]`
  resolution (Direct/Tor/Socks5, per-vendor overrides), the
  malformed-address error case, credential resolution (literal values,
  `env:` indirection, missing-env-var error, per-vendor isolation), and
  `http_client_config`/`rate_limits` (both fall back to framework defaults
  when unset, honor explicit overrides, and confirm setting one field
  doesn't clobber the others').
- **`delve-cli/src/commands/unearth.rs`** (`FileSink`/`finalize_sha256`):
  the real `sha2` crate produces the correct digest for the standard
  SHA-256("abc") and SHA-256("") test vectors, and hashes multiple written
  chunks as one contiguous stream (matching a one-shot hash of the same
  concatenated bytes) rather than just the last chunk written.
- **`delve-vendors/vendor-cisco`**: everything that's testable without a
  live Cisco endpoint. `version.rs` — IOS-XE dotted-version parsing
  (numeric-vs-lexicographic ordering, rebuild-letter ordering, malformed
  input, and confirming classic IOS train notation deliberately fails to
  parse rather than producing a fabricated ordering). `auth.rs`'s
  `TokenCache` — cache hit/miss, the early-renewal safety margin actually
  taking effect before the token's literal expiry, and a
  shorter-than-safety-margin TTL saturating instead of panicking (split
  out as pure functions from the actual HTTP call specifically so this is
  testable without a network dependency). `api.rs` — the list/detail JSON
  parsing and mapping logic against hand-written fixtures matching the
  module's *assumed* schema: a passing test here proves the code is
  internally consistent, not that it matches Cisco's real API (see the
  module's doc comment and the "Known gaps" entry on this).
- **`delve-plugin-testkit`**: the checks themselves, run against a toy
  plugin that is made to break each rule in turn — wrong vendor id, no
  refs, an `Opaque` version with an ordinal, a blank hardware target, a
  plugin that skips `throttle()`, a `discover()` that fails on a bad
  record, and too few requests to measure pacing — to prove each check
  fails when it should and passes a conforming plugin. Also the mock
  server answering from its handler and recording each request.
- **`delve-subscribers/subscriber-log`**: a new-release line and an update
  line each name the vendor, device family, hardware and version, checked by
  capturing what the subscriber actually logs.
- **`delve-subscribers/subscriber-email`**: the subject and body for each
  event kind (new, updated newer/older/unordered, same-version rebuild), a
  newline in vendor data not being able to add header lines, address and
  recipient validation, a message reaching every recipient through a stub
  transport, and real SMTP: a fake in-process server receiving the right
  envelope, subject and body, and a server that is down giving a delivery
  error. `delve-cli`: `[subscribers.email]` parsing (defaults, each `tls`
  mode, unknown fields rejected), `env:` resolution and half-set logins,
  and a real `dig` against a mock vendor whose releases change, mailing
  one message per event and nothing on the baseline.
- **`delve-vendors/vendor-unifi`**: list/detail parsing and field mapping
  against **real** API records (`src/fixtures/`), skipping a single
  malformed record without failing the rest, dropping records with no
  file, other products and other channels, keeping only the newest
  duplicate of a model and version, SHA-256 decoding, version ordinals
  (numeric builds, Cloud Key git-hash builds, pre-releases as Opaque), and
  `metadata()` being served from the discover cache without a request, and
  removing each record as it uses it.
  The plugin also passes all four testkit conformance checks against a
  mock server, including with unparseable records in the list. Product
  lines: known models in their line, codes whose prefix misleads (`USWDA23`,
  `UDMA69B`, `UDMB`), a model with no line keeping its code, two models in
  one line staying distinct by hardware, and the table being sorted, free of
  duplicates and limited to the documented lines, and product names (known,
  unmapped models still named, an unknown model getting none). The ignored live test
  checks every model code Ubiquiti lists against the table.
  `discover` runs against a local mock of the list API: one request by
  default, one request per distinct model with `models` set, dropping
  other models if the server ignores the model filter, and failing on a
  model with no firmware or an empty or non-list `models` setting.
  Three live tests are marked `#[ignore]` so they don't run by default or
  in CI; run them with `cargo test -p vendor-unifi -- --ignored`. One runs
  `discover` and `metadata` over every release record; one runs a
  per-model `discover`; the third downloads a ~500 KB image with an empty
  cache (as `unearth` does) and checks it against the published SHA-256.

- **`delve-cli/src/commands/{dig,catalog,provenance}.rs`**: each command
  driven end to end against the real engine and an in-memory
  `SqliteStore`, with mock vendor plugins (`commands/test_support.rs`)
  whose releases change between digs, so nothing touches the network.
  `dig`: the silent baseline, `NewRelease`/`UpdatedRelease` (version bump
  and same-version rebuild), `--redig`, skipping vendors without
  `tos_reviewed` (and `--allow-unreviewed` running a named one, but never
  an unfiltered dig), `--vendor` and `[vendors].enabled` narrowing, and
  unknown/no-vendor errors. `catalog`: the default table and `--long`
  output, each selector flag, `--latest` ordering numerically, and the
  printed ids addressing exactly one entry. `provenance`: one line per
  observation in order, `--id` alone resolving the entry (and an unknown
  id failing), and ambiguous or unmatched selectors failing.

Not yet covered: `unearth.rs` beyond `FileSink` (resolution, context
building), which is mostly wiring. `vendor-cisco`'s actual HTTP
integration (the `discover`/`metadata` methods themselves, end to end)
isn't tested at all — only the pieces factored out to be testable without
a live Cisco endpoint are; the untested glue between them is exactly the
part most likely to need fixing once the `// VERIFY:` markers are checked.

## Not yet scaffolded

- The dynamic (WASM/`abi_stable`) plugin tier described in
  [Plugin architecture](#plugin-architecture) — intentionally deferred,
  not needed for v1
