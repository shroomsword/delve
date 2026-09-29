//! UniFi vendor plugin.
//!
//! Tracks firmware for **UniFi network devices only** — the API's
//! `unifi-firmware` product: access points, switches, gateways, and older
//! Cloud Keys. UniFi OS consoles (Dream Machines, UNVR, UNAS), Protect
//! cameras, Access devices and the rest of Ubiquiti's catalogue are listed
//! under other product names in the same API and are not tracked yet. See
//! the README's vendor-unifi section for the scope and the items noted
//! there to revisit.
//!
//! Data comes from Ubiquiti's firmware update API
//! (`https://fw-update.ui.com/api/firmware`), which needs no credentials.
//! See `api.rs`'s module doc comment for what was verified against live
//! responses.
//!
//! **One request per dig.** `discover()` fetches every release record in a
//! single list request and caches the full records in memory, keyed by
//! each record's own API URL (the `FirmwareRef::source_url` it hands
//! back). `metadata()` then answers from that cache without a request of
//! its own. Only a cache miss — `metadata()` called without a preceding
//! `discover()` in this process — falls back to fetching the record's own
//! URL. `fetch()` usually runs in a fresh `unearth` process with an empty
//! cache, so it costs two requests: the record, then the file.
//!
//! `capabilities().tos_reviewed` is `true`: the project owner reviewed
//! Ubiquiti's terms for this use in September 2026 — see the README's
//! "Vendor: UniFi" section for the findings, including what stays off
//! limits (redistributing downloaded images).

mod api;
mod version;

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use delve_core::prelude::*;

use api::FirmwareRecord;

pub struct UnifiPlugin {
    /// Records from the most recent `discover()`, keyed by
    /// `FirmwareRef::source_url`. Replaced wholesale on every discover so
    /// records Ubiquiti has removed don't linger.
    cache: Mutex<HashMap<String, FirmwareRecord>>,
}

impl UnifiPlugin {
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn cached(&self, source_url: &url::Url) -> Option<FirmwareRecord> {
        self.cache
            .lock()
            .expect("cache lock poisoned")
            .get(source_url.as_str())
            .cloned()
    }

    /// The record behind `r`, from the cache if `discover()` already saw it,
    /// otherwise from the record's own API URL.
    async fn record_for(
        &self,
        ctx: &ScrapeContext,
        r: &FirmwareRef,
    ) -> Result<FirmwareRecord, PluginError> {
        if let Some(record) = self.cached(&r.source_url) {
            return Ok(record);
        }
        let body = get_text(ctx, r.source_url.as_str()).await?;
        api::parse_detail(&body)
    }
}

impl Default for UnifiPlugin {
    fn default() -> Self {
        Self::new()
    }
}

async fn get_text(ctx: &ScrapeContext, url: &str) -> Result<String, PluginError> {
    ctx.throttle().await;
    let response = ctx
        .http_client()
        .get(url)
        .send()
        .await
        .map_err(PluginError::Transport)?;
    let status = response.status();
    if !status.is_success() {
        return Err(PluginError::UnexpectedResponse(format!(
            "UniFi firmware API returned {status} for {url}"
        )));
    }
    response.text().await.map_err(PluginError::Transport)
}

#[async_trait]
impl VendorPlugin for UnifiPlugin {
    fn vendor_id(&self) -> &'static str {
        "unifi"
    }

    fn capabilities(&self) -> PluginCapabilities {
        PluginCapabilities {
            tos_reviewed: true, // reviewed September 2026 — see this module's doc comment
            supports_signature_verification: false,
        }
    }

    async fn discover(&self, ctx: &ScrapeContext) -> Result<Vec<FirmwareRef>, PluginError> {
        let url = api::list_url();
        let body = get_text(ctx, url.as_str()).await?;
        let parsed = api::parse_list(&body)?;

        if parsed.total >= api::LIST_LIMIT {
            return Err(PluginError::UnexpectedResponse(format!(
                "UniFi firmware list returned {} records, the request's limit — the response may be \
                 truncated, and the API ignores offset so it can't be paged; raise api::LIST_LIMIT",
                parsed.total
            )));
        }
        for reason in &parsed.skipped {
            tracing::warn!(
                vendor = "unifi",
                "skipping unparseable firmware record {reason}"
            );
        }

        let records = api::select_records(parsed.records);
        tracing::info!(
            vendor = "unifi",
            received = parsed.total,
            tracked = records.len(),
            "UniFi firmware list fetched"
        );

        let refs = records
            .iter()
            .map(|record| api::record_to_ref(record, self.vendor_id()))
            .collect();

        let mut cache = self.cache.lock().expect("cache lock poisoned");
        *cache = records
            .into_iter()
            .map(|record| (record.links.self_link.href.to_string(), record))
            .collect();

        Ok(refs)
    }

    async fn metadata(
        &self,
        ctx: &ScrapeContext,
        r: &FirmwareRef,
    ) -> Result<FirmwareMetadata, PluginError> {
        let record = self.record_for(ctx, r).await?;
        Ok(api::record_to_metadata(&record, self.vendor_id()))
    }

    async fn fetch(
        &self,
        ctx: &ScrapeContext,
        r: &FirmwareRef,
        sink: &mut dyn ArtifactSink,
    ) -> Result<(), PluginError> {
        let record = self.record_for(ctx, r).await?;
        let data_url = record.links.data.as_ref().ok_or_else(|| {
            PluginError::UnexpectedResponse(format!(
                "UniFi firmware record {} has no download link",
                record.id
            ))
        })?;

        ctx.throttle().await;
        let mut response = ctx
            .http_client()
            .get(data_url.href.clone())
            .send()
            .await
            .map_err(PluginError::Transport)?;
        let status = response.status();
        if !status.is_success() {
            return Err(PluginError::UnexpectedResponse(format!(
                "UniFi firmware download returned {status} for {}",
                data_url.href
            )));
        }

        let mut written: u64 = 0;
        while let Some(chunk) = response.chunk().await.map_err(PluginError::Transport)? {
            sink.write_chunk(&chunk)?;
            written += chunk.len() as u64;
        }

        // `unearth` checks the SHA-256 afterwards; checking the size here
        // gives a clearer error for the common failure of a cut-off
        // download.
        if let Some(expected) = record.file_size {
            if written != expected {
                return Err(PluginError::UnexpectedResponse(format!(
                    "UniFi firmware download was {written} bytes, expected {expected}"
                )));
            }
        }
        Ok(())
    }
}

inventory::submit! {
    PluginDescriptor {
        id: "unifi",
        factory: || Box::new(UnifiPlugin::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use delve_core::context::{HttpClientConfig, RateLimit};
    use sha2::{Digest, Sha256};

    fn ctx() -> ScrapeContext {
        ScrapeContext::new(
            Transport::Direct,
            Default::default(),
            HttpClientConfig::default(),
            RateLimit::default(),
        )
        .unwrap()
    }

    fn fixture_records() -> Vec<FirmwareRecord> {
        api::parse_list(include_str!("fixtures/firmware_list.json"))
            .unwrap()
            .records
    }

    /// Collects downloaded bytes in memory and hashes them.
    #[derive(Default)]
    struct MemorySink {
        bytes: Vec<u8>,
    }

    impl ArtifactSink for MemorySink {
        fn write_chunk(&mut self, chunk: &[u8]) -> std::io::Result<()> {
            self.bytes.extend_from_slice(chunk);
            Ok(())
        }
        fn finish(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn metadata_is_served_from_the_discover_cache_without_a_request() {
        let plugin = UnifiPlugin::new();
        let record = fixture_records()
            .into_iter()
            .find(|r| r.platform == "U7PG2")
            .unwrap();
        let fref = api::record_to_ref(&record, "unifi");
        // Point the ref's cache key at an address nothing listens on: if
        // metadata() made a request instead of using the cache, this test
        // would fail with a transport error.
        let unreachable: url::Url = "http://127.0.0.1:9/api/firmware/cached".parse().unwrap();
        plugin
            .cache
            .lock()
            .unwrap()
            .insert(unreachable.to_string(), record.clone());
        let fref = FirmwareRef {
            source_url: unreachable,
            ..fref
        };

        let meta = plugin.metadata(&ctx(), &fref).await.unwrap();
        assert_eq!(meta.version.raw, record.version);
        assert_eq!(meta.hardware_targets, vec!["U7PG2".to_string()]);
    }

    #[tokio::test]
    async fn a_cache_miss_falls_back_to_a_request() {
        let plugin = UnifiPlugin::new();
        let fref = FirmwareRef {
            vendor: "unifi".to_string(),
            device_family: "U7PG2".to_string(),
            source_url: "http://127.0.0.1:9/api/firmware/missing".parse().unwrap(),
            discovered_at: chrono::Utc::now(),
        };
        let result = plugin.metadata(&ctx(), &fref).await;
        assert!(matches!(result, Err(PluginError::Transport(_))));
    }

    // ---- Live tests against Ubiquiti's real API. Not run by default (or in
    // CI): they depend on the network and on Ubiquiti's servers. Run with
    //   cargo test -p vendor-unifi -- --ignored

    #[tokio::test]
    #[ignore = "hits the live Ubiquiti API"]
    async fn live_discover_and_metadata() {
        let plugin = UnifiPlugin::new();
        let ctx = ctx();
        let refs = plugin.discover(&ctx).await.unwrap();
        assert!(
            refs.len() > 1000,
            "expected thousands of records, got {}",
            refs.len()
        );

        let mut with_sha = 0;
        for r in &refs {
            let meta = plugin.metadata(&ctx, r).await.unwrap();
            assert_eq!(meta.device_family, r.device_family);
            assert!(meta.release_date.is_some());
            if meta.sha256.is_some() {
                with_sha += 1;
            }
        }
        assert_eq!(
            with_sha,
            refs.len(),
            "every tracked record should have a SHA-256"
        );
    }

    #[tokio::test]
    #[ignore = "hits the live Ubiquiti API and downloads a ~500 KB file"]
    async fn live_fetch_matches_the_published_sha256() {
        // USW-Flex-Mini v1.6.3, the smallest release image; a fresh plugin
        // so this also covers fetch() with an empty cache, as in `unearth`.
        let plugin = UnifiPlugin::new();
        let fref = FirmwareRef {
            vendor: "unifi".to_string(),
            device_family: "USMINI".to_string(),
            source_url:
                "https://fw-update.ui.com/api/firmware/a4fb8871-1951-43bb-9db3-5d8a62e26e3d"
                    .parse()
                    .unwrap(),
            discovered_at: chrono::Utc::now(),
        };
        let ctx = ctx();
        let meta = plugin.metadata(&ctx, &fref).await.unwrap();
        let mut sink = MemorySink::default();
        plugin.fetch(&ctx, &fref, &mut sink).await.unwrap();

        let digest: [u8; 32] = Sha256::digest(&sink.bytes).into();
        assert_eq!(Some(digest), meta.sha256);
    }
}
