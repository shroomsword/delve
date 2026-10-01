//! `FirmwareEvent`, `Subscriber`, `EventBus`. See the README's
//! "Notifications" section for the full rationale.
//!
//! Fully decoupled from plugins — a `VendorPlugin` only ever produces
//! `FirmwareMetadata`; deciding what's new/changed and fanning that out to
//! subscribers is the engine's job (see `dig_vendor` in engine.rs), not the
//! plugin's.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::model::{FirmwareMetadata, VersionDirection};

/// One field that differs between two observations of the same firmware
/// entry. `changed_fields` on `UpdatedRelease` is a `Vec` of these rather
/// than a single before/after struct so subscribers can render "what
/// changed" without re-deriving the diff themselves.
#[derive(Debug, Clone)]
pub struct FieldDiff {
    pub field: &'static str,
    pub before: String,
    pub after: String,
}

// `UpdatedRelease` carries two full `FirmwareMetadata`s, so it is much larger
// than `NewRelease`. Events are published a handful at a time per dig, so the
// wasted space doesn't matter, and boxing the fields would make every
// subscriber and constructor more awkward for no real gain.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum FirmwareEvent {
    NewRelease {
        firmware: FirmwareMetadata,
        first_seen: DateTime<Utc>,
    },
    UpdatedRelease {
        firmware: FirmwareMetadata,
        previous: FirmwareMetadata,
        changed_fields: Vec<FieldDiff>,
        version_direction: VersionDirection,
    },
}

#[derive(Debug, Error)]
pub enum SubscriberError {
    #[error("delivery failed: {0}")]
    Delivery(String),
    #[error("subscriber misconfigured: {0}")]
    Configuration(String),
}

#[async_trait]
pub trait Subscriber: Send + Sync {
    fn id(&self) -> &'static str;
    async fn notify(&self, event: &FirmwareEvent) -> Result<(), SubscriberError>;
}

/// Fans an event out to every registered subscriber concurrently. One
/// slow/broken subscriber never blocks another — failures are logged and
/// swallowed here rather than propagated, since a webhook being down
/// shouldn't fail the whole `dig`.
pub struct EventBus {
    subscribers: Vec<Box<dyn Subscriber>>,
}

impl EventBus {
    pub fn new(subscribers: Vec<Box<dyn Subscriber>>) -> Self {
        Self { subscribers }
    }

    pub async fn publish(&self, event: FirmwareEvent) {
        let futures = self.subscribers.iter().map(|s| s.notify(&event));
        for result in futures::future::join_all(futures).await {
            if let Err(e) = result {
                tracing::warn!(error = %e, "subscriber notification failed");
            }
        }
    }
}
