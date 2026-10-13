//! Cisco vendor plugin.
//!
//! **Implementation status: nothing is fetched.** `discover`, `metadata` and
//! `fetch` all return [`PluginError::Unimplemented`].
//!
//! An earlier draft read Cisco's official Software Suggestion API (OAuth2
//! `client_credentials` against `apix.cisco.com`). It was never run against
//! the live API and has been removed, so this plugin takes no credentials.
//!
//! What exists instead is [`catalog`]: parsers and a subtree walk for the
//! JSON the software.cisco.com download pages read, checked against captured
//! responses. It has no source of those responses yet: the endpoints answer a
//! client with no session with 401, and whether delve may use them is open
//! (the issue "fetch the software.cisco.com catalog"). Until that is settled,
//! wiring `catalog` into `discover` and `metadata` would only produce a plugin
//! that fails on its first request.
//!
//! `capabilities().tos_reviewed` stays `false`: nobody has reviewed Cisco's
//! terms for this use. See the README's "Compliance" section, and don't flip
//! it without doing that review for real.

pub mod catalog;
mod version;

use async_trait::async_trait;
use delve_core::prelude::*;

pub struct CiscoPlugin;

impl CiscoPlugin {
    pub fn new() -> Self {
        Self
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

    async fn discover(&self, _ctx: &ScrapeContext) -> Result<Vec<FirmwareRef>, PluginError> {
        Err(PluginError::Unimplemented)
    }

    async fn metadata(
        &self,
        _ctx: &ScrapeContext,
        _r: &FirmwareRef,
    ) -> Result<FirmwareMetadata, PluginError> {
        Err(PluginError::Unimplemented)
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

inventory::submit! {
    PluginDescriptor {
        id: "cisco",
        factory: || Box::new(CiscoPlugin::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plugin_is_registered_under_its_id_and_unreviewed() {
        let plugin = CiscoPlugin::new();
        assert_eq!(plugin.vendor_id(), "cisco");
        assert!(!plugin.capabilities().tos_reviewed);
    }
}
