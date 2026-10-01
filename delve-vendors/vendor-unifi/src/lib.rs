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
//! **One request per dig**, by default: `discover()` fetches every release
//! record in a single list request. With `models` set under
//! `[vendors.settings.unifi]`, it instead makes one list request per model
//! and tracks only those models. Either way it caches the full records in
//! memory, keyed by each record's own API URL (the `FirmwareRef::source_url`
//! it hands back). `metadata()` then answers from that cache without a request of
//! its own, and removes each record as it does, so the cache shrinks as the
//! dig stores entries instead of holding every record until the process
//! exits. Only a cache miss — `metadata()` called without a preceding
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
    /// Base URL of the firmware list API — [`api::API_BASE`] except in
    /// tests, which point it at a local server.
    api_base: url::Url,
    /// Records from the most recent `discover()`, keyed by
    /// `FirmwareRef::source_url`. Replaced wholesale on every discover so
    /// records Ubiquiti has removed don't linger. `metadata()` takes each
    /// record out as it uses it (see `record_for`).
    cache: Mutex<HashMap<String, FirmwareRecord>>,
}

impl UnifiPlugin {
    pub fn new() -> Self {
        Self {
            api_base: api::api_base(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Fetches and parses one list request — every tracked model, or only
    /// `model` — and fails if the response may have been truncated.
    async fn fetch_list(
        &self,
        ctx: &ScrapeContext,
        model: Option<&str>,
    ) -> Result<api::ParsedList, PluginError> {
        let url = api::list_url(&self.api_base, model);
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
        Ok(parsed)
    }

    /// Every release record for the models in `models`, one request each.
    /// Re-checks each record's model in case the server ignored the filter,
    /// and fails on a model with no records at all — almost always a
    /// mistyped model code, which would otherwise silently track nothing.
    async fn fetch_models(
        &self,
        ctx: &ScrapeContext,
        models: &[String],
    ) -> Result<(Vec<FirmwareRecord>, usize), PluginError> {
        if models.is_empty() {
            return Err(PluginError::Rejected(
                "[vendors.settings.unifi] models is empty; remove it to track every model".into(),
            ));
        }

        let mut records = Vec::new();
        let mut total = 0;
        let mut seen = std::collections::HashSet::new();
        for model in models {
            if !seen.insert(model.as_str()) {
                continue;
            }
            let parsed = self.fetch_list(ctx, Some(model)).await?;
            total += parsed.total;
            let before = records.len();
            records.extend(parsed.records.into_iter().filter(|r| &r.platform == model));
            if records.len() == before {
                return Err(PluginError::Rejected(format!(
                    "no UniFi release firmware found for model '{model}' — check the model code \
                     in [vendors.settings.unifi] models (the API's platform, e.g. U7PG2)"
                )));
            }
        }
        Ok((records, total))
    }

    /// The cached record for `source_url`. With `consume`, the record is
    /// removed from the cache, so a later lookup for it misses.
    fn cached(&self, source_url: &url::Url, consume: bool) -> Option<FirmwareRecord> {
        let mut cache = self.cache.lock().expect("cache lock poisoned");
        if consume {
            cache.remove(source_url.as_str())
        } else {
            cache.get(source_url.as_str()).cloned()
        }
    }

    /// The record behind `r`, from the cache if `discover()` already saw it,
    /// otherwise from the record's own API URL.
    ///
    /// `consume` removes the record from the cache. `metadata()` passes
    /// `true`: the engine calls it once per ref, so the record is no longer
    /// needed afterwards. `fetch()` passes `false` and leaves the cache alone.
    async fn record_for(
        &self,
        ctx: &ScrapeContext,
        r: &FirmwareRef,
        consume: bool,
    ) -> Result<FirmwareRecord, PluginError> {
        if let Some(record) = self.cached(&r.source_url, consume) {
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
        let (records, total) = match ctx.setting_list("models")? {
            Some(models) => self.fetch_models(ctx, models).await?,
            None => {
                let parsed = self.fetch_list(ctx, None).await?;
                (parsed.records, parsed.total)
            }
        };

        let records = api::select_records(records);
        tracing::info!(
            vendor = "unifi",
            received = total,
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
        let record = self.record_for(ctx, r, true).await?;
        Ok(api::record_to_metadata(&record, self.vendor_id()))
    }

    async fn fetch(
        &self,
        ctx: &ScrapeContext,
        r: &FirmwareRef,
        sink: &mut dyn ArtifactSink,
    ) -> Result<(), PluginError> {
        let record = self.record_for(ctx, r, false).await?;
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
    use delve_core::context::{HttpClientConfig, RateLimit, SettingValue, Settings};
    use delve_plugin_testkit::{
        assert_paced, discover_conforms, discover_skips_malformed_records, metadata_conforms,
        MockServer, Response,
    };
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

    /// A context with no rate limit and the given `models` setting, for
    /// tests against the local mock API.
    fn ctx_with_models(models: Option<SettingValue>) -> ScrapeContext {
        let settings: Settings = models
            .map(|m| Settings::from([("models".to_string(), m)]))
            .unwrap_or_default();
        ScrapeContext::new(
            Transport::Direct,
            Default::default(),
            HttpClientConfig::default(),
            RateLimit {
                min_interval: std::time::Duration::ZERO,
            },
        )
        .unwrap()
        .with_settings(settings)
    }

    fn models(codes: &[&str]) -> Option<SettingValue> {
        Some(SettingValue::List(
            codes.iter().map(|c| c.to_string()).collect(),
        ))
    }

    /// A local stand-in for the list API, serving the fixture records. It
    /// applies a `platform` filter the way the real API does, unless
    /// `honor_platform_filter` is false. Returns the plugin pointed at it and
    /// the server, which records every request it received.
    fn mock_api(honor_platform_filter: bool) -> (UnifiPlugin, MockServer) {
        mock_api_with(honor_platform_filter, vec![])
    }

    /// Like [`mock_api`], with `extra` records added to the served list —
    /// for records the plugin can't parse.
    fn mock_api_with(
        honor_platform_filter: bool,
        extra: Vec<serde_json::Value>,
    ) -> (UnifiPlugin, MockServer) {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("fixtures/firmware_list.json")).unwrap();

        let server = MockServer::start(move |request| {
            let url = request.url();
            let platform = url.query_pairs().find_map(|(k, v)| {
                (k == "filter")
                    .then(|| v.strip_prefix("eq~~platform~~").map(String::from))
                    .flatten()
            });

            let firmware: Vec<&serde_json::Value> = fixture["_embedded"]["firmware"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|r| match (&platform, honor_platform_filter) {
                    (Some(p), true) => r["platform"] == p.as_str(),
                    _ => true,
                })
                .chain(extra.iter())
                .collect();
            Response::json(serde_json::json!({ "_embedded": { "firmware": firmware } }).to_string())
        });

        let plugin = UnifiPlugin {
            api_base: server.url().join("api/firmware").unwrap(),
            ..UnifiPlugin::new()
        };
        (plugin, server)
    }

    /// Queries of every request the server has received.
    fn queries(server: &MockServer) -> Vec<String> {
        server.requests().iter().map(|r| r.query()).collect()
    }

    fn platforms(refs: &[FirmwareRef]) -> Vec<&str> {
        let mut p: Vec<&str> = refs.iter().map(|r| r.device_family.as_str()).collect();
        p.sort_unstable();
        p
    }

    #[tokio::test]
    async fn default_discover_makes_one_request_for_every_model() {
        let (plugin, server) = mock_api(true);
        let refs = plugin.discover(&ctx_with_models(None)).await.unwrap();

        let requests = queries(&server);
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].contains("platform"), "{}", requests[0]);
        // Every fixture model except the placeholder `stat` record.
        assert_eq!(
            platforms(&refs),
            ["U7PG2", "U7PG2", "UCKG2", "USMINI", "UX", "UXGPRO"]
        );
    }

    #[tokio::test]
    async fn models_setting_makes_one_request_per_model_and_tracks_only_those() {
        let (plugin, server) = mock_api(true);
        let ctx = ctx_with_models(models(&["U7PG2", "USMINI", "U7PG2"]));
        let refs = plugin.discover(&ctx).await.unwrap();

        let seen = queries(&server);
        assert_eq!(seen.len(), 2, "one request per distinct model: {seen:?}");
        assert!(seen[0].contains("filter=eq%7E%7Eplatform%7E%7EU7PG2"));
        assert!(seen[1].contains("filter=eq%7E%7Eplatform%7E%7EUSMINI"));
        assert_eq!(platforms(&refs), ["U7PG2", "U7PG2", "USMINI"]);

        // metadata() is still answered from the cache that discover filled.
        let meta = plugin.metadata(&ctx, &refs[0]).await.unwrap();
        assert_eq!(meta.device_family, refs[0].device_family);
        assert_eq!(server.requests().len(), 2);
    }

    #[tokio::test]
    async fn a_server_ignoring_the_model_filter_cant_add_other_models() {
        let (plugin, _server) = mock_api(false);
        let refs = plugin
            .discover(&ctx_with_models(models(&["UX"])))
            .await
            .unwrap();
        assert_eq!(platforms(&refs), ["UX"]);
    }

    #[tokio::test]
    async fn a_model_with_no_firmware_fails_the_dig() {
        let (plugin, _server) = mock_api(true);
        let err = plugin
            .discover(&ctx_with_models(models(&["U7PG2", "U7PG3"])))
            .await
            .unwrap_err();
        assert!(matches!(err, PluginError::Rejected(_)), "{err}");
        assert!(err.to_string().contains("model 'U7PG3'"), "{err}");
    }

    #[tokio::test]
    async fn an_empty_or_non_list_models_setting_is_rejected() {
        let (plugin, server) = mock_api(true);

        let err = plugin
            .discover(&ctx_with_models(models(&[])))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("models is empty"), "{err}");

        let err = plugin
            .discover(&ctx_with_models(Some(SettingValue::Text("U7PG2".into()))))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("must be a list"), "{err}");

        assert!(server.requests().is_empty());
    }

    // ---- Conformance: the checks every vendor plugin must pass, from
    // delve-plugin-testkit. This is the reference for other vendor crates.

    const PACING: std::time::Duration = std::time::Duration::from_millis(200);

    /// A context with a measurable rate limit that tracks three models, so a
    /// dig makes three requests.
    fn paced_ctx() -> ScrapeContext {
        delve_plugin_testkit::context(PACING).with_settings(Settings::from([(
            "models".to_string(),
            models(&["U7PG2", "USMINI", "UX"]).unwrap(),
        )]))
    }

    #[tokio::test]
    async fn conforms_to_the_vendor_plugin_contract() {
        let (plugin, server) = mock_api(true);
        let ctx = paced_ctx();

        let refs = discover_conforms(&plugin, &ctx).await;
        metadata_conforms(&plugin, &ctx, &refs).await;
        assert_paced(&server, PACING, 3);
    }

    #[tokio::test]
    async fn conforms_when_the_default_list_includes_unparseable_records() {
        // Two broken records among the fixture's six trackable ones: one
        // missing most fields, one that isn't an object at all.
        let broken = vec![serde_json::json!({ "id": "broken" }), serde_json::json!(7)];
        let (plugin, _server) = mock_api_with(true, broken);
        let ctx = delve_plugin_testkit::context(std::time::Duration::ZERO);

        let refs = discover_skips_malformed_records(&plugin, &ctx, 6).await;
        metadata_conforms(&plugin, &ctx, &refs).await;
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
    async fn metadata_removes_the_record_from_the_cache_as_it_uses_it() {
        let plugin = UnifiPlugin::new();
        let records: Vec<_> = fixture_records().into_iter().take(2).collect();
        assert_eq!(records.len(), 2, "the fixture should have two records");
        let key = |i: usize| -> url::Url {
            format!("http://127.0.0.1:9/api/firmware/cached-{i}")
                .parse()
                .unwrap()
        };
        for (i, record) in records.iter().enumerate() {
            plugin
                .cache
                .lock()
                .unwrap()
                .insert(key(i).to_string(), record.clone());
        }
        let fref = |i: usize| FirmwareRef {
            source_url: key(i),
            ..api::record_to_ref(&records[i], "unifi")
        };

        plugin.metadata(&ctx(), &fref(0)).await.unwrap();
        assert_eq!(plugin.cache.lock().unwrap().len(), 1);

        // The used record is gone, so asking again falls back to a request,
        // which fails against the unreachable address; the other record is
        // still served from the cache.
        let again = plugin.metadata(&ctx(), &fref(0)).await;
        assert!(matches!(again, Err(PluginError::Transport(_))));
        plugin.metadata(&ctx(), &fref(1)).await.unwrap();
        assert!(plugin.cache.lock().unwrap().is_empty());
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
    #[ignore = "hits the live Ubiquiti API"]
    async fn live_per_model_discover_tracks_only_those_models() {
        let plugin = UnifiPlugin::new();
        let ctx = ctx().with_settings(Settings::from([(
            "models".to_string(),
            SettingValue::List(vec!["U7PG2".into(), "USMINI".into()]),
        )]));
        let refs = plugin.discover(&ctx).await.unwrap();

        let mut families: Vec<&str> = refs.iter().map(|r| r.device_family.as_str()).collect();
        families.sort_unstable();
        families.dedup();
        assert_eq!(families, ["U7PG2", "USMINI"]);
        for r in &refs {
            let meta = plugin.metadata(&ctx, r).await.unwrap();
            assert_eq!(meta.hardware_targets, vec![r.device_family.clone()]);
        }
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
