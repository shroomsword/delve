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
) -> anyhow::Result<()> {
    let (default_transport, transport_overrides) = config.transport.resolve()?;
    let http_config = config.transport.http_client_config();
    let (default_rate_limit, rate_limit_overrides) = config.transport.rate_limits();

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
        subscribers.push(Box::new(subscriber_webhook::WebhookSubscriber::new(webhook_cfg.url.clone())));
        #[cfg(not(feature = "subscriber-webhook"))]
        {
            let _ = webhook_cfg;
            tracing::warn!("webhook subscriber configured but the subscriber-webhook feature isn't compiled in");
        }
    }
    let bus = EventBus::new(subscribers);

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

    for vendor_id in vendor_ids {
        let Some(plugin) = registry.get(vendor_id) else {
            anyhow::bail!("unknown vendor: {vendor_id}");
        };

        if !plugin.capabilities().tos_reviewed {
            tracing::warn!(
                vendor = vendor_id,
                "skipping: ToS/robots.txt not yet reviewed (see the README's \"Compliance\" section)"
            );
            continue;
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
            &http_config,
        )?;

        tracing::info!(vendor = vendor_id, "starting dig");
        if let Err(e) = delve_core::engine::dig_vendor(plugin, &ctx, store, &bus).await {
            tracing::error!(vendor = vendor_id, error = %e, "dig failed");
        }
    }

    Ok(())
}
