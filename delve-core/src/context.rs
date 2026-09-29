//! `ScrapeContext` and transport configuration. See the README's
//! "Transport and proxying" section for the full rationale.
//!
//! Proxying is handled entirely here so vendor plugins never construct
//! their own HTTP clients or think about Tor/SOCKS5 at all — they just call
//! `ctx.http_client()` and get whatever the run was configured to use.
//! Credentials and rate limiting work the same way: plugins read/call what
//! they need off `ctx` rather than each inventing their own config lookup
//! or throttling logic.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::plugin::PluginError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Transport {
    Direct,
    Socks5 { addr: SocketAddr },
    Tor(TorConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorConfig {
    pub mode: TorMode,
    pub circuit_isolation: CircuitIsolation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TorMode {
    /// Talk to a running system `tor` daemon's SOCKS port
    /// (127.0.0.1:9050 typically). The current approach — see the
    /// README's "Transport and proxying" section.
    ExternalDaemon { socks_addr: SocketAddr },
    /// Embed Tor in-process via `arti-client`. Not implemented in this
    /// scaffold; `arti-client` maturity is worth checking again before
    /// making it the default (see the README's "Transport and proxying"
    /// section).
    Embedded,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub enum CircuitIsolation {
    /// One circuit for the whole run.
    Shared,
    /// New circuit per vendor. Default — balances not looking like a single
    /// abusive client to any one vendor against exhausting the exit-node
    /// pool with `PerRequest`.
    #[default]
    PerVendor,
    /// New circuit per request — safest, slowest.
    PerRequest,
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("failed to build HTTP client: {0}")]
    ClientBuild(#[from] reqwest::Error),
}

/// The framework-wide default: `delve/<version>` with no contact info. This
/// is intentionally not a great User-Agent — it identifies the tool but
/// gives a vendor no way to reach out about it. Anyone actually running
/// this against a real vendor site should set `[transport].user_agent` in
/// config to something that includes contact info before doing so; that's
/// part of the same honesty-about-what-this-is discipline as the
/// `tos_reviewed` gate described in the README's "Compliance" section,
/// not just a technical nicety.
pub fn default_user_agent() -> String {
    concat!("delve/", env!("CARGO_PKG_VERSION")).to_string()
}

/// HTTP client behavior that isn't per-vendor — applied uniformly to every
/// `ScrapeContext` a run creates. Configurable via `[transport]` in
/// `delve-cli`'s config (`user_agent`, `request_timeout_secs`,
/// `connect_timeout_secs`).
#[derive(Debug, Clone)]
pub struct HttpClientConfig {
    pub user_agent: String,
    /// Caps how long a single request (including reading the response
    /// body) may take. Without this, a hung connection to a vendor site
    /// blocks that vendor's entire dig indefinitely — one unresponsive
    /// portal shouldn't be able to stall a scheduled run forever.
    pub request_timeout: Duration,
    /// Caps how long establishing the TCP/TLS connection itself may take,
    /// separately from the overall request timeout — relevant when a
    /// vendor's host is unreachable rather than merely slow to respond.
    pub connect_timeout: Duration,
}

impl Default for HttpClientConfig {
    fn default() -> Self {
        Self {
            user_agent: default_user_agent(),
            request_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
        }
    }
}

/// The minimum time between requests a vendor plugin should make within
/// one vendor's dig, enforced via `ScrapeContext::throttle()`. This is the
/// concrete enforcement behind the per-vendor robots.txt/ToS review in
/// the README's "Compliance" section: reviewing a vendor's crawl-delay tells you what rate is
/// acceptable; this is what actually keeps requests to it. Per-vendor,
/// since different vendors publish (or don't publish) different
/// crawl-delays — see `RateLimitOverrides`.
#[derive(Debug, Clone, Copy)]
pub struct RateLimit {
    pub min_interval: Duration,
}

impl Default for RateLimit {
    fn default() -> Self {
        // Conservative default: at most one request per second. A vendor
        // whose robots.txt specifies an actual crawl-delay should have
        // that reflected via a per-vendor override in config rather than
        // relying on this guess.
        Self {
            min_interval: Duration::from_secs(1),
        }
    }
}

/// Serializes and paces requests to `min_interval` apart. Internal to
/// `ScrapeContext` — plugins call `ScrapeContext::throttle()`, never this
/// directly. Uses `tokio::time::Instant` rather than `std::time::Instant`
/// specifically so it plays correctly with `tokio::time::pause()` in
/// tests — a real wall-clock `Instant` would ignore a paused test clock
/// entirely and force every rate-limiter test to actually sleep.
struct RateLimiter {
    min_interval: Duration,
    last_request: Mutex<Option<Instant>>,
}

impl RateLimiter {
    fn new(rate_limit: RateLimit) -> Self {
        Self {
            min_interval: rate_limit.min_interval,
            last_request: Mutex::new(None),
        }
    }

    /// Blocks until at least `min_interval` has passed since the previous
    /// `acquire` call completed (anywhere — this is shared across every
    /// concurrent caller holding the same `ScrapeContext`, which is the
    /// point: two requests fired concurrently by a plugin's `discover()`
    /// still get serialized to the configured rate, not just requests made
    /// one after another in sequence). The very first call never waits.
    async fn acquire(&self) {
        let mut last = self.last_request.lock().await;
        let now = Instant::now();
        if let Some(prev) = *last {
            let elapsed = now.saturating_duration_since(prev);
            if elapsed < self.min_interval {
                tokio::time::sleep(self.min_interval - elapsed).await;
            }
        }
        *last = Some(Instant::now());
    }
}

/// Per-vendor credential key/value pairs, resolved (any `env:` indirection
/// already substituted — see `delve-cli`'s `config.rs`) before a
/// `ScrapeContext` is built. Deliberately just string keys rather than a
/// typed struct: different vendors need different shapes (username +
/// password, a single API token, a client cert path), and the framework
/// has no way to know which in advance — each plugin documents and reads
/// whatever keys it needs.
pub type Credentials = HashMap<String, String>;

/// Shared state handed to every plugin call: HTTP client (already
/// configured for whatever transport this vendor/run uses, with a real
/// User-Agent and timeouts applied), resolved credentials for this vendor,
/// and a per-vendor rate limiter plugins are expected to call before each
/// outgoing request.
pub struct ScrapeContext {
    transport: Transport,
    client: reqwest::Client,
    credentials: Credentials,
    rate_limiter: RateLimiter,
}

impl ScrapeContext {
    pub fn new(
        transport: Transport,
        credentials: Credentials,
        http: HttpClientConfig,
        rate_limit: RateLimit,
    ) -> Result<Self, TransportError> {
        let client = Self::build_http_client(&transport, &http)?;
        Ok(Self {
            transport,
            client,
            credentials,
            rate_limiter: RateLimiter::new(rate_limit),
        })
    }

    pub fn http_client(&self) -> &reqwest::Client {
        &self.client
    }

    pub fn transport(&self) -> &Transport {
        &self.transport
    }

    /// Look up a credential by key, if this vendor has one configured.
    pub fn credential(&self, key: &str) -> Option<&str> {
        self.credentials.get(key).map(String::as_str)
    }

    /// Same as `credential`, but fails with a clear `PluginError` instead
    /// of returning `None` — the common case in a plugin's `discover`/
    /// `metadata`/`fetch`, where a missing credential means the call can't
    /// proceed at all and should say so plainly rather than fail deeper
    /// inside an HTTP 401 that doesn't explain itself.
    pub fn require_credential(&self, key: &str) -> Result<&str, PluginError> {
        self.credential(key).ok_or_else(|| {
            PluginError::Rejected(format!(
                "missing required credential '{key}' — set it under [vendors.credentials.<vendor>] in your config"
            ))
        })
    }

    /// Call this immediately before every outgoing HTTP request to a
    /// vendor's site — in `discover`, `metadata`, and `fetch` alike. The
    /// framework can't enforce this automatically (plugins hold the raw
    /// `reqwest::Client` and make requests however they need to), so it's
    /// on each plugin author to call it, the same way `require_credential`
    /// is opt-in rather than intercepted. Skipping it means skipping the
    /// rate limit entirely for that request.
    pub async fn throttle(&self) {
        self.rate_limiter.acquire().await;
    }

    fn build_http_client(
        transport: &Transport,
        http: &HttpClientConfig,
    ) -> Result<reqwest::Client, TransportError> {
        let builder = reqwest::Client::builder()
            .user_agent(http.user_agent.clone())
            .timeout(http.request_timeout)
            .connect_timeout(http.connect_timeout);
        let client = match transport {
            Transport::Direct => builder.build()?,
            Transport::Socks5 { addr } => {
                // socks5h, not socks5: resolve DNS over the proxy too, or
                // the hostname being scraped leaks via a local DNS query.
                let proxy = reqwest::Proxy::all(format!("socks5h://{addr}"))?;
                builder.proxy(proxy).build()?
            }
            Transport::Tor(cfg) => match &cfg.mode {
                TorMode::ExternalDaemon { socks_addr } => {
                    let proxy = reqwest::Proxy::all(format!("socks5h://{socks_addr}"))?;
                    builder.proxy(proxy).build()?
                }
                TorMode::Embedded => {
                    // arti-client integration not yet implemented — see the
                    // README's "Transport and proxying" section.
                    unimplemented!("embedded Tor via arti-client is not yet wired up")
                }
            },
        };
        Ok(client)
    }
}

/// Per-vendor transport override, keyed by `vendor_id` (the README's
/// "Configuration reference" section's `[transport.overrides]` table —
/// some vendor CDNs block Tor exits outright).
pub type TransportOverrides = std::collections::HashMap<String, Transport>;

/// Per-vendor rate-limit override, keyed by `vendor_id` — mirrors
/// `TransportOverrides`. A vendor whose robots.txt specifies a slower (or
/// faster) crawl-delay than the run's default gets its own entry here.
pub type RateLimitOverrides = std::collections::HashMap<String, RateLimit>;

/// Resolves the effective transport for a vendor: override if present,
/// otherwise the run's default.
pub fn resolve_transport(
    default: &Transport,
    overrides: &TransportOverrides,
    vendor_id: &str,
) -> Transport {
    overrides
        .get(vendor_id)
        .cloned()
        .unwrap_or_else(|| default.clone())
}

/// Resolves the effective rate limit for a vendor: override if present,
/// otherwise the run's default. Mirrors `resolve_transport`.
pub fn resolve_rate_limit(
    default: &RateLimit,
    overrides: &RateLimitOverrides,
    vendor_id: &str,
) -> RateLimit {
    overrides.get(vendor_id).copied().unwrap_or(*default)
}

/// Convenience for building one `ScrapeContext` per vendor per the
/// README's "Transport and proxying" per-vendor override model; the CLI's
/// `dig` command calls this once per
/// vendor before invoking `discover`/`metadata`. `http` is the same for
/// every vendor in a run — only `transport`/`rate_limit`/`credentials` vary
/// per vendor.
#[allow(clippy::too_many_arguments)]
pub fn context_for_vendor(
    default_transport: &Transport,
    transport_overrides: &TransportOverrides,
    default_rate_limit: &RateLimit,
    rate_limit_overrides: &RateLimitOverrides,
    vendor_id: &str,
    credentials: Credentials,
    http: &HttpClientConfig,
) -> Result<Arc<ScrapeContext>, TransportError> {
    let transport = resolve_transport(default_transport, transport_overrides, vendor_id);
    let rate_limit = resolve_rate_limit(default_rate_limit, rate_limit_overrides, vendor_id);
    Ok(Arc::new(ScrapeContext::new(
        transport,
        credentials,
        http.clone(),
        rate_limit,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with(credentials: Credentials) -> ScrapeContext {
        ScrapeContext::new(
            Transport::Direct,
            credentials,
            HttpClientConfig::default(),
            RateLimit::default(),
        )
        .expect("Direct transport never fails to build")
    }

    #[test]
    fn credential_returns_none_when_not_configured() {
        let ctx = ctx_with(Credentials::new());
        assert!(ctx.credential("username").is_none());
    }

    #[test]
    fn credential_returns_the_value_when_configured() {
        let ctx = ctx_with(Credentials::from([(
            "username".to_string(),
            "alice".to_string(),
        )]));
        assert_eq!(ctx.credential("username"), Some("alice"));
    }

    #[test]
    fn require_credential_errors_clearly_when_missing() {
        let ctx = ctx_with(Credentials::new());
        let err = ctx.require_credential("password").unwrap_err();
        match err {
            PluginError::Rejected(msg) => {
                assert!(
                    msg.contains("password"),
                    "error message should name the missing key"
                );
            }
            other => panic!("expected PluginError::Rejected, got {other:?}"),
        }
    }

    #[test]
    fn require_credential_succeeds_when_present() {
        let ctx = ctx_with(Credentials::from([(
            "token".to_string(),
            "abc123".to_string(),
        )]));
        assert_eq!(ctx.require_credential("token").unwrap(), "abc123");
    }

    #[test]
    fn default_user_agent_identifies_the_tool_and_its_version() {
        let ua = default_user_agent();
        assert!(ua.starts_with("delve/"), "got: {ua}");
    }

    #[test]
    fn default_http_client_config_has_finite_timeouts() {
        // The specific numbers aren't the point — the point is that
        // neither is zero/unset, since an unset timeout is what lets a
        // hung vendor connection block a dig forever.
        let cfg = HttpClientConfig::default();
        assert!(cfg.request_timeout > Duration::ZERO);
        assert!(cfg.connect_timeout > Duration::ZERO);
    }

    #[test]
    fn scrape_context_builds_successfully_with_a_custom_http_config() {
        let http = HttpClientConfig {
            user_agent: "test-agent/1.0 (contact: test@example.test)".to_string(),
            request_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(2),
        };
        let result = ScrapeContext::new(
            Transport::Direct,
            Credentials::new(),
            http,
            RateLimit::default(),
        );
        assert!(
            result.is_ok(),
            "a well-formed HttpClientConfig must not fail client construction"
        );
    }

    #[test]
    fn resolve_rate_limit_falls_back_to_default_when_no_override_exists() {
        let default = RateLimit {
            min_interval: Duration::from_millis(500),
        };
        let overrides = RateLimitOverrides::new();
        let resolved = resolve_rate_limit(&default, &overrides, "cisco");
        assert_eq!(resolved.min_interval, Duration::from_millis(500));
    }

    #[test]
    fn resolve_rate_limit_uses_the_per_vendor_override_when_present() {
        let default = RateLimit {
            min_interval: Duration::from_millis(500),
        };
        let overrides = RateLimitOverrides::from([(
            "cisco".to_string(),
            RateLimit {
                min_interval: Duration::from_secs(3),
            },
        )]);
        let resolved = resolve_rate_limit(&default, &overrides, "cisco");
        assert_eq!(resolved.min_interval, Duration::from_secs(3));

        // A different vendor with no override still gets the default.
        let unaffected = resolve_rate_limit(&default, &overrides, "netgear");
        assert_eq!(unaffected.min_interval, Duration::from_millis(500));
    }

    #[tokio::test(start_paused = true)]
    async fn throttle_does_not_delay_the_very_first_call() {
        let ctx = ScrapeContext::new(
            Transport::Direct,
            Credentials::new(),
            HttpClientConfig::default(),
            RateLimit {
                min_interval: Duration::from_millis(500),
            },
        )
        .unwrap();

        let start = Instant::now();
        ctx.throttle().await;
        assert_eq!(
            Instant::now(),
            start,
            "the very first throttle call must not wait at all"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn throttle_delays_an_immediate_second_call_by_the_configured_interval() {
        let ctx = ScrapeContext::new(
            Transport::Direct,
            Credentials::new(),
            HttpClientConfig::default(),
            RateLimit {
                min_interval: Duration::from_millis(500),
            },
        )
        .unwrap();

        ctx.throttle().await;
        let before_second = Instant::now();
        ctx.throttle().await;
        let elapsed = Instant::now().saturating_duration_since(before_second);
        assert!(
            elapsed >= Duration::from_millis(500),
            "second throttle call must wait out the remaining interval, only waited {elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn throttle_does_not_delay_once_enough_time_has_already_elapsed() {
        let ctx = ScrapeContext::new(
            Transport::Direct,
            Credentials::new(),
            HttpClientConfig::default(),
            RateLimit {
                min_interval: Duration::from_millis(500),
            },
        )
        .unwrap();

        ctx.throttle().await;
        tokio::time::advance(Duration::from_millis(600)).await;

        let before = Instant::now();
        ctx.throttle().await;
        assert_eq!(
            Instant::now(),
            before,
            "no additional wait needed once the interval has already elapsed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn throttle_serializes_concurrent_callers_to_the_configured_rate() {
        // The point of throttle() being shared (not per-caller) state: two
        // requests fired concurrently by a plugin's discover() must still
        // end up spaced apart, not both fire immediately just because they
        // were awaited from different tasks.
        let ctx = Arc::new(
            ScrapeContext::new(
                Transport::Direct,
                Credentials::new(),
                HttpClientConfig::default(),
                RateLimit {
                    min_interval: Duration::from_millis(500),
                },
            )
            .unwrap(),
        );

        let start = Instant::now();
        let ctx_a = ctx.clone();
        let ctx_b = ctx.clone();
        let (_, _) = tokio::join!(async move { ctx_a.throttle().await }, async move {
            ctx_b.throttle().await
        });

        // One of the two calls must have been delayed by roughly one
        // interval — total elapsed time for both to complete must be at
        // least min_interval, not ~0 for both.
        assert!(Instant::now().saturating_duration_since(start) >= Duration::from_millis(500));
    }
}
