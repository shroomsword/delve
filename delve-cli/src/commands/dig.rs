//! `delve dig` — see the README's "CLI commands" section and the engine
//! loop described in "Baseline vs incremental digs".

use delve_core::context::context_for_vendor;
use delve_core::events::EventBus;
use delve_core::plugin::PluginRegistry;
use delve_core::store::MetadataStore;

use crate::config::Config;

pub async fn run(
    config: &Config,
    registry: &PluginRegistry,
    store: &dyn MetadataStore,
    vendor_filter: Option<String>,
    redig: bool,
    allow_unreviewed: bool,
) -> anyhow::Result<()> {
    // Always-on log subscriber (see the README's "Notifications" section)
    // plus whatever's configured. Webhook is
    // feature-gated in Cargo.toml and only constructed if the feature is
    // compiled in and configured — kept simple here as a scaffold; a real
    // build would branch on the `subscriber-webhook` feature flag.
    // `mut` is only exercised when the `subscriber-webhook` feature is
    // compiled in (see the cfg push below) — silence the warning on
    // builds without that feature rather than split this into two paths.
    #[allow(unused_mut)]
    let mut subscribers: Vec<Box<dyn delve_core::events::Subscriber>> = Vec::new();
    if let Some(webhook_cfg) = &config.subscribers.webhook {
        #[cfg(feature = "subscriber-webhook")]
        subscribers.push(Box::new(subscriber_webhook::WebhookSubscriber::new(
            webhook_cfg.url.clone(),
        )));
        #[cfg(not(feature = "subscriber-webhook"))]
        {
            let _ = webhook_cfg;
            tracing::warn!("webhook subscriber configured but the subscriber-webhook feature isn't compiled in");
        }
    }
    if let Some(email_cfg) = &config.subscribers.email {
        #[cfg(feature = "subscriber-email")]
        subscribers.push(email_subscriber(email_cfg)?);
        #[cfg(not(feature = "subscriber-email"))]
        {
            let _ = email_cfg;
            tracing::warn!(
                "email subscriber configured but the subscriber-email feature isn't compiled in"
            );
        }
    }
    let bus = EventBus::new(subscribers).with_change_policy(config.notifications.change_policy()?);

    dig_vendors(
        config,
        registry,
        store,
        &bus,
        vendor_filter,
        redig,
        allow_unreviewed,
    )
    .await
}

/// Builds the email subscriber from `[subscribers.email]`. Fails the dig up
/// front on a bad address or an unset `env:` variable, rather than running
/// without the notifications it was asked for.
#[cfg(feature = "subscriber-email")]
pub(crate) fn email_subscriber(
    cfg: &crate::config::EmailConfig,
) -> anyhow::Result<Box<dyn delve_core::events::Subscriber>> {
    use crate::config::EmailTls;
    use subscriber_email::{EmailSettings, EmailSubscriber, TlsMode};

    let settings = EmailSettings {
        host: cfg.host.clone(),
        port: cfg.port,
        tls: match cfg.tls {
            EmailTls::StartTls => TlsMode::StartTls,
            EmailTls::Implicit => TlsMode::Implicit,
            EmailTls::None => TlsMode::None,
        },
        from: cfg.from.clone(),
        to: cfg.to.clone(),
        credentials: cfg.resolve_credentials()?,
        batch: cfg.batch,
        max_events_per_message: cfg.max_events()?,
    };
    Ok(Box::new(EmailSubscriber::smtp(&settings)?))
}

/// Everything `run` does after building the event bus — split out so tests
/// can drive a real dig with their own subscribers.
///
/// Every vendor's events are delivered to the subscribers together, once,
/// after the last vendor, however the dig ended: a vendor that fails, or a
/// setup error partway through, doesn't lose what the others found.
pub async fn dig_vendors(
    config: &Config,
    registry: &PluginRegistry,
    store: &dyn MetadataStore,
    bus: &EventBus,
    vendor_filter: Option<String>,
    redig: bool,
    allow_unreviewed: bool,
) -> anyhow::Result<()> {
    let result = dig_each_vendor(
        config,
        registry,
        store,
        bus,
        vendor_filter,
        redig,
        allow_unreviewed,
    )
    .await;
    bus.deliver().await;
    result
}

async fn dig_each_vendor(
    config: &Config,
    registry: &PluginRegistry,
    store: &dyn MetadataStore,
    bus: &EventBus,
    vendor_filter: Option<String>,
    redig: bool,
    allow_unreviewed: bool,
) -> anyhow::Result<()> {
    // clap already enforces this, but the override is too dangerous to rely
    // on a single layer: it must never apply to an unfiltered dig.
    if allow_unreviewed && vendor_filter.is_none() {
        anyhow::bail!("--allow-unreviewed requires --vendor");
    }

    let (default_transport, transport_overrides) = config.transport.resolve()?;
    let http_config = config.transport.http_client_config();
    let (default_rate_limit, rate_limit_overrides) = config.transport.rate_limits();

    let vendor_ids: Vec<&str> = match &vendor_filter {
        // The registered id, so `--vendor ACME` digs `acme` and everything
        // below (baselines, settings, credentials, events) uses that id. An
        // unknown name falls through as typed and is reported as typed.
        Some(v) => vec![registry.resolve_id(v).unwrap_or(v.as_str())],
        // No explicit --vendor: honor [vendors].enabled from config if the
        // user set it (a narrowing allowlist over whatever vendors were
        // compiled in via Cargo features); an empty list means no
        // narrowing was configured, so dig everything that's compiled in.
        None if !config.vendors.enabled.is_empty() => registry
            .vendor_ids()
            .filter(|id| config.vendors.enabled.iter().any(|e| e == id))
            .collect(),
        None => registry.vendor_ids().collect(),
    };

    if vendor_ids.is_empty() {
        anyhow::bail!("no vendors enabled — check Cargo features and [vendors].enabled in config");
    }

    // A failed vendor doesn't stop the others, but it does fail the command
    // once they've all run, so a scheduler (cron, systemd) sees a non-zero
    // exit status instead of a failure that only shows up in the logs.
    let mut failed = Vec::new();
    for vendor_id in vendor_ids {
        let Some(plugin) = registry.get(vendor_id) else {
            anyhow::bail!("unknown vendor: {vendor_id}");
        };

        if !plugin.capabilities().tos_reviewed {
            if allow_unreviewed {
                tracing::warn!(
                    vendor = vendor_id,
                    "RUNNING UNREVIEWED VENDOR: ToS/robots.txt has not been reviewed; \
                     --allow-unreviewed is a dev escape hatch, not for scheduled runs"
                );
            } else {
                tracing::warn!(
                    vendor = vendor_id,
                    "skipping: ToS/robots.txt not yet reviewed"
                );
                continue;
            }
        }

        if redig {
            store.clear_baseline(vendor_id).await?;
        }

        let credentials = config.vendors.resolve_credentials(vendor_id)?;
        let ctx = context_for_vendor(
            &default_transport,
            &transport_overrides,
            &default_rate_limit,
            &rate_limit_overrides,
            vendor_id,
            credentials,
            config.vendors.settings_for(vendor_id),
            &http_config,
        )?;

        tracing::info!(vendor = vendor_id, "starting dig");
        if let Err(e) = delve_core::engine::dig_vendor(plugin, &ctx, store, bus).await {
            tracing::error!(vendor = vendor_id, error = %e, "dig failed");
            failed.push(vendor_id);
        }
    }

    if !failed.is_empty() {
        failed.sort_unstable();
        anyhow::bail!("dig failed for: {}", failed.join(", "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_support::*;
    use delve_core::prelude::*;

    fn registry(plugins: Vec<MockPlugin>) -> PluginRegistry {
        PluginRegistry::from_plugins(
            plugins
                .into_iter()
                .map(|p| Box::new(p) as Box<dyn VendorPlugin>)
                .collect(),
        )
    }

    async fn dig(
        registry: &PluginRegistry,
        store: &dyn MetadataStore,
        subscriber: &RecordingSubscriber,
        vendor: Option<&str>,
        redig: bool,
    ) -> anyhow::Result<()> {
        dig_vendors(
            &config(""),
            registry,
            store,
            &subscriber.bus(),
            vendor.map(String::from),
            redig,
            false,
        )
        .await
    }

    #[cfg(feature = "subscriber-email")]
    #[test]
    fn the_email_subscriber_builds_from_config_and_fails_early_when_misconfigured() {
        let build = |toml: &str| {
            let config = config(toml);
            email_subscriber(config.subscribers.email.as_ref().expect("email section"))
                .map(|s| s.id())
        };
        let section = |extra: &str| {
            format!(
                "[subscribers.email]\nhost = \"smtp.example.test\"\nfrom = \"delve@example.test\"\n\
                 to = [\"ops@example.test\"]\n{extra}"
            )
        };

        assert_eq!(build(&section("")).unwrap(), "email");
        assert_eq!(
            build(&section(
                "tls = \"implicit\"\nusername = \"u\"\npassword = \"p\""
            ))
            .unwrap(),
            "email"
        );

        let missing_env = build(&section(
            "username = \"u\"\npassword = \"env:DELVE_TEST_DIG_SMTP_UNSET\"",
        ))
        .unwrap_err();
        assert!(
            missing_env
                .to_string()
                .contains("DELVE_TEST_DIG_SMTP_UNSET"),
            "{missing_env}"
        );

        let zero_cap = build(&section("max_events_per_email = 0\n")).unwrap_err();
        assert!(zero_cap.to_string().contains("at least 1"), "{zero_cap}");

        let bad_to =
            build("[subscribers.email]\nhost = \"h\"\nfrom = \"a@b.test\"\nto = [\"nope\"]\n")
                .unwrap_err();
        assert!(bad_to.to_string().contains("to address 'nope'"), "{bad_to}");
    }

    /// Digs a mock vendor twice, the second time with a version bump and a new
    /// line (two events), mailing through a fake SMTP server configured with
    /// `extra` under `[subscribers.email]`. Returns what the server saw.
    #[cfg(feature = "subscriber-email")]
    async fn dig_with_two_events_by_email(extra: &str) -> String {
        let (addr, received) = fake_smtp_server();
        let config = config(&format!(
            "[subscribers.email]\nhost = \"127.0.0.1\"\nport = {}\ntls = \"none\"\n\
             from = \"Delve <delve@example.test>\"\nto = [\"ops@example.test\"]\n{extra}",
            addr.port()
        ));
        let store = memory_store().await;
        let plugin = MockPlugin::new("acme", vec![release("widget", "1.0", &[1, 0], 1)]);
        let releases = plugin.releases.clone();
        let registry = registry(vec![plugin]);
        let dig_once = || run(&config, &registry, &store, None, false, false);

        // The baseline dig is silent, so no mail.
        dig_once().await.unwrap();
        assert!(
            received.lock().unwrap().is_empty(),
            "a baseline must not send mail"
        );

        // A version bump on a known line and a brand-new line: two events.
        releases.lock().unwrap().extend([
            release("widget", "1.1", &[1, 1], 2),
            release("gadget", "0.1", &[0, 1], 3),
        ]);
        dig_once().await.unwrap();

        let log = received.lock().unwrap().join("\n");
        log
    }

    #[cfg(feature = "subscriber-email")]
    #[tokio::test]
    async fn a_dig_with_several_events_sends_one_digest_email() {
        let log = dig_with_two_events_by_email("").await;

        assert_eq!(
            log.matches("MAIL FROM:<delve@example.test>").count(),
            1,
            "two events in one dig must be one message: {log}"
        );
        assert!(
            log.contains("Subject: [delve] 2 firmware changes: acme (2 devices)"),
            "{log}"
        );
        // Both changes are in that one message.
        assert!(log.contains("1.0 -> 1.1"), "{log}");
        assert!(log.contains("acme gadget (rev-a)"), "{log}");
        assert!(log.contains("0.1"), "{log}");
    }

    #[cfg(feature = "subscriber-email")]
    #[tokio::test]
    async fn with_batching_off_each_event_is_its_own_email() {
        let log = dig_with_two_events_by_email("batch = false\n").await;

        assert_eq!(
            log.matches("MAIL FROM:<delve@example.test>").count(),
            2,
            "{log}"
        );
        assert_eq!(
            log.matches("RCPT TO:<ops@example.test>").count(),
            2,
            "{log}"
        );
        assert!(
            log.contains("Subject: [delve] Firmware updated: acme widget (rev-a) 1.0 -> 1.1"),
            "{log}"
        );
        assert!(
            log.contains("Subject: [delve] New firmware: acme gadget (rev-a) 0.1"),
            "{log}"
        );
    }

    #[cfg(feature = "subscriber-email")]
    #[tokio::test]
    async fn the_cap_splits_a_dig_into_numbered_emails() {
        let log = dig_with_two_events_by_email("max_events_per_email = 1\n").await;

        assert_eq!(
            log.matches("MAIL FROM:<delve@example.test>").count(),
            2,
            "{log}"
        );
        assert!(log.contains("(part 1 of 2)"), "{log}");
        assert!(log.contains("(part 2 of 2)"), "{log}");
    }

    #[tokio::test]
    async fn first_dig_is_a_silent_baseline_then_later_digs_notify() {
        let store = memory_store().await;
        let plugin = MockPlugin::new("acme", vec![release("widget", "1.0", &[1, 0], 1)]);
        let releases = plugin.releases.clone();
        let registry = registry(vec![plugin]);
        let sub = RecordingSubscriber::default();

        dig(&registry, &store, &sub, None, false).await.unwrap();
        assert!(sub.take().is_empty(), "a baseline dig must not notify");
        assert!(store.has_completed_baseline("acme").await.unwrap());
        assert_eq!(store.all_current("acme").await.unwrap().len(), 1);

        // Nothing changed: an incremental dig with no news is silent too.
        dig(&registry, &store, &sub, None, false).await.unwrap();
        assert!(sub.take().is_empty());

        // A version bump on a known line, and a brand-new line.
        releases.lock().unwrap().extend([
            release("widget", "1.1", &[1, 1], 2),
            release("gadget", "0.1", &[0, 1], 3),
        ]);
        dig(&registry, &store, &sub, None, false).await.unwrap();

        let mut events = sub.take();
        events.sort_by_key(|e| match e {
            FirmwareEvent::NewRelease { firmware, .. } => firmware.device_family.clone(),
            FirmwareEvent::UpdatedRelease { firmware, .. } => firmware.device_family.clone(),
        });
        assert_eq!(events.len(), 2);
        match &events[0] {
            FirmwareEvent::NewRelease { firmware, .. } => {
                assert_eq!(firmware.device_family, "gadget")
            }
            other => panic!("expected NewRelease for gadget, got {other:?}"),
        }
        match &events[1] {
            FirmwareEvent::UpdatedRelease {
                firmware,
                previous,
                version_direction,
                ..
            } => {
                assert_eq!(previous.version.raw, "1.0");
                assert_eq!(firmware.version.raw, "1.1");
                assert_eq!(*version_direction, VersionDirection::Newer);
            }
            other => panic!("expected UpdatedRelease for widget, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rebuilt_release_under_the_same_version_notifies() {
        let store = memory_store().await;
        let plugin = MockPlugin::new("acme", vec![release("widget", "1.0", &[1, 0], 1)]);
        let releases = plugin.releases.clone();
        let registry = registry(vec![plugin]);
        let sub = RecordingSubscriber::default();
        dig(&registry, &store, &sub, None, false).await.unwrap();

        releases.lock().unwrap()[0].sha = 9;
        dig(&registry, &store, &sub, None, false).await.unwrap();

        let events = sub.take();
        assert_eq!(events.len(), 1);
        match &events[0] {
            FirmwareEvent::UpdatedRelease {
                changed_fields,
                version_direction,
                ..
            } => {
                assert!(changed_fields.iter().any(|d| d.field == "sha256"));
                assert_eq!(*version_direction, VersionDirection::Unordered);
            }
            other => panic!("expected UpdatedRelease, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn redig_stores_new_releases_without_notifying() {
        let store = memory_store().await;
        let plugin = MockPlugin::new("acme", vec![release("widget", "1.0", &[1, 0], 1)]);
        let releases = plugin.releases.clone();
        let registry = registry(vec![plugin]);
        let sub = RecordingSubscriber::default();
        dig(&registry, &store, &sub, None, false).await.unwrap();

        releases
            .lock()
            .unwrap()
            .push(release("widget", "2.0", &[2, 0], 2));
        dig(&registry, &store, &sub, Some("acme"), true)
            .await
            .unwrap();

        assert!(sub.take().is_empty(), "--redig must behave like a baseline");
        assert_eq!(store.all_current("acme").await.unwrap().len(), 2);
        assert!(store.has_completed_baseline("acme").await.unwrap());
    }

    #[tokio::test]
    async fn vendors_without_tos_review_are_skipped() {
        let store = memory_store().await;
        let mut unreviewed = MockPlugin::new("acme", vec![release("widget", "1.0", &[1, 0], 1)]);
        unreviewed.tos_reviewed = false;
        let registry = registry(vec![unreviewed]);
        let sub = RecordingSubscriber::default();

        // Skipped both when digging everything and when named explicitly.
        dig(&registry, &store, &sub, None, false).await.unwrap();
        dig(&registry, &store, &sub, Some("acme"), false)
            .await
            .unwrap();

        assert!(store.all_current("acme").await.unwrap().is_empty());
        assert!(!store.has_completed_baseline("acme").await.unwrap());
    }

    #[tokio::test]
    async fn allow_unreviewed_runs_a_named_unreviewed_vendor() {
        let store = memory_store().await;
        let mut unreviewed = MockPlugin::new("acme", vec![release("widget", "1.0", &[1, 0], 1)]);
        unreviewed.tos_reviewed = false;
        let registry = registry(vec![unreviewed]);
        let sub = RecordingSubscriber::default();

        dig_vendors(
            &config(""),
            &registry,
            &store,
            &sub.bus(),
            Some("acme".into()),
            false,
            true,
        )
        .await
        .unwrap();

        assert_eq!(store.all_current("acme").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn allow_unreviewed_without_a_vendor_is_an_error_and_digs_nothing() {
        let store = memory_store().await;
        let mut unreviewed = MockPlugin::new("acme", vec![release("widget", "1.0", &[1, 0], 1)]);
        unreviewed.tos_reviewed = false;
        let registry = registry(vec![
            unreviewed,
            MockPlugin::new("globex", vec![release("sprocket", "1.0", &[1, 0], 2)]),
        ]);
        let sub = RecordingSubscriber::default();

        let err = dig_vendors(
            &config(""),
            &registry,
            &store,
            &sub.bus(),
            None,
            false,
            true,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("requires --vendor"), "{err}");
        assert!(store.all_current("acme").await.unwrap().is_empty());
        assert!(store.all_current("globex").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn vendor_filter_and_enabled_list_narrow_which_vendors_dig() {
        let registry = registry(vec![
            MockPlugin::new("acme", vec![release("widget", "1.0", &[1, 0], 1)]),
            MockPlugin::new("globex", vec![release("sprocket", "1.0", &[1, 0], 2)]),
        ]);
        let sub = RecordingSubscriber::default();

        let store = memory_store().await;
        dig(&registry, &store, &sub, Some("globex"), false)
            .await
            .unwrap();
        assert!(store.all_current("acme").await.unwrap().is_empty());
        assert_eq!(store.all_current("globex").await.unwrap().len(), 1);

        let store = memory_store().await;
        let narrowed = config("[vendors]\nenabled = [\"acme\"]");
        dig_vendors(&narrowed, &registry, &store, &sub.bus(), None, false, false)
            .await
            .unwrap();
        assert_eq!(store.all_current("acme").await.unwrap().len(), 1);
        assert!(store.all_current("globex").await.unwrap().is_empty());

        let store = memory_store().await;
        dig(&registry, &store, &sub, None, false).await.unwrap();
        assert_eq!(store.all_current("acme").await.unwrap().len(), 1);
        assert_eq!(store.all_current("globex").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn vendor_is_matched_ignoring_case_and_the_registered_id_is_used() {
        let store = memory_store().await;
        let plugin = MockPlugin::new("acme", vec![release("widget", "1.0", &[1, 0], 1)]);
        let releases = plugin.releases.clone();
        let registry = registry(vec![plugin, MockPlugin::new("globex", vec![])]);
        let sub = RecordingSubscriber::default();

        assert_eq!(registry.resolve_id("ACME"), Some("acme"));
        assert_eq!(registry.resolve_id("Acme"), Some("acme"));
        assert_eq!(registry.resolve_id("acme"), Some("acme"));
        assert_eq!(registry.resolve_id("acm"), None);

        dig(&registry, &store, &sub, Some("ACME"), false)
            .await
            .unwrap();
        // Everything is stored and tracked under the registered id, not the
        // spelling that was typed.
        assert_eq!(store.all_current("acme").await.unwrap().len(), 1);
        assert!(store.all_current("ACME").await.unwrap().is_empty());
        assert!(store.has_completed_baseline("acme").await.unwrap());
        assert!(!store.has_completed_baseline("ACME").await.unwrap());

        // `--redig` clears the registered id's baseline too.
        releases
            .lock()
            .unwrap()
            .push(release("widget", "2.0", &[2, 0], 2));
        dig(&registry, &store, &sub, Some("AcMe"), true)
            .await
            .unwrap();
        assert!(sub.take().is_empty(), "--redig must behave like a baseline");
        assert_eq!(store.all_current("acme").await.unwrap().len(), 2);
        assert!(store.has_completed_baseline("acme").await.unwrap());
    }

    #[tokio::test]
    async fn an_unknown_vendor_is_reported_as_it_was_typed() {
        let store = memory_store().await;
        let registry = registry(vec![MockPlugin::new("acme", vec![])]);
        let err = dig(
            &registry,
            &store,
            &RecordingSubscriber::default(),
            Some("NoPe"),
            false,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("unknown vendor: NoPe"), "{err}");
    }

    #[tokio::test]
    async fn unknown_vendor_and_no_vendors_are_errors() {
        let store = memory_store().await;
        let sub = RecordingSubscriber::default();

        let err = dig(&registry(vec![]), &store, &sub, None, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no vendors enabled"), "{err}");

        let registry = registry(vec![MockPlugin::new("acme", vec![])]);
        let err = dig(&registry, &store, &sub, Some("nope"), false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown vendor: nope"), "{err}");
    }
    /// Digs `registry` twice: a silent baseline, then again after
    /// `add` has given each vendor something new.
    async fn baseline_then_dig(
        registry: &PluginRegistry,
        store: &dyn MetadataStore,
        sub: &RecordingSubscriber,
        add: impl FnOnce(),
    ) -> anyhow::Result<()> {
        dig(registry, store, sub, None, false).await.unwrap();
        add();
        dig(registry, store, sub, None, false).await
    }

    #[tokio::test]
    async fn every_vendor_s_events_are_delivered_together_once_per_dig() {
        let store = memory_store().await;
        let acme = MockPlugin::new("acme", vec![release("widget", "1.0", &[1, 0], 1)]);
        let globex = MockPlugin::new("globex", vec![release("sprocket", "1.0", &[1, 0], 2)]);
        let (acme_releases, globex_releases) = (acme.releases.clone(), globex.releases.clone());
        let registry = registry(vec![acme, globex]);
        let sub = RecordingSubscriber::default();

        baseline_then_dig(&registry, &store, &sub, || {
            acme_releases
                .lock()
                .unwrap()
                .push(release("widget", "1.1", &[1, 1], 3));
            globex_releases
                .lock()
                .unwrap()
                .push(release("sprocket", "1.1", &[1, 1], 4));
        })
        .await
        .unwrap();

        // The baseline delivered nothing; the second dig made one delivery
        // holding both vendors' events.
        assert_eq!(sub.deliveries.load(std::sync::atomic::Ordering::SeqCst), 1);
        let mut vendors: Vec<String> = sub
            .take()
            .iter()
            .map(|e| match e {
                FirmwareEvent::NewRelease { firmware, .. }
                | FirmwareEvent::UpdatedRelease { firmware, .. } => firmware.vendor.clone(),
            })
            .collect();
        vendors.sort();
        assert_eq!(vendors, ["acme", "globex"]);
    }

    #[tokio::test]
    async fn what_other_vendors_found_is_delivered_when_one_fails() {
        let store = memory_store().await;
        let globex = MockPlugin::new("globex", vec![release("sprocket", "1.0", &[1, 0], 2)]);
        let globex_releases = globex.releases.clone();
        let sub = RecordingSubscriber::default();
        // A baseline for both vendors.
        let healthy = registry(vec![
            MockPlugin::new("acme", vec![release("widget", "1.0", &[1, 0], 1)]),
            globex,
        ]);
        dig(&healthy, &store, &sub, None, false).await.unwrap();

        // Then acme's portal goes down while globex publishes a release.
        let mut broken = MockPlugin::new("acme", vec![]);
        broken.fail_discover = Some("portal is down");
        globex_releases
            .lock()
            .unwrap()
            .push(release("sprocket", "1.1", &[1, 1], 3));
        let globex = MockPlugin {
            releases: globex_releases,
            ..MockPlugin::new("globex", vec![])
        };
        let failing = registry(vec![broken, globex]);

        let err = dig(&failing, &store, &sub, None, false).await.unwrap_err();
        assert_eq!(err.to_string(), "dig failed for: acme");
        assert_eq!(sub.deliveries.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(sub.take().len(), 1, "globex's release is still delivered");
    }

    #[cfg(feature = "subscriber-email")]
    #[tokio::test]
    async fn two_vendors_with_changes_are_one_email() {
        let (addr, received) = fake_smtp_server();
        let config = config(&format!(
            "[subscribers.email]\nhost = \"127.0.0.1\"\nport = {}\ntls = \"none\"\n\
             from = \"Delve <delve@example.test>\"\nto = [\"ops@example.test\"]\n",
            addr.port()
        ));
        let store = memory_store().await;
        let acme = MockPlugin::new("acme", vec![release("widget", "1.0", &[1, 0], 1)]);
        let globex = MockPlugin::new("globex", vec![release("sprocket", "1.0", &[1, 0], 2)]);
        let (acme_releases, globex_releases) = (acme.releases.clone(), globex.releases.clone());
        let registry = registry(vec![acme, globex]);

        run(&config, &registry, &store, None, false, false)
            .await
            .unwrap();
        acme_releases
            .lock()
            .unwrap()
            .push(release("widget", "1.1", &[1, 1], 3));
        globex_releases
            .lock()
            .unwrap()
            .push(release("sprocket", "1.1", &[1, 1], 4));
        run(&config, &registry, &store, None, false, false)
            .await
            .unwrap();

        let log = received.lock().unwrap().join("\n");
        assert_eq!(
            log.matches("MAIL FROM:<delve@example.test>").count(),
            1,
            "two vendors' changes in one dig must be one message: {log}"
        );
        assert!(log.contains("2 firmware changes"), "{log}");
        assert!(log.contains("acme widget"), "{log}");
        assert!(log.contains("globex sprocket"), "{log}");
    }

    #[tokio::test]
    async fn a_failed_vendor_fails_the_dig_but_other_vendors_still_run() {
        let store = memory_store().await;
        let mut broken = MockPlugin::new("acme", vec![]);
        broken.fail_discover = Some("portal is down");
        let registry = registry(vec![
            broken,
            MockPlugin::new("globex", vec![release("sprocket", "1.0", &[1, 0], 2)]),
        ]);
        let sub = RecordingSubscriber::default();

        let err = dig(&registry, &store, &sub, None, false)
            .await
            .expect_err("a vendor failing must fail the dig, so cron/systemd can see it");
        assert_eq!(err.to_string(), "dig failed for: acme");

        assert_eq!(store.all_current("globex").await.unwrap().len(), 1);
        assert!(!store.has_completed_baseline("acme").await.unwrap());
    }
}
