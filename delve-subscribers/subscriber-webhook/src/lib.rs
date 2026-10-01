//! Webhook subscriber — POSTs a JSON payload to a configured URL for every
//! event. Feature-gated in `delve-cli`, same pattern as vendor crates (see
//! the README's "Notifications" section).

use async_trait::async_trait;
use delve_core::prelude::*;
use serde::Serialize;

pub struct WebhookSubscriber {
    url: String,
    client: reqwest::Client,
}

impl WebhookSubscriber {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            client: reqwest::Client::new(),
        }
    }
}

/// Wire payload. Kept separate from `FirmwareEvent` rather than deriving
/// `Serialize` directly on it, so the webhook's JSON shape can evolve
/// without coupling to internal event representation.
#[derive(Serialize)]
#[serde(tag = "kind")]
enum WebhookPayload<'a> {
    NewRelease {
        version: &'a str,
    },
    UpdatedRelease {
        version: &'a str,
        previous_version: &'a str,
    },
}

impl<'a> From<&'a FirmwareEvent> for WebhookPayload<'a> {
    fn from(event: &'a FirmwareEvent) -> Self {
        match event {
            FirmwareEvent::NewRelease { firmware, .. } => WebhookPayload::NewRelease {
                version: &firmware.version.raw,
            },
            FirmwareEvent::UpdatedRelease {
                firmware, previous, ..
            } => WebhookPayload::UpdatedRelease {
                version: &firmware.version.raw,
                previous_version: &previous.version.raw,
            },
        }
    }
}

#[async_trait]
impl Subscriber for WebhookSubscriber {
    fn id(&self) -> &'static str {
        "webhook"
    }

    async fn notify(&self, event: &FirmwareEvent) -> Result<(), SubscriberError> {
        let payload = WebhookPayload::from(event);
        self.client
            .post(&self.url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| SubscriberError::Delivery(e.to_string()))?
            .error_for_status()
            .map_err(|e| SubscriberError::Delivery(e.to_string()))?;
        Ok(())
    }
}
