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

    /// Called once when a vendor's dig is over, after the last event, **whether
    /// the dig succeeded or failed**. A subscriber that holds events back (the
    /// email subscriber batches a dig's events into one message) sends them
    /// here. The default does nothing, for subscribers that act on each event
    /// as it arrives.
    async fn flush(&self) -> Result<(), SubscriberError> {
        Ok(())
    }
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

    /// Tells every subscriber the dig is over. Like `publish`, a failure is
    /// logged and does not stop the others.
    pub async fn flush(&self) {
        let futures = self.subscribers.iter().map(|s| s.flush());
        for result in futures::future::join_all(futures).await {
            if let Err(e) = result {
                tracing::warn!(error = %e, "subscriber flush failed");
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Flushed {
        flushes: Arc<AtomicUsize>,
        fail: bool,
    }

    #[async_trait]
    impl Subscriber for Flushed {
        fn id(&self) -> &'static str {
            "flushed"
        }

        async fn notify(&self, _event: &FirmwareEvent) -> Result<(), SubscriberError> {
            Ok(())
        }

        async fn flush(&self) -> Result<(), SubscriberError> {
            self.flushes.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(SubscriberError::Delivery("down".into()))
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn flush_reaches_every_subscriber_even_when_one_fails() {
        let (a, b) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let bus = EventBus::new(vec![
            Box::new(Flushed {
                flushes: a.clone(),
                fail: true,
            }),
            Box::new(Flushed {
                flushes: b.clone(),
                fail: false,
            }),
        ]);

        bus.flush().await;

        assert_eq!((a.load(Ordering::SeqCst), b.load(Ordering::SeqCst)), (1, 1));
    }

    struct NoFlush;

    #[async_trait]
    impl Subscriber for NoFlush {
        fn id(&self) -> &'static str {
            "no-flush"
        }

        async fn notify(&self, _event: &FirmwareEvent) -> Result<(), SubscriberError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_subscriber_that_does_not_batch_needs_no_flush() {
        // The default `flush` does nothing, so existing subscribers are unchanged.
        assert!(NoFlush.flush().await.is_ok());
    }
}
