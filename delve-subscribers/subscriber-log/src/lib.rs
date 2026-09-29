//! Always-on structured-logging subscriber. See the README's
//! "Notifications" section: every event
//! is logged here regardless of what else is configured, so there's an
//! audit trail independent of whether webhook/email delivery succeeds. The
//! CLI wires this in unconditionally, not behind a Cargo feature.

use async_trait::async_trait;
use delve_core::prelude::*;

pub struct LogSubscriber;

#[async_trait]
impl Subscriber for LogSubscriber {
    fn id(&self) -> &'static str {
        "log"
    }

    async fn notify(&self, event: &FirmwareEvent) -> Result<(), SubscriberError> {
        match event {
            FirmwareEvent::NewRelease { firmware, first_seen } => {
                tracing::info!(
                    version = %firmware.version.raw,
                    first_seen = %first_seen,
                    "new firmware release detected"
                );
            }
            FirmwareEvent::UpdatedRelease {
                firmware,
                previous,
                changed_fields,
                version_direction,
            } => {
                tracing::info!(
                    version = %firmware.version.raw,
                    previous_version = %previous.version.raw,
                    ?version_direction,
                    field_count = changed_fields.len(),
                    "firmware release updated"
                );
            }
        }
        Ok(())
    }
}
