//! Shared fixtures for the command-handler tests: a mock vendor plugin
//! whose releases can change between digs, a subscriber that records every
//! event, and an in-memory `SqliteStore`. The store and engine are the real
//! ones, so these tests exercise each command end to end without touching
//! the network.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use delve_core::prelude::*;
use delve_store_sqlite::SqliteStore;

use crate::cli::SelectorArgs;
use crate::config::Config;

pub async fn memory_store() -> SqliteStore {
    SqliteStore::open_url("sqlite::memory:")
        .await
        .expect("in-memory store")
}

pub fn config(toml: &str) -> Config {
    toml::from_str(toml).expect("test config parses")
}

/// One release a `MockPlugin` reports. `sha` fills every byte of the
/// SHA-256, so two releases differ in hash exactly when their `sha` does.
#[derive(Clone)]
pub struct Release {
    pub family: &'static str,
    pub version: &'static str,
    pub ordinal: Option<Vec<u64>>,
    pub hardware: &'static [&'static str],
    pub sha: u8,
    /// The product name the plugin reports, if any.
    pub display_name: Option<&'static str>,
}

pub fn release(family: &'static str, version: &'static str, ordinal: &[u64], sha: u8) -> Release {
    Release {
        family,
        version,
        ordinal: Some(ordinal.to_vec()),
        hardware: &["rev-a"],
        sha,
        display_name: None,
    }
}

impl Release {
    fn source_url(&self, vendor: &str) -> url::Url {
        url::Url::parse(&format!(
            "https://{vendor}.example.test/{}/{}/{}",
            self.family,
            self.hardware.join("+"),
            self.version
        ))
        .unwrap()
    }

    fn metadata(&self, vendor: &str) -> FirmwareMetadata {
        FirmwareMetadata {
            vendor: vendor.to_string(),
            device_family: self.family.to_string(),
            source_url: self.source_url(vendor),
            version: match &self.ordinal {
                Some(ordinal) => VersionKey {
                    raw: self.version.to_string(),
                    scheme: VersionScheme::VendorNumeric,
                    ordinal: Some(ordinal.clone()),
                },
                None => VersionKey::opaque(self.version.to_string()),
            },
            release_date: chrono::NaiveDate::from_ymd_opt(2026, 9, 1),
            sha256: Some([self.sha; 32]),
            signature: None,
            hardware_targets: self.hardware.iter().map(|h| h.to_string()).collect(),
            release_notes_url: None,
            display_name: self.display_name.map(String::from),
        }
    }
}

/// A vendor plugin serving whatever `releases` currently holds. Tests keep
/// a clone of the `Arc` to change what the next dig sees.
pub struct MockPlugin {
    pub id: &'static str,
    pub tos_reviewed: bool,
    pub releases: Arc<Mutex<Vec<Release>>>,
    /// When set, `discover` fails with this message.
    pub fail_discover: Option<&'static str>,
}

impl MockPlugin {
    pub fn new(id: &'static str, releases: Vec<Release>) -> Self {
        Self {
            id,
            tos_reviewed: true,
            releases: Arc::new(Mutex::new(releases)),
            fail_discover: None,
        }
    }
}

#[async_trait]
impl VendorPlugin for MockPlugin {
    fn vendor_id(&self) -> &'static str {
        self.id
    }

    fn capabilities(&self) -> PluginCapabilities {
        PluginCapabilities {
            tos_reviewed: self.tos_reviewed,
            supports_signature_verification: false,
        }
    }

    async fn discover(&self, _ctx: &ScrapeContext) -> Result<Vec<FirmwareRef>, PluginError> {
        if let Some(msg) = self.fail_discover {
            return Err(PluginError::UnexpectedResponse(msg.to_string()));
        }
        Ok(self
            .releases
            .lock()
            .unwrap()
            .iter()
            .map(|r| FirmwareRef {
                vendor: self.id.to_string(),
                device_family: r.family.to_string(),
                source_url: r.source_url(self.id),
                discovered_at: chrono::Utc::now(),
            })
            .collect())
    }

    async fn metadata(
        &self,
        _ctx: &ScrapeContext,
        r: &FirmwareRef,
    ) -> Result<FirmwareMetadata, PluginError> {
        self.releases
            .lock()
            .unwrap()
            .iter()
            .find(|rel| rel.source_url(self.id) == r.source_url)
            .map(|rel| rel.metadata(self.id))
            .ok_or_else(|| {
                PluginError::UnexpectedResponse(format!("no release at {}", r.source_url))
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

/// Records every published event, so tests can assert what a dig notified.
#[derive(Clone, Default)]
pub struct RecordingSubscriber {
    pub events: Arc<Mutex<Vec<FirmwareEvent>>>,
}

#[async_trait]
impl Subscriber for RecordingSubscriber {
    fn id(&self) -> &'static str {
        "recording"
    }

    async fn notify(&self, event: &FirmwareEvent) -> Result<(), SubscriberError> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }
}

impl RecordingSubscriber {
    pub fn bus(&self) -> EventBus {
        EventBus::new(vec![Box::new(self.clone())])
    }

    /// Takes the events recorded so far, leaving the list empty.
    pub fn take(&self) -> Vec<FirmwareEvent> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }
}

/// Digs `plugin` once with a default config and no subscribers — for tests
/// that only need data in the store.
pub async fn seed(store: &SqliteStore, plugin: MockPlugin) {
    let registry = PluginRegistry::from_plugins(vec![Box::new(plugin)]);
    super::dig::dig_vendors(
        &config(""),
        &registry,
        store,
        &EventBus::new(vec![]),
        None,
        false,
        false,
    )
    .await
    .expect("seed dig succeeds");
}

pub fn selector() -> SelectorArgs {
    SelectorArgs {
        id: None,
        vendor: None,
        device_family: None,
        hardware: vec![],
        version: None,
        latest: false,
    }
}
