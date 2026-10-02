//! Webhook subscriber — POSTs a JSON payload to a configured URL for every
//! event. Feature-gated in `delve-cli`, same pattern as vendor crates (see
//! the README's "Notifications" section, which documents the payload).
//!
//! The payload is its own type, separate from `FirmwareEvent`, so the JSON
//! can evolve without coupling to the engine's internal representation.
//! Its `schema` number says which shape a receiver is looking at; receivers
//! should ignore keys they don't know.

use std::time::Duration;

use async_trait::async_trait;
use chrono::SecondsFormat;
use delve_core::prelude::*;
use serde::Serialize;

/// The payload shape. Bump it when a key is removed or changes meaning;
/// adding a key does not need a new number.
pub const SCHEMA_VERSION: u32 = 1;

/// How long one request may take, connecting included. Without a limit, an
/// endpoint that hangs holds up the end of the dig, since the event bus waits
/// for every subscriber.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct WebhookSubscriber {
    url: String,
    client: reqwest::Client,
}

impl WebhookSubscriber {
    pub fn new(url: impl Into<String>) -> Self {
        Self::with_timeout(url, DEFAULT_TIMEOUT)
    }

    /// Like [`new`](Self::new), with `timeout` as the limit on a request.
    ///
    /// # Panics
    ///
    /// If the HTTP client cannot be built, which `reqwest::Client::new`
    /// would also panic on.
    pub fn with_timeout(url: impl Into<String>, timeout: Duration) -> Self {
        Self {
            url: url.into(),
            client: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("a reqwest client with a timeout builds"),
        }
    }
}

/// The wire payload. Every key is always present: a value that doesn't apply
/// to an event is `null`, so a receiver never has to test for a missing key.
#[derive(Serialize)]
struct WebhookPayload<'a> {
    schema: u32,
    /// `NewRelease` or `UpdatedRelease`.
    kind: &'static str,
    vendor: &'a str,
    device_family: &'a str,
    hardware_targets: &'a [String],
    /// A human-readable product name, when the plugin has one.
    display_name: Option<&'a str>,
    version: &'a str,
    /// `UpdatedRelease` only.
    previous_version: Option<&'a str>,
    /// `newer`, `older` or `unordered`. `UpdatedRelease` only.
    version_direction: Option<&'static str>,
    /// `YYYY-MM-DD`.
    release_date: Option<String>,
    /// Lower-case hex.
    sha256: Option<String>,
    source_url: &'a str,
    release_notes_url: Option<&'a str>,
    /// What differs from the previous observation. `UpdatedRelease` only.
    changed_fields: Option<Vec<ChangedField<'a>>>,
    /// When delve first saw the release, as RFC 3339 to the second, in UTC
    /// (`2026-10-02T03:30:00Z`). `NewRelease` only.
    first_seen: Option<String>,
}

#[derive(Serialize)]
struct ChangedField<'a> {
    field: &'static str,
    before: &'a str,
    after: &'a str,
}

impl<'a> From<&'a FirmwareEvent> for WebhookPayload<'a> {
    fn from(event: &'a FirmwareEvent) -> Self {
        let (kind, firmware, previous, direction, changed, first_seen) = match event {
            FirmwareEvent::NewRelease {
                firmware,
                first_seen,
            } => (
                "NewRelease",
                firmware,
                None,
                None,
                None,
                Some(first_seen.to_rfc3339_opts(SecondsFormat::Secs, true)),
            ),
            FirmwareEvent::UpdatedRelease {
                firmware,
                previous,
                changed_fields,
                version_direction,
            } => (
                "UpdatedRelease",
                firmware,
                Some(previous),
                Some(match version_direction {
                    VersionDirection::Newer => "newer",
                    VersionDirection::Older => "older",
                    VersionDirection::Unordered => "unordered",
                }),
                Some(
                    changed_fields
                        .iter()
                        .map(|d| ChangedField {
                            field: d.field,
                            before: &d.before,
                            after: &d.after,
                        })
                        .collect(),
                ),
                None,
            ),
        };

        Self {
            schema: SCHEMA_VERSION,
            kind,
            vendor: &firmware.vendor,
            device_family: &firmware.device_family,
            hardware_targets: &firmware.hardware_targets,
            display_name: firmware.display_name.as_deref(),
            version: &firmware.version.raw,
            previous_version: previous.map(|p| p.version.raw.as_str()),
            version_direction: direction,
            release_date: firmware.release_date.map(|d| d.to_string()),
            sha256: firmware
                .sha256
                .map(|h| h.iter().map(|b| format!("{b:02x}")).collect()),
            source_url: firmware.source_url.as_str(),
            release_notes_url: firmware.release_notes_url.as_ref().map(|u| u.as_str()),
            changed_fields: changed,
            first_seen,
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone, Utc};
    use serde_json::{json, Value};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Instant;

    fn firmware(version: &str, sha: u8) -> FirmwareMetadata {
        FirmwareMetadata {
            vendor: "unifi".into(),
            device_family: "USW".into(),
            source_url: "https://fw.example.test/api/firmware/abc".parse().unwrap(),
            version: VersionKey {
                raw: version.into(),
                scheme: VersionScheme::Semver,
                ordinal: Some(vec![1]),
            },
            release_date: NaiveDate::from_ymd_opt(2025, 8, 25),
            sha256: Some([sha; 32]),
            signature: None,
            hardware_targets: vec!["USMINI".into()],
            release_notes_url: Some("https://fw.example.test/notes".parse().unwrap()),
            display_name: Some("Switch Flex Mini".into()),
        }
    }

    fn new_release() -> FirmwareEvent {
        FirmwareEvent::NewRelease {
            firmware: firmware("v2.1.6", 0xab),
            first_seen: Utc.with_ymd_and_hms(2026, 10, 2, 3, 30, 0).unwrap(),
        }
    }

    fn updated(direction: VersionDirection) -> FirmwareEvent {
        FirmwareEvent::UpdatedRelease {
            firmware: firmware("v2.1.6", 0xab),
            previous: firmware("v2.1.3", 0xcd),
            changed_fields: vec![
                FieldDiff {
                    field: "version",
                    before: "v2.1.3".into(),
                    after: "v2.1.6".into(),
                },
                FieldDiff {
                    field: "sha256",
                    before: "cd".repeat(32),
                    after: "ab".repeat(32),
                },
            ],
            version_direction: direction,
        }
    }

    fn payload(event: &FirmwareEvent) -> Value {
        serde_json::to_value(WebhookPayload::from(event)).unwrap()
    }

    #[test]
    fn a_new_release_payload_is_complete() {
        assert_eq!(
            payload(&new_release()),
            json!({
                "schema": 1,
                "kind": "NewRelease",
                "vendor": "unifi",
                "device_family": "USW",
                "hardware_targets": ["USMINI"],
                "display_name": "Switch Flex Mini",
                "version": "v2.1.6",
                "previous_version": null,
                "version_direction": null,
                "release_date": "2025-08-25",
                "sha256": "ab".repeat(32),
                "source_url": "https://fw.example.test/api/firmware/abc",
                "release_notes_url": "https://fw.example.test/notes",
                "changed_fields": null,
                "first_seen": "2026-10-02T03:30:00Z",
            })
        );
    }

    #[test]
    fn an_updated_release_payload_is_complete() {
        assert_eq!(
            payload(&updated(VersionDirection::Newer)),
            json!({
                "schema": 1,
                "kind": "UpdatedRelease",
                "vendor": "unifi",
                "device_family": "USW",
                "hardware_targets": ["USMINI"],
                "display_name": "Switch Flex Mini",
                "version": "v2.1.6",
                "previous_version": "v2.1.3",
                "version_direction": "newer",
                "release_date": "2025-08-25",
                "sha256": "ab".repeat(32),
                "source_url": "https://fw.example.test/api/firmware/abc",
                "release_notes_url": "https://fw.example.test/notes",
                "changed_fields": [
                    {"field": "version", "before": "v2.1.3", "after": "v2.1.6"},
                    {"field": "sha256", "before": "cd".repeat(32), "after": "ab".repeat(32)},
                ],
                "first_seen": null,
            })
        );
    }

    #[test]
    fn every_key_is_always_present_so_receivers_never_test_for_a_missing_one() {
        let keys = |v: Value| -> Vec<String> {
            let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        // Both kinds have the same 15 keys, and a bare event still does.
        let mut bare = firmware("v1", 1);
        bare.release_date = None;
        bare.sha256 = None;
        bare.release_notes_url = None;
        bare.display_name = None;
        let bare_event = FirmwareEvent::NewRelease {
            firmware: bare,
            first_seen: Utc.with_ymd_and_hms(2026, 10, 2, 3, 30, 0).unwrap(),
        };

        let new = keys(payload(&new_release()));
        assert_eq!(new.len(), 15, "{new:?}");
        assert_eq!(new, keys(payload(&updated(VersionDirection::Newer))));
        assert_eq!(new, keys(payload(&bare_event)));

        let p = payload(&bare_event);
        for absent in [
            "display_name",
            "release_date",
            "sha256",
            "release_notes_url",
        ] {
            assert_eq!(p[absent], Value::Null, "{absent}");
        }
    }

    #[test]
    fn the_original_keys_keep_their_meaning_for_existing_receivers() {
        // `kind`, `version` and `previous_version` are what the first payload had.
        let p = payload(&updated(VersionDirection::Newer));
        assert_eq!(p["kind"], "UpdatedRelease");
        assert_eq!(p["version"], "v2.1.6");
        assert_eq!(p["previous_version"], "v2.1.3");
        assert_eq!(payload(&new_release())["kind"], "NewRelease");
        assert_eq!(payload(&new_release())["version"], "v2.1.6");
    }

    #[test]
    fn the_version_direction_is_spelled_out() {
        for (direction, text) in [
            (VersionDirection::Newer, "newer"),
            (VersionDirection::Older, "older"),
            (VersionDirection::Unordered, "unordered"),
        ] {
            assert_eq!(payload(&updated(direction))["version_direction"], text);
        }
    }

    /// Takes one request and answers it with `status`, after `delay`.
    /// Returns the URL and what the request carried: its content type and body.
    fn serve_once(status: u16, delay: Duration) -> (String, mpsc::Receiver<(String, String)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let (mut length, mut content_type) = (0, String::new());
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                let header = line.trim_end().to_ascii_lowercase();
                if header.is_empty() {
                    break;
                }
                if let Some(v) = header.strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
                if let Some(v) = header.strip_prefix("content-type:") {
                    content_type = v.trim().to_string();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let _ = tx.send((content_type, String::from_utf8(body).unwrap()));
            std::thread::sleep(delay);
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
        });
        (url, rx)
    }

    #[tokio::test]
    async fn the_payload_is_posted_as_json() {
        let (url, received) = serve_once(200, Duration::ZERO);
        WebhookSubscriber::new(url)
            .notify(&updated(VersionDirection::Newer))
            .await
            .unwrap();

        let (content_type, body) = received.recv().unwrap();
        assert!(
            content_type.starts_with("application/json"),
            "{content_type}"
        );
        let sent: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(sent, payload(&updated(VersionDirection::Newer)));
    }

    #[tokio::test]
    async fn an_error_status_is_a_delivery_error() {
        let (url, _received) = serve_once(500, Duration::ZERO);
        let err = WebhookSubscriber::new(url)
            .notify(&new_release())
            .await
            .unwrap_err();
        assert!(matches!(err, SubscriberError::Delivery(_)), "{err}");
    }

    #[tokio::test]
    async fn an_endpoint_that_hangs_times_out_instead_of_holding_up_the_dig() {
        // It reads the request and then says nothing for 10 seconds.
        let (url, _received) = serve_once(200, Duration::from_secs(10));
        let started = Instant::now();
        let err = WebhookSubscriber::with_timeout(url, Duration::from_millis(300))
            .notify(&new_release())
            .await
            .unwrap_err();

        assert!(matches!(err, SubscriberError::Delivery(_)), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }
}
