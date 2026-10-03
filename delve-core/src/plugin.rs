//! The `VendorPlugin` trait and static plugin registry. See the README's
//! "Plugin architecture" section for the full rationale.
//!
//! Kept intentionally narrow: vendor-specific quirks (auth flows,
//! pagination, container formats) stay inside each plugin crate, never leak
//! into this trait's signature. That's also what keeps the door open to a
//! dynamic (WASM or `abi_stable`) plugin tier later without a redesign.

use async_trait::async_trait;
use thiserror::Error;

use crate::context::ScrapeContext;
use crate::model::{FirmwareMetadata, FirmwareRef};

#[derive(Debug, Error)]
pub enum PluginError {
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("failed to parse vendor response: {0}")]
    Parse(String),
    #[error("vendor site returned unexpected data: {0}")]
    UnexpectedResponse(String),
    #[error("vendor rejected the request (rate limited or blocked): {0}")]
    Rejected(String),
    #[error("failed to write downloaded firmware: {0}")]
    Sink(#[from] std::io::Error),
    #[error("not implemented yet")]
    Unimplemented,
}

/// Anything a `dyn ArtifactSink` needs to do with the bytes of a downloaded
/// firmware image. `unearth` hands plugins a `FileSink` writing to the
/// user-supplied `--out` path; nothing else in the framework touches binary
/// bytes — see the README's "Storage" section ("Binaries are never stored
/// automatically") for why there's deliberately no `ArtifactStore`.
pub trait ArtifactSink: Send {
    /// Offers the vendor's own name for the file being downloaded, such as
    /// the last segment of its download URL. A plugin calls this once, before
    /// the first `write_chunk`, and only when it has a name it knows is the
    /// file's own (a URL like `.../download?id=7` has none). A sink may use it
    /// to name the file, and may ignore it, or reject it if it isn't a plain
    /// file name. The default ignores it.
    fn suggest_file_name(&mut self, _name: &str) {}

    fn write_chunk(&mut self, chunk: &[u8]) -> std::io::Result<()>;
    fn finish(&mut self) -> std::io::Result<()>;
}

/// Static capability/compliance flags a plugin declares about itself.
/// `tos_reviewed` exists so the per-vendor ToS/robots.txt review (see the
/// README's "Compliance" section) has somewhere concrete to live — the
/// framework can't determine this itself, only a human reviewing the
/// vendor's terms can, but it can refuse to enable a plugin that hasn't
/// been marked reviewed.
#[derive(Debug, Clone)]
pub struct PluginCapabilities {
    pub tos_reviewed: bool,
    pub supports_signature_verification: bool,
}

#[async_trait]
pub trait VendorPlugin: Send + Sync {
    fn vendor_id(&self) -> &'static str;
    fn capabilities(&self) -> PluginCapabilities;

    /// Enumerate candidate firmware artifacts from this vendor's sources
    /// (portal API, FTP tree, git releases, whatever). Called every `dig`,
    /// baseline or incremental alike.
    async fn discover(&self, ctx: &ScrapeContext) -> Result<Vec<FirmwareRef>, PluginError>;

    /// Pull metadata (version, hash, release notes, hardware targets)
    /// without downloading the full image where the source allows it.
    async fn metadata(
        &self,
        ctx: &ScrapeContext,
        r: &FirmwareRef,
    ) -> Result<FirmwareMetadata, PluginError>;

    /// Download and optionally verify the artifact. Only ever called from
    /// the manual `unearth` CLI command — never invoked during a scheduled
    /// `dig` (see the README's "Baseline vs incremental digs" and "CLI
    /// commands" sections).
    async fn fetch(
        &self,
        ctx: &ScrapeContext,
        r: &FirmwareRef,
        sink: &mut dyn ArtifactSink,
    ) -> Result<(), PluginError>;
}

/// One entry in the static plugin registry. Vendor crates self-register at
/// link time via `inventory::submit!` — see vendor-cisco for an example.
/// This is the "static plugins" tier described in the README's "Plugin
/// architecture" section; a dynamic tier is not implemented here but this
/// type deliberately doesn't preclude adding one later.
pub struct PluginDescriptor {
    pub id: &'static str,
    pub factory: fn() -> Box<dyn VendorPlugin>,
}

inventory::collect!(PluginDescriptor);

/// Built from every registered `PluginDescriptor` at startup. The CLI's
/// `--vendor` filters and `catalog`/`unearth` vendor resolution all go
/// through this.
pub struct PluginRegistry {
    plugins: std::collections::HashMap<&'static str, Box<dyn VendorPlugin>>,
}

impl PluginRegistry {
    /// Instantiate every plugin that self-registered via `inventory`. This
    /// picks up exactly the vendors compiled in via Cargo features — there's
    /// no separate enable/disable step beyond the feature flags that pulled
    /// the vendor crate in as a dependency in the first place.
    pub fn discover() -> Self {
        let mut plugins = std::collections::HashMap::new();
        for descriptor in inventory::iter::<PluginDescriptor> {
            plugins.insert(descriptor.id, (descriptor.factory)());
        }
        Self { plugins }
    }

    /// Build a registry from explicit plugin instances instead of the
    /// `inventory` link-time registry — lets tests drive the CLI's
    /// registry-based code paths with mock plugins.
    pub fn from_plugins(plugins: Vec<Box<dyn VendorPlugin>>) -> Self {
        Self {
            plugins: plugins.into_iter().map(|p| (p.vendor_id(), p)).collect(),
        }
    }

    pub fn get(&self, vendor_id: &str) -> Option<&dyn VendorPlugin> {
        self.plugins.get(vendor_id).map(|b| b.as_ref())
    }

    pub fn vendor_ids(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.plugins.keys().copied()
    }
}
