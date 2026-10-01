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
    let mut subscribers: Vec<Box<dyn delve_core::events::Subscriber>> =
        vec![Box::new(subscriber_log::LogSubscriber)];
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
    let bus = EventBus::new(subscribers);

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

/// Everything `run` does after building the event bus — split out so tests
/// can drive a real dig with their own subscribers.
pub async fn dig_vendors(
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
        Some(v) => vec![v.as_str()],
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
                     --allow-unreviewed is a dev escape hatch, not for scheduled runs \
                     (see the README's \"Compliance\" section)"
                );
            } else {
                tracing::warn!(
                    vendor = vendor_id,
                    "skipping: ToS/robots.txt not yet reviewed (see the README's \"Compliance\" section)"
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
