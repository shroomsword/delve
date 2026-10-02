//! Email subscriber — sends plain-text messages over SMTP. Feature-gated in
//! `delve-cli`, same pattern as `subscriber-webhook` (see the README's
//! "Notifications" section).
//!
//! **Batching.** By default the events of one dig are held back and sent as
//! one message when the dig is over (`Subscriber::flush`): a single event
//! gets the ordinary message, and two or more get a digest, split into several
//! messages only past `max_events_per_message`. Without batching every event
//! is sent as it arrives. Either way a vendor's first dig is a silent
//! baseline, so a busy dig means a later one that finds a lot at once.
//!
//! Events are only in memory until the flush, so a process that is killed
//! before it loses them. The engine flushes however a dig ends, including
//! when it fails part-way.

mod digest;

use std::fmt::Display;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use delve_core::prelude::*;
use lettre::message::header::ContentType;
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

/// How the connection to the SMTP server is secured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TlsMode {
    /// Connect in the clear, then upgrade with STARTTLS. Usually port 587.
    /// The upgrade is required: if the server doesn't offer it, sending
    /// fails instead of falling back to plain text.
    #[default]
    StartTls,
    /// TLS from the first byte ("SMTPS"). Usually port 465.
    Implicit,
    /// No encryption. Only for a local relay or a test server: credentials
    /// and message contents cross the network in the clear.
    None,
}

/// Everything needed to send mail. Built by `delve-cli` from
/// `[subscribers.email]`, with any `env:` password already resolved.
#[derive(Debug, Clone)]
pub struct EmailSettings {
    pub host: String,
    /// `None` uses the mode's usual port (587, 465, or 25).
    pub port: Option<u16>,
    pub tls: TlsMode,
    /// A bare address or `Name <address>`.
    pub from: String,
    pub to: Vec<String>,
    /// Username and password, sent with whatever mechanism the server offers.
    /// `None` sends without authenticating.
    pub credentials: Option<(String, String)>,
    /// Send one message per dig instead of one per event.
    pub batch: bool,
    /// With `batch`, the most events in one message; a bigger burst is split
    /// into several. At least 1.
    pub max_events_per_message: usize,
}

/// How long to wait on the SMTP server for any one step.
const SMTP_TIMEOUT: Duration = Duration::from_secs(30);

pub struct EmailSubscriber<T> {
    transport: T,
    from: Mailbox,
    to: Vec<Mailbox>,
    /// `Some(max)` when batching, with at most `max` events per message.
    batching: Option<usize>,
    /// Events held back until the next flush.
    pending: Mutex<Vec<FirmwareEvent>>,
}

impl EmailSubscriber<AsyncSmtpTransport<Tokio1Executor>> {
    /// Builds the subscriber and its SMTP transport. Doesn't connect: a
    /// server that is down fails the first `notify`, not this call.
    ///
    /// # Errors
    ///
    /// `SubscriberError::Configuration` if an address doesn't parse, `to`
    /// is empty, or the host name is rejected.
    pub fn smtp(settings: &EmailSettings) -> Result<Self, SubscriberError> {
        let builder = match settings.tls {
            TlsMode::StartTls => {
                AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&settings.host)
                    .map_err(|e| config_error(format!("SMTP host '{}': {e}", settings.host)))?
            }
            TlsMode::Implicit => AsyncSmtpTransport::<Tokio1Executor>::relay(&settings.host)
                .map_err(|e| config_error(format!("SMTP host '{}': {e}", settings.host)))?,
            TlsMode::None => {
                AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&settings.host)
            }
        };
        let builder = match settings.port {
            Some(port) => builder.port(port),
            None => builder,
        };
        let builder = match &settings.credentials {
            Some((user, password)) => {
                builder.credentials(Credentials::new(user.clone(), password.clone()))
            }
            None => builder,
        };
        let transport = builder.timeout(Some(SMTP_TIMEOUT)).build();
        let subscriber = Self::with_transport(transport, &settings.from, &settings.to)?;
        Ok(if settings.batch {
            subscriber.batched(settings.max_events_per_message)
        } else {
            subscriber
        })
    }
}

impl<T> EmailSubscriber<T> {
    /// Uses `transport` to send. `smtp` is the normal route; this exists so
    /// tests can swap in a stub.
    ///
    /// # Errors
    ///
    /// `SubscriberError::Configuration` if an address doesn't parse or `to`
    /// is empty.
    pub fn with_transport(
        transport: T,
        from: &str,
        to: &[String],
    ) -> Result<Self, SubscriberError> {
        let from: Mailbox = from
            .parse()
            .map_err(|e| config_error(format!("from address '{from}': {e}")))?;
        if to.is_empty() {
            return Err(config_error("no recipients: `to` is empty".into()));
        }
        let to = to
            .iter()
            .map(|a| {
                a.parse::<Mailbox>()
                    .map_err(|e| config_error(format!("to address '{a}': {e}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            transport,
            from,
            to,
            batching: None,
            pending: Mutex::new(Vec::new()),
        })
    }

    /// Holds events back and sends a digest on `flush`, with at most
    /// `max_events_per_message` events in each message (at least 1).
    #[must_use]
    pub fn batched(mut self, max_events_per_message: usize) -> Self {
        self.batching = Some(max_events_per_message.max(1));
        self
    }
}

fn config_error(message: String) -> SubscriberError {
    SubscriberError::Configuration(message)
}

#[async_trait]
impl<T> Subscriber for EmailSubscriber<T>
where
    T: AsyncTransport + Send + Sync,
    T::Error: Display,
{
    fn id(&self) -> &'static str {
        "email"
    }

    async fn notify(&self, event: &FirmwareEvent) -> Result<(), SubscriberError> {
        if self.batching.is_some() {
            self.pending
                .lock()
                .expect("pending events lock")
                .push(event.clone());
            return Ok(());
        }
        let (subject, body) = render(event);
        self.send(subject, body).await
    }

    async fn flush(&self) -> Result<(), SubscriberError> {
        let Some(max) = self.batching else {
            return Ok(());
        };
        let events = std::mem::take(&mut *self.pending.lock().expect("pending events lock"));
        self.send_batch(events, max).await
    }
}

impl<T> EmailSubscriber<T>
where
    T: AsyncTransport + Send + Sync,
    T::Error: Display,
{
    async fn send(&self, subject: String, body: String) -> Result<(), SubscriberError> {
        let mut message = Message::builder().from(self.from.clone());
        for recipient in &self.to {
            message = message.to(recipient.clone());
        }
        let message = message
            .subject(subject)
            .header(ContentType::TEXT_PLAIN)
            .body(body)
            .map_err(|e| SubscriberError::Delivery(format!("building the message: {e}")))?;

        self.transport
            .send(message)
            .await
            .map_err(|e| SubscriberError::Delivery(e.to_string()))?;
        Ok(())
    }

    /// Sends what a dig held back: nothing for no events, the ordinary
    /// message for one, and a digest (or several, past `max` events) for more.
    /// Every message is tried even if an earlier one fails.
    async fn send_batch(
        &self,
        events: Vec<FirmwareEvent>,
        max: usize,
    ) -> Result<(), SubscriberError> {
        match events.as_slice() {
            [] => return Ok(()),
            [only] => {
                let (subject, body) = render(only);
                return self.send(subject, body).await;
            }
            _ => {}
        }

        let events = digest::sorted(events);
        let chunks: Vec<&[FirmwareEvent]> = events.chunks(max).collect();
        let total = chunks.len();
        let (mut failed, mut first_error) = (0, None);
        for (i, chunk) in chunks.into_iter().enumerate() {
            let part = (total > 1).then_some((i + 1, total));
            let (subject, body) = digest::render_digest(chunk, part);
            if let Err(e) = self.send(subject, body).await {
                failed += 1;
                first_error.get_or_insert(e);
            }
        }
        match first_error {
            None => Ok(()),
            Some(e) if total == 1 => Err(e),
            Some(e) => Err(SubscriberError::Delivery(format!(
                "{failed} of {total} messages failed; the first error was: {e}"
            ))),
        }
    }
}

/// The subject and plain-text body for an event.
///
/// Vendor data is untrusted, so the subject is a single line with control
/// characters (including newlines) replaced by spaces.
fn render(event: &FirmwareEvent) -> (String, String) {
    match event {
        FirmwareEvent::NewRelease {
            firmware,
            first_seen,
        } => {
            let subject = format!(
                "[delve] New firmware: {} {}",
                device_label(firmware),
                firmware.version.raw
            );
            let mut body = describe(firmware);
            body.push_str(&format!("First seen:    {first_seen}\n"));
            (single_line(&subject), body)
        }
        FirmwareEvent::UpdatedRelease {
            firmware,
            previous,
            changed_fields,
            version_direction,
        } => {
            let same_version = previous.version.raw == firmware.version.raw;
            let subject = if same_version {
                format!(
                    "[delve] Firmware rebuilt under the same version: {} {}",
                    device_label(firmware),
                    firmware.version.raw
                )
            } else {
                let what = match version_direction {
                    VersionDirection::Newer => "Firmware updated",
                    VersionDirection::Older => "Firmware DOWNGRADED",
                    VersionDirection::Unordered => "Firmware changed",
                };
                format!(
                    "[delve] {what}: {} {} -> {}",
                    device_label(firmware),
                    previous.version.raw,
                    firmware.version.raw
                )
            };

            let mut body = describe(firmware);
            body.push_str(&format!(
                "Previous:      {} ({})\n",
                previous.version.raw,
                match version_direction {
                    VersionDirection::Newer => "this is newer",
                    VersionDirection::Older => "this is older",
                    VersionDirection::Unordered => "order unknown",
                }
            ));
            if !changed_fields.is_empty() {
                body.push_str("\nChanged fields:\n");
                for diff in changed_fields {
                    body.push_str(&format!(
                        "  {}: {} -> {}\n",
                        diff.field, diff.before, diff.after
                    ));
                }
            }
            (single_line(&subject), body)
        }
    }
}

/// Names the device in a subject: the vendor, the device family, and the
/// hardware in parentheses, such as `unifi USW (USMINI)`. A family can span
/// many models (UniFi's `USW` is every switch), so the hardware is what says
/// which one changed. It is left out when it would only repeat the family,
/// as for a UniFi model that has no product line.
///
/// The display name is deliberately not here. Putting it in made a UniFi
/// subject a median 93 characters (up to 109) against 74 (at most 78), and the
/// version, the part that matters most, moved past column 60 in most of them,
/// where an inbox preview cuts it off. It goes in the body instead.
pub(crate) fn device_label(firmware: &FirmwareMetadata) -> String {
    let hardware = firmware.hardware_targets.join("+");
    if hardware.is_empty() || hardware == firmware.device_family {
        format!("{} {}", firmware.vendor, firmware.device_family)
    } else {
        format!(
            "{} {} ({hardware})",
            firmware.vendor, firmware.device_family
        )
    }
}

/// The lines every message starts with.
fn describe(firmware: &FirmwareMetadata) -> String {
    let mut out = String::new();
    out.push_str(&format!("Vendor:        {}\n", firmware.vendor));
    if let Some(name) = &firmware.display_name {
        out.push_str(&format!("Product:       {name}\n"));
    }
    out.push_str(&format!("Device family: {}\n", firmware.device_family));
    out.push_str(&format!(
        "Hardware:      {}\n",
        firmware.hardware_targets.join(", ")
    ));
    out.push_str(&format!("Version:       {}\n", firmware.version.raw));
    if let Some(date) = firmware.release_date {
        out.push_str(&format!("Released:      {date}\n"));
    }
    if let Some(sha) = firmware.sha256 {
        let hex: String = sha.iter().map(|b| format!("{b:02x}")).collect();
        out.push_str(&format!("SHA-256:       {hex}\n"));
    }
    out.push_str(&format!("Source:        {}\n", firmware.source_url));
    if let Some(url) = &firmware.release_notes_url {
        out.push_str(&format!("Release notes: {url}\n"));
    }
    out
}

pub(crate) fn single_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone, Utc};
    use lettre::transport::stub::AsyncStubTransport;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    fn firmware(version: &str, sha: u8) -> FirmwareMetadata {
        FirmwareMetadata {
            vendor: "acme".into(),
            device_family: "widget".into(),
            source_url: "https://acme.example.test/widget/fw".parse().unwrap(),
            version: VersionKey {
                raw: version.into(),
                scheme: VersionScheme::Semver,
                ordinal: Some(vec![1, 0]),
            },
            release_date: NaiveDate::from_ymd_opt(2026, 9, 1),
            sha256: Some([sha; 32]),
            signature: None,
            hardware_targets: vec!["rev-a".into(), "rev-b".into()],
            release_notes_url: Some("https://acme.example.test/notes".parse().unwrap()),
            display_name: None,
        }
    }

    fn new_release(version: &str) -> FirmwareEvent {
        FirmwareEvent::NewRelease {
            firmware: firmware(version, 1),
            first_seen: Utc.with_ymd_and_hms(2026, 10, 1, 12, 0, 0).unwrap(),
        }
    }

    fn updated(previous: &str, current: &str, direction: VersionDirection) -> FirmwareEvent {
        FirmwareEvent::UpdatedRelease {
            firmware: firmware(current, 2),
            previous: firmware(previous, 1),
            changed_fields: vec![FieldDiff {
                field: "sha256",
                before: "aa".into(),
                after: "bb".into(),
            }],
            version_direction: direction,
        }
    }

    fn recipients(addresses: &[&str]) -> Vec<String> {
        addresses.iter().map(|a| a.to_string()).collect()
    }

    // ---- rendering ----

    #[test]
    fn a_new_release_names_the_firmware_in_the_subject_and_body() {
        let (subject, body) = render(&new_release("1.0"));
        assert_eq!(
            subject,
            "[delve] New firmware: acme widget (rev-a+rev-b) 1.0"
        );
        assert!(body.contains("Vendor:        acme\n"), "{body}");
        assert!(body.contains("Hardware:      rev-a, rev-b\n"), "{body}");
        assert!(body.contains("Released:      2026-09-01\n"), "{body}");
        assert!(
            body.contains(&format!("SHA-256:       {}\n", "01".repeat(32))),
            "{body}"
        );
        assert!(
            body.contains("Source:        https://acme.example.test/widget/fw\n"),
            "{body}"
        );
        assert!(
            body.contains("Release notes: https://acme.example.test/notes\n"),
            "{body}"
        );
        assert!(
            body.contains("First seen:    2026-10-01 12:00:00 UTC\n"),
            "{body}"
        );
    }

    #[test]
    fn an_update_says_which_way_the_version_moved() {
        let (subject, body) = render(&updated("1.0", "1.1", VersionDirection::Newer));
        assert_eq!(
            subject,
            "[delve] Firmware updated: acme widget (rev-a+rev-b) 1.0 -> 1.1"
        );
        assert!(
            body.contains("Previous:      1.0 (this is newer)\n"),
            "{body}"
        );
        assert!(body.contains("  sha256: aa -> bb\n"), "{body}");

        let (subject, _) = render(&updated("1.1", "1.0", VersionDirection::Older));
        assert_eq!(
            subject,
            "[delve] Firmware DOWNGRADED: acme widget (rev-a+rev-b) 1.1 -> 1.0"
        );

        let (subject, body) = render(&updated("r1", "r2", VersionDirection::Unordered));
        assert_eq!(
            subject,
            "[delve] Firmware changed: acme widget (rev-a+rev-b) r1 -> r2"
        );
        assert!(body.contains("(order unknown)"), "{body}");
    }

    #[test]
    fn the_subject_names_the_hardware_when_the_family_covers_many_models() {
        let mut event = new_release("1.0");
        if let FirmwareEvent::NewRelease { firmware, .. } = &mut event {
            firmware.device_family = "USW".into();
            firmware.hardware_targets = vec!["USMINI".into()];
        }
        assert_eq!(
            render(&event).0,
            "[delve] New firmware: acme USW (USMINI) 1.0"
        );
    }

    #[test]
    fn the_display_name_is_in_the_body_but_not_the_subject() {
        let mut event = new_release("1.0");
        if let FirmwareEvent::NewRelease { firmware, .. } = &mut event {
            firmware.device_family = "USW".into();
            firmware.hardware_targets = vec!["USMINI".into()];
            firmware.display_name = Some("Switch Flex Mini".into());
        }
        let (subject, body) = render(&event);
        // The name in the subject would push the version past column 60 for
        // most UniFi models, so only the codes are there.
        assert_eq!(subject, "[delve] New firmware: acme USW (USMINI) 1.0");
        assert!(body.contains("Product:       Switch Flex Mini\n"), "{body}");
    }

    #[test]
    fn there_is_no_product_line_without_a_display_name() {
        let (_, body) = render(&new_release("1.0"));
        assert!(!body.contains("Product:"), "{body}");
    }

    #[test]
    fn the_subject_does_not_repeat_the_hardware_when_it_is_the_family() {
        let mut event = new_release("1.0");
        if let FirmwareEvent::NewRelease { firmware, .. } = &mut event {
            firmware.device_family = "USPRPS".into(); // a model with no product line
            firmware.hardware_targets = vec!["USPRPS".into()];
        }
        assert_eq!(render(&event).0, "[delve] New firmware: acme USPRPS 1.0");

        if let FirmwareEvent::NewRelease { firmware, .. } = &mut event {
            firmware.hardware_targets = vec![];
        }
        assert_eq!(render(&event).0, "[delve] New firmware: acme USPRPS 1.0");
    }

    #[test]
    fn a_rebuild_under_the_same_version_is_called_that() {
        let (subject, _) = render(&updated("1.0", "1.0", VersionDirection::Unordered));
        assert_eq!(
            subject,
            "[delve] Firmware rebuilt under the same version: acme widget (rev-a+rev-b) 1.0"
        );
    }

    #[test]
    fn vendor_data_cannot_add_lines_to_the_subject() {
        let (subject, _) = render(&new_release("1.0\r\nBcc: attacker@example.test"));
        assert!(
            !subject.contains('\n') && !subject.contains('\r'),
            "{subject:?}"
        );
        assert_eq!(
            subject,
            "[delve] New firmware: acme widget (rev-a+rev-b) 1.0  Bcc: attacker@example.test"
        );
    }

    // ---- configuration ----

    fn rejected(result: Result<EmailSubscriber<()>, SubscriberError>) -> String {
        match result {
            Err(SubscriberError::Configuration(message)) => message,
            Err(other) => panic!("expected a configuration error, got {other}"),
            Ok(_) => panic!("expected a configuration error"),
        }
    }

    #[test]
    fn bad_addresses_and_no_recipients_are_configuration_errors() {
        let bad_from = rejected(EmailSubscriber::with_transport(
            (),
            "not an address",
            &recipients(&["ops@example.test"]),
        ));
        assert!(
            bad_from.contains("from address 'not an address'"),
            "{bad_from}"
        );

        let bad_to = rejected(EmailSubscriber::with_transport(
            (),
            "delve@example.test",
            &recipients(&["ops@example.test", "nope"]),
        ));
        assert!(bad_to.contains("to address 'nope'"), "{bad_to}");

        let none = rejected(EmailSubscriber::with_transport(
            (),
            "delve@example.test",
            &[],
        ));
        assert!(none.contains("no recipients"), "{none}");
    }

    #[test]
    fn every_tls_mode_builds_a_transport_without_connecting() {
        for tls in [TlsMode::StartTls, TlsMode::Implicit, TlsMode::None] {
            let settings = EmailSettings {
                host: "smtp.example.test".into(),
                port: None,
                tls,
                from: "Delve <delve@example.test>".into(),
                to: recipients(&["ops@example.test"]),
                credentials: Some(("user".into(), "secret".into())),
                batch: false,
                max_events_per_message: 50,
            };
            assert!(EmailSubscriber::smtp(&settings).is_ok(), "{tls:?}");
        }
    }

    // ---- sending, through a stub transport ----

    #[tokio::test]
    async fn notify_sends_one_message_to_every_recipient() {
        let transport = AsyncStubTransport::new_ok();
        let subscriber = EmailSubscriber::with_transport(
            transport.clone(),
            "Delve <delve@example.test>",
            &recipients(&["ops@example.test", "Sec Team <sec@example.test>"]),
        )
        .unwrap();

        subscriber.notify(&new_release("1.0")).await.unwrap();

        let sent = transport.messages().await;
        assert_eq!(sent.len(), 1);
        let (envelope, raw) = &sent[0];
        assert_eq!(envelope.from().unwrap().to_string(), "delve@example.test");
        let to: Vec<String> = envelope.to().iter().map(|a| a.to_string()).collect();
        assert_eq!(to, ["ops@example.test", "sec@example.test"]);
        assert!(
            raw.contains("Subject: [delve] New firmware: acme widget (rev-a+rev-b) 1.0"),
            "{raw}"
        );
        assert!(raw.contains("Content-Type: text/plain"), "{raw}");
        assert!(raw.contains("Device family: widget"), "{raw}");
    }

    #[tokio::test]
    async fn a_failing_transport_is_a_delivery_error() {
        let subscriber = EmailSubscriber::with_transport(
            AsyncStubTransport::new_error(),
            "delve@example.test",
            &recipients(&["ops@example.test"]),
        )
        .unwrap();

        let err = subscriber.notify(&new_release("1.0")).await.unwrap_err();
        assert!(matches!(err, SubscriberError::Delivery(_)), "{err}");
    }

    // ---- sending, over real SMTP to an in-process fake server ----

    /// Accepts one connection and plays just enough SMTP to take a message.
    /// Returns its address and everything the client sent.
    fn fake_smtp_server() -> (std::net::SocketAddr, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = received.clone();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut say = |line: &str| stream.write_all(format!("{line}\r\n").as_bytes()).unwrap();
            say("220 fake ESMTP");
            let mut in_data = false;
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let text = line.trim_end().to_string();
                log.lock().unwrap().push(text.clone());
                if in_data {
                    if text == "." {
                        in_data = false;
                        say("250 queued");
                    }
                    continue;
                }
                let command = text.to_ascii_uppercase();
                if command.starts_with("EHLO") || command.starts_with("HELO") {
                    say("250 fake");
                } else if command == "DATA" {
                    in_data = true;
                    say("354 go ahead");
                } else if command == "QUIT" {
                    say("221 bye");
                    break;
                } else {
                    say("250 ok");
                }
            }
        });
        (addr, received)
    }

    #[tokio::test]
    async fn smtp_delivers_the_message_with_the_right_envelope() {
        let (addr, received) = fake_smtp_server();
        let subscriber = EmailSubscriber::smtp(&EmailSettings {
            host: addr.ip().to_string(),
            port: Some(addr.port()),
            tls: TlsMode::None,
            from: "Delve <delve@example.test>".into(),
            to: recipients(&["ops@example.test", "sec@example.test"]),
            credentials: None,
            batch: false,
            max_events_per_message: 50,
        })
        .unwrap();

        subscriber
            .notify(&updated("1.0", "1.1", VersionDirection::Newer))
            .await
            .unwrap();

        let log = received.lock().unwrap().join("\n");
        assert!(log.contains("MAIL FROM:<delve@example.test>"), "{log}");
        assert!(log.contains("RCPT TO:<ops@example.test>"), "{log}");
        assert!(log.contains("RCPT TO:<sec@example.test>"), "{log}");
        assert!(
            log.contains("Subject: [delve] Firmware updated: acme widget (rev-a+rev-b) 1.0 -> 1.1"),
            "{log}"
        );
        assert!(log.contains("Previous:      1.0 (this is newer)"), "{log}");
    }

    #[tokio::test]
    async fn smtp_to_a_server_that_is_down_is_a_delivery_error() {
        // Reserve a port, then close it so nothing is listening.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let subscriber = EmailSubscriber::smtp(&EmailSettings {
            host: "127.0.0.1".into(),
            port: Some(port),
            tls: TlsMode::None,
            from: "delve@example.test".into(),
            to: recipients(&["ops@example.test"]),
            credentials: None,
            batch: false,
            max_events_per_message: 50,
        })
        .unwrap();

        let err = subscriber.notify(&new_release("1.0")).await.unwrap_err();
        assert!(matches!(err, SubscriberError::Delivery(_)), "{err}");
    }

    // ---- batching ----

    /// A new release of `acme widget` at version `1.<n>`, ordered by `n`.
    fn release(n: u64) -> FirmwareEvent {
        let mut firmware = firmware(&format!("1.{n}"), n as u8);
        firmware.version.ordinal = Some(vec![1, n]);
        FirmwareEvent::NewRelease {
            firmware,
            first_seen: Utc.with_ymd_and_hms(2026, 10, 1, 12, 0, 0).unwrap(),
        }
    }

    fn batched(transport: AsyncStubTransport, max: usize) -> EmailSubscriber<AsyncStubTransport> {
        EmailSubscriber::with_transport(
            transport,
            "delve@example.test",
            &recipients(&["ops@example.test"]),
        )
        .unwrap()
        .batched(max)
    }

    /// The subjects of what a stub transport was asked to send.
    async fn subjects(transport: &AsyncStubTransport) -> Vec<String> {
        transport
            .messages()
            .await
            .iter()
            .map(|(_, raw)| {
                let line = raw.lines().find(|l| l.starts_with("Subject:")).unwrap();
                line.trim_start_matches("Subject: ").to_string()
            })
            .collect()
    }

    #[tokio::test]
    async fn a_batching_subscriber_sends_nothing_until_flush() {
        let transport = AsyncStubTransport::new_ok();
        let subscriber = batched(transport.clone(), 50);

        subscriber.notify(&release(1)).await.unwrap();
        subscriber.notify(&release(2)).await.unwrap();
        assert!(transport.messages().await.is_empty());

        subscriber.flush().await.unwrap();
        assert_eq!(transport.messages().await.len(), 1);
    }

    #[tokio::test]
    async fn a_burst_is_one_digest_that_counts_the_changes() {
        let transport = AsyncStubTransport::new_ok();
        let subscriber = batched(transport.clone(), 50);
        for n in 1..=26 {
            subscriber.notify(&release(n)).await.unwrap();
        }
        subscriber.flush().await.unwrap();

        let sent = transport.messages().await;
        assert_eq!(sent.len(), 1, "26 events must be one message");
        assert_eq!(
            subjects(&transport).await,
            ["[delve] 26 firmware changes: acme widget (rev-a+rev-b)"]
        );
        let raw = &sent[0].1;
        assert!(
            raw.contains("26 firmware changes found in this dig."),
            "{raw}"
        );
        // Newest first within the device.
        let (first, last) = (
            raw.find("1.26").unwrap(),
            raw.find("1.1 ").unwrap_or(usize::MAX),
        );
        assert!(first < last, "{raw}");
    }

    #[tokio::test]
    async fn one_event_gets_the_ordinary_message_not_a_digest() {
        let transport = AsyncStubTransport::new_ok();
        let subscriber = batched(transport.clone(), 50);
        subscriber.notify(&release(1)).await.unwrap();
        subscriber.flush().await.unwrap();

        assert_eq!(
            subjects(&transport).await,
            [render(&release(1)).0],
            "a quiet dig must look exactly as it does without batching"
        );
    }

    #[tokio::test]
    async fn no_events_send_nothing_and_a_second_flush_sends_nothing_new() {
        let transport = AsyncStubTransport::new_ok();
        let subscriber = batched(transport.clone(), 50);
        subscriber.flush().await.unwrap();
        assert!(transport.messages().await.is_empty());

        subscriber.notify(&release(1)).await.unwrap();
        subscriber.notify(&release(2)).await.unwrap();
        subscriber.flush().await.unwrap();
        subscriber.flush().await.unwrap();
        assert_eq!(
            transport.messages().await.len(),
            1,
            "what was sent is not kept"
        );
    }

    #[tokio::test]
    async fn a_burst_past_the_cap_is_split_into_numbered_messages() {
        let transport = AsyncStubTransport::new_ok();
        let subscriber = batched(transport.clone(), 3);
        for n in 1..=7 {
            subscriber.notify(&release(n)).await.unwrap();
        }
        subscriber.flush().await.unwrap();

        let subjects = subjects(&transport).await;
        assert_eq!(subjects.len(), 3, "{subjects:?}");
        assert!(subjects[0].ends_with("(part 1 of 3)"), "{subjects:?}");
        assert!(subjects[2].ends_with("(part 3 of 3)"), "{subjects:?}");
        assert!(
            subjects[0].starts_with("[delve] 3 firmware changes"),
            "{subjects:?}"
        );
        assert!(
            subjects[2].starts_with("[delve] 1 firmware change:"),
            "{subjects:?}"
        );

        // Every event is in exactly one message.
        let all: String = transport
            .messages()
            .await
            .iter()
            .map(|m| m.1.clone())
            .collect();
        for n in 1..=7 {
            assert_eq!(
                all.matches(&format!("1.{n}\n")).count()
                    + all.matches(&format!("1.{n}\r\n")).count(),
                1,
                "1.{n}"
            );
        }
    }

    #[tokio::test]
    async fn the_cap_is_at_least_one() {
        let transport = AsyncStubTransport::new_ok();
        let subscriber = batched(transport.clone(), 0);
        subscriber.notify(&release(1)).await.unwrap();
        subscriber.notify(&release(2)).await.unwrap();
        subscriber.flush().await.unwrap();
        assert_eq!(transport.messages().await.len(), 2);
    }

    #[tokio::test]
    async fn without_batching_events_are_sent_as_they_arrive_and_flush_does_nothing() {
        let transport = AsyncStubTransport::new_ok();
        let subscriber = EmailSubscriber::with_transport(
            transport.clone(),
            "delve@example.test",
            &recipients(&["ops@example.test"]),
        )
        .unwrap();

        subscriber.notify(&release(1)).await.unwrap();
        subscriber.notify(&release(2)).await.unwrap();
        assert_eq!(transport.messages().await.len(), 2);
        subscriber.flush().await.unwrap();
        assert_eq!(transport.messages().await.len(), 2);
    }

    fn other_device(mut event: FirmwareEvent, vendor: &str, hardware: &str) -> FirmwareEvent {
        if let FirmwareEvent::NewRelease { firmware, .. } = &mut event {
            firmware.vendor = vendor.into();
            firmware.hardware_targets = vec![hardware.into()];
        }
        event
    }

    #[test]
    fn the_subject_says_how_many_devices_when_there_are_several() {
        let one_vendor =
            digest::sorted(vec![release(1), other_device(release(2), "acme", "rev-z")]);
        assert_eq!(
            digest::render_digest(&one_vendor, None).0,
            "[delve] 2 firmware changes: acme (2 devices)"
        );

        let two_vendors =
            digest::sorted(vec![release(1), other_device(release(2), "globex", "r1")]);
        assert_eq!(
            digest::render_digest(&two_vendors, None).0,
            "[delve] 2 firmware changes: 2 devices"
        );
    }

    #[test]
    fn a_digest_groups_by_device_with_a_heading_each() {
        let mut events = vec![
            other_device(release(1), "globex", "r1"),
            release(2),
            release(3),
        ];
        // The heading carries the product name when there is one.
        for event in &mut events {
            if let FirmwareEvent::NewRelease { firmware, .. } = event {
                if firmware.vendor == "acme" {
                    firmware.display_name = Some("Switch Flex Mini".into());
                }
            }
        }
        let events = digest::sorted(events);
        let (_, body) = digest::render_digest(&events, None);
        let acme = body
            .find("acme widget (rev-a+rev-b) - Switch Flex Mini")
            .unwrap();
        let globex = body.find("globex widget (r1)").unwrap();
        assert!(acme < globex, "devices are in order: {body}");
        // Two acme entries under one heading, newest first.
        assert_eq!(body.matches("acme widget").count(), 1, "{body}");
        assert!(
            body.find("1.3").unwrap() < body.find("1.2").unwrap(),
            "{body}"
        );
    }

    #[test]
    fn a_digest_entry_says_what_kind_of_change_it_is() {
        let events = digest::sorted(vec![
            release(1),
            updated("1.0", "1.1", VersionDirection::Newer),
            updated("1.1", "1.0", VersionDirection::Older),
            updated("r1", "r2", VersionDirection::Unordered),
            updated("1.0", "1.0", VersionDirection::Unordered),
        ]);
        let (_, body) = digest::render_digest(&events, None);
        for expected in [
            "  new        1.1\n",
            "  updated    1.0 -> 1.1\n",
            "  DOWNGRADED 1.1 -> 1.0\n",
            "  changed    r1 -> r2\n",
            "  rebuilt    1.0\n",
        ] {
            assert!(body.contains(expected), "missing {expected:?}: {body}");
        }
        // The release date, a short hash (and the old one for a change), the
        // release notes and the source are with the entry.
        assert!(body.contains("released 2026-09-01, sha256 "), "{body}");
        // The fixture's changed `sha256` field is "aa" -> "bb".
        assert!(body.contains("(was aa)"), "{body}");
        assert!(
            body.contains("notes https://acme.example.test/notes"),
            "{body}"
        );
        assert!(
            body.contains("https://acme.example.test/widget/fw"),
            "{body}"
        );
    }

    #[test]
    fn vendor_data_cannot_add_lines_to_a_digest_subject() {
        let mut bad = release(1);
        if let FirmwareEvent::NewRelease { firmware, .. } = &mut bad {
            firmware.device_family = "widget\r\nBcc: attacker@example.test".into();
        }
        let events = digest::sorted(vec![bad, release(2)]);
        let (subject, _) = digest::render_digest(&events, None);
        assert!(
            !subject.contains('\n') && !subject.contains('\r'),
            "{subject:?}"
        );
    }

    /// A transport that fails its second send and keeps the others.
    struct FlakyTransport {
        calls: std::sync::atomic::AtomicUsize,
        sent: Mutex<Vec<String>>,
    }

    #[derive(Debug)]
    struct Refused;

    impl std::fmt::Display for Refused {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "server refused the message")
        }
    }

    #[async_trait]
    impl AsyncTransport for FlakyTransport {
        type Ok = ();
        type Error = Refused;

        async fn send_raw(
            &self,
            _envelope: &lettre::address::Envelope,
            email: &[u8],
        ) -> Result<(), Refused> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call == 1 {
                return Err(Refused);
            }
            self.sent
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(email).into_owned());
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_failed_message_does_not_stop_the_rest_and_is_reported() {
        let subscriber = EmailSubscriber::with_transport(
            FlakyTransport {
                calls: std::sync::atomic::AtomicUsize::new(0),
                sent: Mutex::new(Vec::new()),
            },
            "delve@example.test",
            &recipients(&["ops@example.test"]),
        )
        .unwrap()
        .batched(2);
        for n in 1..=6 {
            subscriber.notify(&release(n)).await.unwrap();
        }

        let err = subscriber.flush().await.unwrap_err();

        assert!(err.to_string().contains("1 of 3 messages failed"), "{err}");
        assert_eq!(
            subscriber.transport.sent.lock().unwrap().len(),
            2,
            "the other two went out"
        );
    }

    #[tokio::test]
    async fn a_failing_transport_is_a_delivery_error_when_batching_too() {
        let subscriber = EmailSubscriber::with_transport(
            AsyncStubTransport::new_error(),
            "delve@example.test",
            &recipients(&["ops@example.test"]),
        )
        .unwrap()
        .batched(50);
        subscriber.notify(&release(1)).await.unwrap();
        subscriber.notify(&release(2)).await.unwrap();

        let err = subscriber.flush().await.unwrap_err();
        assert!(matches!(err, SubscriberError::Delivery(_)), "{err}");
    }
}
