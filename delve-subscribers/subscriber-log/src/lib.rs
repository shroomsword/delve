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
            FirmwareEvent::NewRelease {
                firmware,
                first_seen,
            } => {
                tracing::info!(
                    vendor = %firmware.vendor,
                    device_family = %firmware.device_family,
                    hardware = %firmware.hardware_targets.join("+"),
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
                    vendor = %firmware.vendor,
                    device_family = %firmware.device_family,
                    hardware = %firmware.hardware_targets.join("+"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    /// Collects what the subscriber logs so a test can read it back.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn firmware(version: &str) -> FirmwareMetadata {
        FirmwareMetadata {
            vendor: "unifi".into(),
            device_family: "USW".into(),
            source_url: "https://example.test/fw".parse().unwrap(),
            version: VersionKey {
                raw: version.into(),
                scheme: VersionScheme::Semver,
                ordinal: Some(vec![1]),
            },
            release_date: None,
            sha256: None,
            signature: None,
            hardware_targets: vec!["USMINI".into()],
            release_notes_url: None,
        }
    }

    /// Logs one event and returns the line it produced.
    async fn logged(event: FirmwareEvent) -> String {
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        // The default flavor of `#[tokio::test]` runs on this thread, so a
        // thread-local subscriber sees the events.
        let guard = tracing::subscriber::set_default(subscriber);
        LogSubscriber.notify(&event).await.unwrap();
        drop(guard);
        let bytes = capture.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[tokio::test]
    async fn a_new_release_line_names_the_device() {
        let line = logged(FirmwareEvent::NewRelease {
            firmware: firmware("1.1"),
            first_seen: Utc.with_ymd_and_hms(2026, 10, 1, 12, 0, 0).unwrap(),
        })
        .await;
        for field in [
            "vendor=unifi",
            "device_family=USW",
            "hardware=USMINI",
            "version=1.1",
            "new firmware release detected",
        ] {
            assert!(line.contains(field), "missing {field}: {line}");
        }
    }

    #[tokio::test]
    async fn an_update_line_names_the_device_and_both_versions() {
        let line = logged(FirmwareEvent::UpdatedRelease {
            firmware: firmware("1.1"),
            previous: firmware("1.0"),
            changed_fields: vec![],
            version_direction: VersionDirection::Newer,
        })
        .await;
        for field in [
            "vendor=unifi",
            "device_family=USW",
            "hardware=USMINI",
            "version=1.1",
            "previous_version=1.0",
            "firmware release updated",
        ] {
            assert!(line.contains(field), "missing {field}: {line}");
        }
    }
}
