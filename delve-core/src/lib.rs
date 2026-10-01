//! Core traits, data model, and engine for delve.
//!
//! This crate has no knowledge of any specific vendor, storage backend, or
//! notification channel — it defines the contracts (`VendorPlugin`,
//! `MetadataStore`, `Subscriber`) that `delve-vendors/*`,
//! `delve-store-sqlite`, and `delve-subscribers/*` implement, plus the
//! engine loop (`dig_vendor`) that ties them together. See the project
//! README for the reasoning behind each module — in particular "Plugin
//! architecture", "Data model and identity keys", "Baseline vs incremental
//! digs", and "Storage".

pub mod context;
pub mod engine;
pub mod events;
pub mod model;
pub mod plugin;
pub mod store;

// Re-exports for the common path: `use delve_core::prelude::*;`
pub mod prelude {
    pub use crate::context::{CircuitIsolation, ScrapeContext, TorConfig, TorMode, Transport};
    pub use crate::engine::{dig_vendor, EngineError};
    pub use crate::events::{EventBus, FieldDiff, FirmwareEvent, Subscriber, SubscriberError};
    pub use crate::model::{
        hardware_key, FirmwareMetadata, FirmwareRef, SignatureInfo, VersionDirection, VersionKey,
        VersionScheme,
    };
    pub use crate::plugin::{
        ArtifactSink, PluginCapabilities, PluginDescriptor, PluginError, PluginRegistry,
        VendorPlugin,
    };
    pub use crate::store::{
        FirmwareKey, FirmwareRevision, FirmwareSelector, MetadataStore, RunKind, RunOutcome,
        StoreError,
    };
}
