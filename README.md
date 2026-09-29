# delve

An embedded firmware scraper and notification framework. `delve` discovers
firmware releases across embedded device vendors, tracks their metadata
over time, and notifies configured subscribers when something new or
changed shows up. Vendor support is pluggable — the core engine has no
built-in knowledge of any specific vendor's portal, API, or firmware
format.

**Status: scaffold, not a verified build.** Every module compiles
conceptually and has real test coverage. `vendor-cisco`'s `discover`/
`metadata` are implemented against Cisco's Software Suggestion API — real
control flow (OAuth2 auth, rate limiting, error handling), but the exact
API endpoints and response shapes are **unverified**, since this was
written with no network access to check them against Cisco's live API.
`fetch` (actual binary download) remains unimplemented — see the
"Plugin architecture" and vendor-cisco's own module docs for exactly
what's confirmed-safe vs. best-effort. Nothing here has been built against
real crates.io dependencies in this environment either. Treat this as a
first draft to verify and correct against Cisco's actual API, not a
working integration yet.

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
delve-vendors/
  vendor-cisco/          discover/metadata implemented but unverified — see below
delve-subscribers/
  subscriber-log/        always-on audit-trail subscriber
  subscriber-webhook/     POSTs events to a configured URL, feature-gated
```

`delve-store-sqlite` is its own crate rather than folded into `delve-core`,
so `delve-core` doesn't pull in `sqlx` as a dependency — anything
implementing `MetadataStore` against a different backend later only needs
to depend on `delve-core`.

Build the CLI with a vendor and the webhook subscriber:

```
cargo build --features "vendor-cisco,subscriber-webhook" -p delve-cli
```

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
all-vendors = ["vendor-cisco"]
```

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
`catalog`, `provenance`, and `unearth`) only return `FirmwareMetadata`. Without
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
  daemon-free option (`arti-client`) is a documented future possibility
  but not implemented — its maturity relative to the C `tor`
  implementation is worth checking again before making it the default.
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
  webhook/email delivery succeeds) and `subscriber-webhook` (POSTs a JSON
  payload).
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
```

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
There's currently no way to exercise `discover`/`metadata` against a real
site while the flag is `false` short of temporarily flipping it, which is
its own small gap — see [Known gaps](#known-gaps).

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

2. **No vendor has `tos_reviewed: true`.** See
   [Compliance](#compliance-tos-and-robotstxt) — don't flip
   `vendor-cisco`'s flag without actually reading Cisco's robots.txt and
   ToS first. There's also still no dev escape hatch for exercising
   `discover`/`metadata` against a real site while the flag is `false` —
   right now `dig` hard-skips, rather than warn-and-continue.

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

Not yet covered: the rest of `delve-cli`'s command handlers (`dig.rs`,
`catalog.rs`, `provenance.rs`) — these are thin enough to mostly be
integration-tested once a real vendor plugin exists to drive them against,
rather than mocked at the unit level. `unearth.rs`'s `FileSink` was worth
testing directly since it's real, non-trivial logic (streaming hash state)
independent of any vendor plugin; the rest of that file (resolution,
context building) is mostly wiring. `vendor-cisco`'s actual HTTP
integration (the `discover`/`metadata` methods themselves, end to end)
isn't tested at all — only the pieces factored out to be testable without
a live Cisco endpoint are; the untested glue between them is exactly the
part most likely to need fixing once the `// VERIFY:` markers are checked.

## Not yet scaffolded

- `subscriber-email` — no crate here yet, copy `subscriber-webhook`'s
  shape when needed
- The dynamic (WASM/`abi_stable`) plugin tier described in
  [Plugin architecture](#plugin-architecture) — intentionally deferred,
  not needed for v1
- `delve-plugin-testkit` — a conformance-test harness for vendor crates
  (a mock `ScrapeContext` plus a standard assertion suite). `vendor-cisco`
  now exists as a first real attempt, so this is worth building against
  its actual failure modes rather than guessed at up front — but its
  `discover`/`metadata` are still unverified against a live endpoint (see
  "Known gaps"), so the harness's assertions would currently only be
  checking against an unconfirmed assumption of what "correct" looks like
