//! The checks every vendor plugin must pass. Each one panics with a message
//! that names the vendor and the broken rule.

use std::collections::HashSet;
use std::time::Duration;

use delve_core::context::ScrapeContext;
use delve_core::model::{FirmwareRef, VersionScheme};
use delve_core::plugin::VendorPlugin;

use crate::server::MockServer;

/// Runs `discover()` and checks the refs it returns:
///
/// - the call succeeds and returns at least one ref;
/// - every ref's `vendor` equals `vendor_id()`, since the engine stores
///   entries under it;
/// - every `device_family` is non-blank;
/// - every `source_url` is distinct, since it is the cache key and the
///   address `unearth` rebuilds a ref from.
///
/// Returns the refs for the next check.
///
/// # Panics
///
/// If any rule above is broken.
pub async fn discover_conforms(plugin: &dyn VendorPlugin, ctx: &ScrapeContext) -> Vec<FirmwareRef> {
    let id = plugin.vendor_id();
    let refs = plugin
        .discover(ctx)
        .await
        .unwrap_or_else(|e| panic!("{id}: discover() failed: {e}"));
    assert!(
        !refs.is_empty(),
        "{id}: discover() returned no refs; the test fixtures should yield at least one"
    );

    let mut urls = HashSet::new();
    for r in &refs {
        assert_eq!(
            r.vendor, id,
            "{id}: discover() returned a ref for vendor '{}', not vendor_id()",
            r.vendor
        );
        assert!(
            !r.device_family.trim().is_empty(),
            "{id}: discover() returned a ref with a blank device_family ({})",
            r.source_url
        );
        assert!(
            urls.insert(r.source_url.as_str()),
            "{id}: discover() returned two refs with the same source_url {}",
            r.source_url
        );
    }
    refs
}

/// Runs `metadata()` for every ref and checks the result:
///
/// - the call succeeds;
/// - `version.raw` is non-blank;
/// - the version's scheme agrees with its ordinal: an `Opaque` version has
///   no ordinal, and an ordinal is never empty or attached to `Opaque`;
/// - no `hardware_targets` entry is blank.
///
/// The engine overwrites `vendor`, `device_family` and `source_url` from the
/// ref, so they are not checked here.
///
/// # Panics
///
/// If any rule above is broken.
pub async fn metadata_conforms(
    plugin: &dyn VendorPlugin,
    ctx: &ScrapeContext,
    refs: &[FirmwareRef],
) {
    let id = plugin.vendor_id();
    for r in refs {
        let meta = plugin
            .metadata(ctx, r)
            .await
            .unwrap_or_else(|e| panic!("{id}: metadata() failed for {}: {e}", r.source_url));
        let version = &meta.version;

        assert!(
            !version.raw.trim().is_empty(),
            "{id}: metadata() returned a blank version for {}",
            r.source_url
        );
        match (&version.scheme, &version.ordinal) {
            (VersionScheme::Opaque, Some(ordinal)) => panic!(
                "{id}: version '{}' is Opaque but has ordinal {ordinal:?}; an Opaque version \
                 has no ordering",
                version.raw
            ),
            (_, Some(ordinal)) => assert!(
                !ordinal.is_empty(),
                "{id}: version '{}' has an empty ordinal; use None when there is no ordering",
                version.raw
            ),
            (_, None) => {}
        }
        for target in &meta.hardware_targets {
            assert!(
                !target.trim().is_empty(),
                "{id}: version '{}' lists a blank hardware target",
                version.raw
            );
        }
    }
}

/// Checks that the plugin called `ctx.throttle()` before each request, by
/// looking at when the requests reached `server`: no two may arrive closer
/// than `min_interval`, less a fifth of it for scheduling jitter. Build the
/// context with the same `min_interval` (see [`crate::context`]).
///
/// A run that sent fewer than `min_requests` requests proves nothing, so it
/// fails: arrange for the plugin to make several requests, such as one per
/// model.
///
/// # Panics
///
/// If too few requests were made, or two arrived too close together.
pub fn assert_paced(server: &MockServer, min_interval: Duration, min_requests: usize) {
    let requests = server.requests();
    assert!(
        requests.len() >= min_requests,
        "cannot check pacing: the plugin made {} request(s), need at least {min_requests}",
        requests.len()
    );

    let tolerance = min_interval / 5;
    for pair in requests.windows(2) {
        let gap = pair[1].at.duration_since(pair[0].at);
        assert!(
            gap + tolerance >= min_interval,
            "requests to {} and {} arrived {gap:?} apart, under the {min_interval:?} rate limit; \
             call ctx.throttle() before every request",
            pair[0].target,
            pair[1].target
        );
    }
}

/// Runs `discover()` against a server that includes records the plugin
/// can't parse, and checks that it skips them and still returns the
/// `expected` valid refs, instead of failing the whole dig.
///
/// # Panics
///
/// If `discover()` fails, or returns a different number of refs.
pub async fn discover_skips_malformed_records(
    plugin: &dyn VendorPlugin,
    ctx: &ScrapeContext,
    expected: usize,
) -> Vec<FirmwareRef> {
    let id = plugin.vendor_id();
    let refs = plugin.discover(ctx).await.unwrap_or_else(|e| {
        panic!("{id}: discover() failed on a malformed record instead of skipping it: {e}")
    });
    assert_eq!(
        refs.len(),
        expected,
        "{id}: discover() returned {} ref(s) with a malformed record present, expected {expected}",
        refs.len()
    );
    refs
}
