//! Cisco vendor plugin.
//!
//! **Implementation status: `discover`/`metadata` are implemented against
//! Cisco's Software Suggestion API, structurally real but unverified
//! against a live endpoint (this environment has no network access — see
//! `api.rs`'s and `auth.rs`'s module doc comments for exactly what's
//! confirmed-safe vs. best-effort-and-needs-checking). `fetch` remains
//! unimplemented — see its own doc comment below for why that's a
//! separate, even-less-certain piece of work.**
//!
//! Authentication is OAuth2 `client_credentials` (see `auth.rs`), which is
//! how Cisco's real Support APIs work — this replaces an earlier draft of
//! this plugin that assumed a `username`/`password` credential pair, which
//! doesn't match how Cisco's API access actually works. Configure
//! `[vendors.credentials.cisco]` with `client_id`/`client_secret` from a
//! Cisco API Console application registration (see the README's
//! "Credentials" section).
//!
//! `capabilities().tos_reviewed` stays `false` regardless of any of the
//! above — using Cisco's own published API is plausibly easier to stay
//! compliant with than scraping their web portal would be, but "plausibly
//! easier" isn't the same as "actually reviewed," and that flag exists
//! specifically to record the latter. See the README's "Compliance"
//! section, and don't flip it without doing that review for real.

mod api;
mod auth;
mod version;

use async_trait::async_trait;
use delve_core::prelude::*;

use api::{CiscoProduct, KNOWN_PRODUCTS};
use auth::TokenCache;

pub struct CiscoPlugin {
    tokens: TokenCache,
}

impl CiscoPlugin {
    pub fn new() -> Self {
        Self {
            tokens: TokenCache::new(),
        }
    }

    fn product_for_device_family(device_family: &str) -> Option<&'static CiscoProduct> {
        KNOWN_PRODUCTS
            .iter()
            .find(|p| p.device_family == device_family)
    }
}

impl Default for CiscoPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VendorPlugin for CiscoPlugin {
    fn vendor_id(&self) -> &'static str {
        "cisco"
    }

    fn capabilities(&self) -> PluginCapabilities {
        PluginCapabilities {
            tos_reviewed: false, // see this module's doc comment — do not flip without review
            supports_signature_verification: false,
        }
    }

    async fn discover(&self, ctx: &ScrapeContext) -> Result<Vec<FirmwareRef>, PluginError> {
        let client_id = ctx.require_credential("client_id")?;
        let client_secret = ctx.require_credential("client_secret")?;
        let token = self.tokens.get_token(ctx, client_id, client_secret).await?;

        let mut all_refs = Vec::new();
        for product in KNOWN_PRODUCTS {
            ctx.throttle().await;

            let url = api::suggestions_list_url(product.base_pid);
            let response = ctx
                .http_client()
                .get(url.as_str())
                .bearer_auth(&token)
                .send()
                .await
                .map_err(PluginError::Transport)?;

            if !response.status().is_success() {
                let status = response.status();
                return Err(PluginError::UnexpectedResponse(format!(
                    "Cisco suggestion list request for {} failed with status {status} \
                     (url: {url}) — see api.rs's doc comment if this is a 404: the endpoint \
                     path there is unverified",
                    product.base_pid
                )));
            }

            let body = response.text().await.map_err(PluginError::Transport)?;
            let refs = api::parse_suggestion_list(&body, self.vendor_id(), product.device_family)?;
            all_refs.extend(refs);
        }

        Ok(all_refs)
    }

    async fn metadata(
        &self,
        ctx: &ScrapeContext,
        r: &FirmwareRef,
    ) -> Result<FirmwareMetadata, PluginError> {
        let client_id = ctx.require_credential("client_id")?;
        let client_secret = ctx.require_credential("client_secret")?;
        let token = self.tokens.get_token(ctx, client_id, client_secret).await?;

        let product = Self::product_for_device_family(&r.device_family).ok_or_else(|| {
            PluginError::UnexpectedResponse(format!(
                "no known Cisco base PID for device family '{}' — was this FirmwareRef produced \
                 by a different plugin, or has KNOWN_PRODUCTS changed since discover() ran?",
                r.device_family
            ))
        })?;

        ctx.throttle().await;

        let response = ctx
            .http_client()
            .get(r.source_url.clone())
            .bearer_auth(&token)
            .send()
            .await
            .map_err(PluginError::Transport)?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(PluginError::UnexpectedResponse(format!(
                "Cisco suggestion detail request failed with status {status} (url: {}) — see \
                 api.rs's doc comment point 3: whether this per-ID detail endpoint exists at \
                 all is unverified",
                r.source_url
            )));
        }

        let body = response.text().await.map_err(PluginError::Transport)?;
        api::parse_suggestion_detail(&body, product.base_pid)
    }

    async fn fetch(
        &self,
        ctx: &ScrapeContext,
        _r: &FirmwareRef,
        _sink: &mut dyn ArtifactSink,
    ) -> Result<(), PluginError> {
        // Deliberately not implemented, rather than guessed at along with
        // discover/metadata: getting an actual downloadable binary URL out
        // of Cisco typically requires a separate "generate download
        // token/URL" API call beyond the Suggestion API (the suggestion
        // list/detail endpoints describe *what* releases exist, not a
        // direct download link) — and unlike discover/metadata, getting
        // that flow wrong doesn't just return bad metadata, it means
        // `unearth` either fails outright or (worse) downloads something
        // that silently isn't the right file. That's a bigger risk to
        // guess at blind than list/detail was. Implement once real API
        // access is available to confirm the actual download-URL flow.
        //
        // Still subject to the same rate limit as every other Cisco
        // request once implemented — call ctx.throttle().await before the
        // real request goes in here.
        ctx.throttle().await;
        Err(PluginError::Unimplemented)
    }
}

inventory::submit! {
    PluginDescriptor {
        id: "cisco",
        factory: || Box::new(CiscoPlugin::new()),
    }
}
