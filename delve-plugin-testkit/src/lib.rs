//! Conformance checks for vendor plugins, plus a local mock HTTP server to
//! run them against. A vendor crate adds this as a **dev-dependency** and
//! calls the checks from its own tests, so every plugin is held to the same
//! contract and none of them touches the network in CI. See the README's
//! "Testing a vendor plugin" section.
//!
//! The checks panic with a message naming the vendor and the rule that was
//! broken, like `assert!` does, so they are for tests only.
//!
//! A vendor's test builds three things and hands them to the checks:
//!
//! 1. a [`MockServer`] serving the vendor's wire format (fixtures),
//! 2. the plugin, pointed at that server,
//! 3. a [`ScrapeContext`] from
//!    [`context`], whose rate limit is long enough to measure.
//!
//! Then it runs [`discover_conforms`], [`metadata_conforms`] and
//! [`assert_paced`]; and [`discover_skips_malformed_records`] against a
//! server that includes a record the plugin cannot parse. `vendor-unifi`'s
//! tests are the reference.

mod conformance;
mod server;

pub use conformance::{
    assert_paced, discover_conforms, discover_skips_malformed_records, metadata_conforms,
};
pub use server::{MockServer, RecordedRequest, Request, Response};

use std::time::Duration;

use delve_core::context::{HttpClientConfig, RateLimit, ScrapeContext, Transport};

/// A [`ScrapeContext`] for tests: direct transport, no credentials, default
/// HTTP settings, and `min_interval` between requests. Chain
/// [`ScrapeContext::with_settings`] to add the vendor's settings.
///
/// Use a real interval (a few hundred milliseconds) with [`assert_paced`],
/// which measures when requests reach the server. Use `Duration::ZERO` to
/// switch pacing off in tests that don't check it.
///
/// # Panics
///
/// If the HTTP client cannot be built, which does not happen with the
/// direct transport.
pub fn context(min_interval: Duration) -> ScrapeContext {
    ScrapeContext::new(
        Transport::Direct,
        Default::default(),
        HttpClientConfig::default(),
        RateLimit { min_interval },
    )
    .expect("a direct-transport HTTP client always builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use delve_core::model::{FirmwareMetadata, FirmwareRef, VersionKey, VersionScheme};
    use delve_core::plugin::{ArtifactSink, PluginCapabilities, PluginError, VendorPlugin};

    const INTERVAL: Duration = Duration::from_millis(100);

    /// A plugin that makes `requests` requests to the server on `discover()`
    /// and can be told to break each rule the checks look for.
    struct Toy {
        base: url::Url,
        requests: usize,
        throttle: bool,
        ref_vendor: &'static str,
        version: VersionKey,
        hardware: Vec<String>,
        fail_discover: bool,
    }

    impl Toy {
        fn conforming(server: &MockServer) -> Self {
            Self {
                base: server.url(),
                requests: 3,
                throttle: true,
                ref_vendor: "toy",
                version: VersionKey {
                    raw: "1.2.3".into(),
                    scheme: VersionScheme::Semver,
                    ordinal: Some(vec![1, 2, 3]),
                },
                hardware: vec!["rev-a".into()],
                fail_discover: false,
            }
        }
    }

    #[async_trait]
    impl VendorPlugin for Toy {
        fn vendor_id(&self) -> &'static str {
            "toy"
        }

        fn capabilities(&self) -> PluginCapabilities {
            PluginCapabilities {
                tos_reviewed: true,
                supports_signature_verification: false,
            }
        }

        async fn discover(&self, ctx: &ScrapeContext) -> Result<Vec<FirmwareRef>, PluginError> {
            if self.fail_discover {
                return Err(PluginError::Parse("bad record".into()));
            }
            let mut refs = Vec::new();
            for i in 0..self.requests {
                let url = self.base.join(&format!("r{i}")).unwrap();
                if self.throttle {
                    ctx.throttle().await;
                }
                ctx.http_client()
                    .get(url.clone())
                    .send()
                    .await
                    .map_err(PluginError::Transport)?;
                refs.push(FirmwareRef {
                    vendor: self.ref_vendor.into(),
                    device_family: "widget".into(),
                    source_url: url,
                    discovered_at: chrono::Utc::now(),
                });
            }
            Ok(refs)
        }

        async fn metadata(
            &self,
            _ctx: &ScrapeContext,
            r: &FirmwareRef,
        ) -> Result<FirmwareMetadata, PluginError> {
            Ok(FirmwareMetadata {
                vendor: r.vendor.clone(),
                device_family: r.device_family.clone(),
                source_url: r.source_url.clone(),
                version: self.version.clone(),
                release_date: None,
                sha256: None,
                signature: None,
                hardware_targets: self.hardware.clone(),
                release_notes_url: None,
                display_name: None,
            })
        }

        async fn fetch(
            &self,
            _ctx: &ScrapeContext,
            _r: &FirmwareRef,
            _sink: &mut dyn ArtifactSink,
        ) -> Result<(), PluginError> {
            Err(PluginError::Unimplemented)
        }
    }

    fn ok_server() -> MockServer {
        MockServer::start(|_| Response::json("{}"))
    }

    #[tokio::test]
    async fn a_conforming_plugin_passes_every_check() {
        let server = ok_server();
        let plugin = Toy::conforming(&server);
        let ctx = context(INTERVAL);

        let refs = discover_conforms(&plugin, &ctx).await;
        metadata_conforms(&plugin, &ctx, &refs).await;
        assert_paced(&server, INTERVAL, 3);
        discover_skips_malformed_records(&plugin, &context(Duration::ZERO), 3).await;
    }

    #[tokio::test]
    #[should_panic(expected = "not vendor_id()")]
    async fn refs_for_another_vendor_fail_discover() {
        let server = ok_server();
        let plugin = Toy {
            ref_vendor: "other",
            ..Toy::conforming(&server)
        };
        discover_conforms(&plugin, &context(Duration::ZERO)).await;
    }

    #[tokio::test]
    #[should_panic(expected = "returned no refs")]
    async fn no_refs_fail_discover() {
        let server = ok_server();
        let plugin = Toy {
            requests: 0,
            ..Toy::conforming(&server)
        };
        discover_conforms(&plugin, &context(Duration::ZERO)).await;
    }

    #[tokio::test]
    #[should_panic(expected = "is Opaque but has ordinal")]
    async fn an_opaque_version_with_an_ordinal_fails_metadata() {
        let server = ok_server();
        let plugin = Toy {
            version: VersionKey {
                raw: "weird".into(),
                scheme: VersionScheme::Opaque,
                ordinal: Some(vec![1]),
            },
            ..Toy::conforming(&server)
        };
        let ctx = context(Duration::ZERO);
        let refs = discover_conforms(&plugin, &ctx).await;
        metadata_conforms(&plugin, &ctx, &refs).await;
    }

    #[tokio::test]
    #[should_panic(expected = "blank hardware target")]
    async fn a_blank_hardware_target_fails_metadata() {
        let server = ok_server();
        let plugin = Toy {
            hardware: vec![" ".into()],
            ..Toy::conforming(&server)
        };
        let ctx = context(Duration::ZERO);
        let refs = discover_conforms(&plugin, &ctx).await;
        metadata_conforms(&plugin, &ctx, &refs).await;
    }

    #[tokio::test]
    #[should_panic(expected = "call ctx.throttle() before every request")]
    async fn a_plugin_that_skips_throttle_fails_the_pacing_check() {
        let server = ok_server();
        let plugin = Toy {
            throttle: false,
            ..Toy::conforming(&server)
        };
        discover_conforms(&plugin, &context(INTERVAL)).await;
        assert_paced(&server, INTERVAL, 3);
    }

    #[tokio::test]
    #[should_panic(expected = "cannot check pacing")]
    async fn too_few_requests_cannot_prove_pacing() {
        let server = ok_server();
        let plugin = Toy {
            requests: 1,
            ..Toy::conforming(&server)
        };
        discover_conforms(&plugin, &context(INTERVAL)).await;
        assert_paced(&server, INTERVAL, 2);
    }

    #[tokio::test]
    #[should_panic(expected = "failed on a malformed record instead of skipping it")]
    async fn a_discover_that_fails_on_a_bad_record_fails_the_skip_check() {
        let server = ok_server();
        let plugin = Toy {
            fail_discover: true,
            ..Toy::conforming(&server)
        };
        discover_skips_malformed_records(&plugin, &context(Duration::ZERO), 3).await;
    }

    #[tokio::test]
    async fn the_server_answers_from_its_handler_and_records_each_request() {
        let server = MockServer::start(|req| {
            if req.url().path() == "/missing" {
                Response::status(404)
            } else {
                Response::json(r#"{"ok":true}"#)
            }
        });
        let client = reqwest::Client::new();
        let base = server.url();

        let ok = client
            .get(base.join("a?x=1").unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), 200);
        assert_eq!(ok.text().await.unwrap(), r#"{"ok":true}"#);
        let missing = client
            .get(base.join("missing").unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);

        let seen = server.requests();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].target, "/a?x=1");
        assert_eq!(seen[0].query(), "x=1");
        assert_eq!(seen[1].query(), "");
        assert!(seen[1].at >= seen[0].at);
    }
}
