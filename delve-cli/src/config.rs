//! Config file loading. See the README's "Configuration reference"
//! section: TOML, default path with `--config` override, no implicit
//! multi-file merging.

use std::path::{Path, PathBuf};

use delve_core::context::{Credentials, Transport, TransportOverrides};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub vendors: VendorsConfig,
    #[serde(default)]
    pub transport: TransportConfig,
    #[serde(default)]
    pub subscribers: SubscribersConfig,
    #[serde(default = "default_db_path")]
    /// Defaults under the XDG data directory (`$XDG_DATA_HOME`, typically
    /// `~/.local/share` on Linux — see `default_data_dir` below), not the
    /// config directory. The SQLite file is application data, not
    /// configuration, so it belongs in the data location even though it's
    /// configured from the same file as everything else.
    pub database_path: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct VendorsConfig {
    /// Vendor IDs to enable for `dig`/`catalog`/etc. Only meaningful for
    /// vendors actually compiled in via Cargo features — this list
    /// narrows, it can't add vendors that weren't built in.
    #[serde(default)]
    pub enabled: Vec<String>,

    /// Per-vendor credential key/value pairs, keyed by vendor id then by
    /// whatever key names that vendor's plugin documents it needs (e.g.
    /// "username"/"password", or a single "api_token"). Deliberately
    /// untyped here — the framework doesn't know what shape of credentials
    /// any given vendor needs, only the plugin does.
    ///
    /// A value of the form `"env:VAR_NAME"` is resolved from the process
    /// environment at load time via `resolve_credentials` rather than
    /// stored in the config file directly, so a config file can be
    /// committed/shared without embedding secrets in it. Anything not
    /// prefixed with `env:` is used as a literal.
    #[serde(default)]
    pub credentials: std::collections::HashMap<String, std::collections::HashMap<String, String>>,
}

impl VendorsConfig {
    /// Resolves the configured credentials for one vendor into what
    /// `ScrapeContext::new` expects — substituting any `env:VAR_NAME`
    /// values along the way. A vendor with no `[vendors.credentials.<id>]`
    /// table at all resolves to an empty map, not an error; whether that's
    /// a problem is up to the plugin (via `ScrapeContext::require_credential`),
    /// not this function. A referenced environment variable that isn't set
    /// IS an error here — silently substituting an empty string would just
    /// turn into a more confusing failure later, inside the plugin.
    pub fn resolve_credentials(&self, vendor_id: &str) -> anyhow::Result<Credentials> {
        let Some(raw) = self.credentials.get(vendor_id) else {
            return Ok(Credentials::new());
        };

        let mut resolved = Credentials::new();
        for (key, value) in raw {
            let resolved_value = match value.strip_prefix("env:") {
                Some(var_name) => std::env::var(var_name).map_err(|_| {
                    anyhow::anyhow!(
                        "credential '{key}' for vendor '{vendor_id}' references env var \
                         '{var_name}', which is not set"
                    )
                })?,
                None => value.clone(),
            };
            resolved.insert(key.clone(), resolved_value);
        }
        Ok(resolved)
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct TransportConfig {
    pub default: Option<TransportKind>,
    #[serde(default)]
    pub overrides: std::collections::HashMap<String, TransportKind>,

    /// Identifies this tool to vendor sites. Defaults to
    /// `delve_core::context::default_user_agent()` (just the tool name and
    /// version, no contact info) if unset — see that function's doc comment
    /// for why you should set this to something with real contact info
    /// before running against an actual vendor site.
    pub user_agent: Option<String>,
    /// Caps how long a single HTTP request may take. Defaults to 30s if
    /// unset — see `HttpClientConfig::request_timeout`.
    pub request_timeout_secs: Option<u64>,
    /// Caps how long establishing a connection may take, separately from
    /// the overall request timeout. Defaults to 10s if unset.
    pub connect_timeout_secs: Option<u64>,

    /// Minimum time between requests within one vendor's dig, in
    /// milliseconds. Defaults to 1000ms (one request/sec) if unset — see
    /// `delve_core::context::RateLimit::default()`. This is the concrete
    /// enforcement behind the robots.txt/ToS crawl-delay review described
    /// in the README's "Compliance" section:
    /// reviewing a vendor's terms tells you what rate is acceptable, this
    /// is what actually keeps requests to it (via `ScrapeContext::throttle()`,
    /// which plugins call before each outgoing request).
    pub min_request_interval_ms: Option<u64>,
    /// Per-vendor override for `min_request_interval_ms` — a vendor whose
    /// robots.txt specifies a different crawl-delay than the run's default
    /// gets its own entry here, keyed by vendor id. Mirrors `overrides`
    /// above, which does the same thing for `Transport`.
    #[serde(default)]
    pub rate_limit_overrides: std::collections::HashMap<String, u64>,
}

/// Simplified config-file representation of `Transport` — the config file
/// shouldn't need to spell out a full `SocketAddr` struct for the common
/// cases. Resolved into `delve_core::context::Transport` in `resolve()`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    Direct,
    Tor,
    Socks5 { addr: String },
}

impl TransportConfig {
    pub fn resolve(&self) -> anyhow::Result<(Transport, TransportOverrides)> {
        let default = self
            .default
            .as_ref()
            .map(resolve_kind)
            .transpose()?
            .unwrap_or(Transport::Direct);

        let mut overrides = TransportOverrides::new();
        for (vendor, kind) in &self.overrides {
            overrides.insert(vendor.clone(), resolve_kind(kind)?);
        }

        Ok((default, overrides))
    }

    /// Builds the HTTP client settings shared across every vendor in a run
    /// (the per-vendor knob is `Transport` itself, via `overrides` above —
    /// User-Agent and timeouts are deliberately not per-vendor).
    pub fn http_client_config(&self) -> delve_core::context::HttpClientConfig {
        let defaults = delve_core::context::HttpClientConfig::default();
        delve_core::context::HttpClientConfig {
            user_agent: self.user_agent.clone().unwrap_or(defaults.user_agent),
            request_timeout: self
                .request_timeout_secs
                .map(std::time::Duration::from_secs)
                .unwrap_or(defaults.request_timeout),
            connect_timeout: self
                .connect_timeout_secs
                .map(std::time::Duration::from_secs)
                .unwrap_or(defaults.connect_timeout),
        }
    }

    /// Builds the default rate limit plus per-vendor overrides, resolved
    /// the same way `resolve()` does for `Transport`.
    pub fn rate_limits(
        &self,
    ) -> (
        delve_core::context::RateLimit,
        delve_core::context::RateLimitOverrides,
    ) {
        let default_ms = self.min_request_interval_ms.unwrap_or_else(|| {
            delve_core::context::RateLimit::default()
                .min_interval
                .as_millis() as u64
        });
        let default = delve_core::context::RateLimit {
            min_interval: std::time::Duration::from_millis(default_ms),
        };

        let overrides = self
            .rate_limit_overrides
            .iter()
            .map(|(vendor, ms)| {
                (
                    vendor.clone(),
                    delve_core::context::RateLimit {
                        min_interval: std::time::Duration::from_millis(*ms),
                    },
                )
            })
            .collect();

        (default, overrides)
    }
}

fn resolve_kind(kind: &TransportKind) -> anyhow::Result<Transport> {
    Ok(match kind {
        TransportKind::Direct => Transport::Direct,
        TransportKind::Tor => Transport::Tor(delve_core::context::TorConfig {
            mode: delve_core::context::TorMode::ExternalDaemon {
                socks_addr: "127.0.0.1:9050".parse()?,
            },
            circuit_isolation: delve_core::context::CircuitIsolation::PerVendor,
        }),
        TransportKind::Socks5 { addr } => Transport::Socks5 {
            addr: addr.parse()?,
        },
    })
}

#[derive(Debug, Default, Deserialize)]
pub struct SubscribersConfig {
    pub webhook: Option<WebhookConfig>,
    // subscriber-email intentionally omitted from this scaffold — add a
    // config struct here + the crate under delve-subscribers/ when needed.
}

/// Only read from `dig.rs` when the `subscriber-webhook` feature is
/// compiled in — without it, parsing still validates the config shape but
/// `url` genuinely goes unused, hence the targeted allow rather than a
/// blanket one.
#[cfg_attr(not(feature = "subscriber-webhook"), allow(dead_code))]
#[derive(Debug, Deserialize)]
pub struct WebhookConfig {
    pub url: String,
}

/// Defaults the database under the XDG *data* directory
/// (`$XDG_DATA_HOME`, typically `~/.local/share` on Linux; the `dirs`
/// crate resolves the platform-appropriate equivalent elsewhere), not the
/// config directory used for `config.toml` itself — the SQLite file is
/// application data, not configuration, and belongs in the data location
/// even though both are set from the same config file.
fn default_db_path() -> String {
    default_data_dir()
        .join("delve.sqlite")
        .to_string_lossy()
        .into_owned()
}

/// `~/.local/share/delve/` on Linux (respecting `$XDG_DATA_HOME`), with
/// the `dirs` crate resolving the equivalent on other platforms (e.g.
/// `~/Library/Application Support/delve/` on macOS). This is intentionally
/// a different directory from `default_config_dir` below — see
/// `default_db_path`'s doc comment for why.
fn default_data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("delve")
}

/// `~/.config/delve/` (respecting `$XDG_CONFIG_HOME`) — the README's
/// "Configuration reference" section's default
/// resolution path, via the `dirs` crate.
fn default_config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("delve")
}

pub fn default_config_path() -> PathBuf {
    default_config_dir().join("config.toml")
}

/// Loads config from `override_path` if given, otherwise the default path
/// (see the README's "Configuration reference" section). Missing
/// default-path file is not an error — falls back to
/// `Config::default`-equivalent so a first run works without any setup;
/// missing `--config` path IS an error, since the user asked for it
/// explicitly.
pub fn load(override_path: Option<&Path>) -> anyhow::Result<Config> {
    let path = match override_path {
        Some(p) => p.to_path_buf(),
        None => default_config_path(),
    };

    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && override_path.is_none() => {
            return Ok(Config {
                vendors: VendorsConfig::default(),
                transport: TransportConfig::default(),
                subscribers: SubscribersConfig::default(),
                database_path: default_db_path(),
            });
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "failed to read config at {}: {e}",
                path.display()
            ))
        }
    };

    toml::from_str(&contents)
        .map_err(|e| anyhow::anyhow!("failed to parse config at {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_parses_with_all_defaults() {
        let config: Config =
            toml::from_str("").expect("an empty file must parse — every field has a default");
        assert!(config.vendors.enabled.is_empty());
        assert!(config.subscribers.webhook.is_none());
        assert!(config.transport.default.is_none());
    }

    #[test]
    fn vendors_enabled_parses_as_a_narrowing_allowlist() {
        let config: Config = toml::from_str(
            r#"
            [vendors]
            enabled = ["cisco", "netgear"]
            "#,
        )
        .unwrap();
        assert_eq!(
            config.vendors.enabled,
            vec!["cisco".to_string(), "netgear".to_string()]
        );
    }

    #[test]
    fn transport_default_resolves_direct() {
        let config: Config = toml::from_str(
            r#"
            [transport]
            default = "direct"
            "#,
        )
        .unwrap();
        let (transport, overrides) = config.transport.resolve().unwrap();
        assert!(matches!(transport, delve_core::context::Transport::Direct));
        assert!(overrides.is_empty());
    }

    #[test]
    fn transport_default_resolves_tor_to_external_daemon_on_the_standard_socks_port() {
        let config: Config = toml::from_str(
            r#"
            [transport]
            default = "tor"
            "#,
        )
        .unwrap();
        let (transport, _) = config.transport.resolve().unwrap();
        match transport {
            delve_core::context::Transport::Tor(cfg) => match cfg.mode {
                delve_core::context::TorMode::ExternalDaemon { socks_addr } => {
                    assert_eq!(
                        socks_addr.port(),
                        9050,
                        "the conventional Tor SOCKS port — see the README's \"Transport and proxying\" section"
                    );
                }
                delve_core::context::TorMode::Embedded => {
                    panic!("expected ExternalDaemon, not Embedded, by default")
                }
            },
            other => panic!("expected Tor, got {other:?}"),
        }
    }

    #[test]
    fn transport_overrides_apply_per_vendor_without_changing_the_default() {
        let config: Config = toml::from_str(
            r#"
            [transport]
            default = "tor"

            [transport.overrides]
            cisco = "direct"
            "#,
        )
        .unwrap();
        let (default, overrides) = config.transport.resolve().unwrap();
        assert!(
            matches!(default, delve_core::context::Transport::Tor(_)),
            "default must stay Tor"
        );
        assert!(
            matches!(overrides.get("cisco"), Some(delve_core::context::Transport::Direct)),
            "cisco's override must resolve to Direct, per the README's \"Transport and proxying\" per-vendor override example"
        );
        assert!(
            !overrides.contains_key("netgear"),
            "a vendor with no override entry must not appear in the map"
        );
    }

    #[test]
    fn socks5_transport_parses_the_given_address() {
        // Externally-tagged enum (no #[serde(tag = ...)] on TransportKind):
        // a struct variant serializes as a table keyed by the variant name,
        // not a "kind" field — `{ socks5 = { addr = ... } }`, not
        // `{ kind = "socks5", addr = ... }`.
        let config: Config = toml::from_str(
            r#"
            [transport]
            default = { socks5 = { addr = "127.0.0.1:1080" } }
            "#,
        )
        .unwrap();
        let (transport, _) = config.transport.resolve().unwrap();
        match transport {
            delve_core::context::Transport::Socks5 { addr } => assert_eq!(addr.port(), 1080),
            other => panic!("expected Socks5, got {other:?}"),
        }
    }

    #[test]
    fn invalid_socks5_address_fails_to_resolve_rather_than_silently_falling_back() {
        let config: Config = toml::from_str(
            r#"
            [transport]
            default = { socks5 = { addr = "not-an-address" } }
            "#,
        )
        .unwrap();
        assert!(
            config.transport.resolve().is_err(),
            "a malformed address must error, not silently become Direct"
        );
    }

    #[test]
    fn database_path_defaults_under_the_data_dir_not_the_config_dir_when_unset() {
        let config: Config = toml::from_str("").unwrap();
        let expected_dir = default_data_dir();
        let actual_path = std::path::Path::new(&config.database_path);

        assert_eq!(
            actual_path.file_name().unwrap(),
            "delve.sqlite",
            "got database_path = {}",
            config.database_path
        );
        // The actual point of this test: default_db_path must be built
        // from default_data_dir (XDG data dir), not default_config_dir
        // (XDG config dir) — the database is application data, not
        // configuration. Compared against the real default_data_dir()
        // output directly, rather than asserting inequality with
        // default_config_dir()'s output, since both independently fall
        // back to "." when $HOME isn't set, which would make an
        // inequality check fail in that environment for a reason
        // unrelated to this actual behavior.
        assert_eq!(actual_path.parent().unwrap(), expected_dir);
    }

    #[test]
    fn database_path_can_be_overridden() {
        let config: Config = toml::from_str(r#"database_path = "/tmp/custom.sqlite""#).unwrap();
        assert_eq!(config.database_path, "/tmp/custom.sqlite");
    }

    #[test]
    fn vendor_with_no_credentials_table_resolves_to_an_empty_map() {
        let config: Config = toml::from_str("").unwrap();
        let resolved = config.vendors.resolve_credentials("cisco").unwrap();
        assert!(resolved.is_empty());
    }

    #[test]
    fn literal_credential_values_pass_through_unchanged() {
        let config: Config = toml::from_str(
            r#"
            [vendors.credentials.cisco]
            username = "alice"
            "#,
        )
        .unwrap();
        let resolved = config.vendors.resolve_credentials("cisco").unwrap();
        assert_eq!(resolved.get("username"), Some(&"alice".to_string()));
    }

    #[test]
    fn env_prefixed_credential_values_resolve_from_the_environment() {
        // SAFETY: test-only env mutation; not run concurrently with other
        // tests that read this exact variable name.
        std::env::set_var("DELVE_TEST_CISCO_PASSWORD", "hunter2");
        let config: Config = toml::from_str(
            r#"
            [vendors.credentials.cisco]
            password = "env:DELVE_TEST_CISCO_PASSWORD"
            "#,
        )
        .unwrap();
        let resolved = config.vendors.resolve_credentials("cisco").unwrap();
        assert_eq!(resolved.get("password"), Some(&"hunter2".to_string()));
        std::env::remove_var("DELVE_TEST_CISCO_PASSWORD");
    }

    #[test]
    fn missing_referenced_env_var_is_an_error_not_an_empty_string() {
        let config: Config = toml::from_str(
            r#"
            [vendors.credentials.cisco]
            password = "env:DELVE_TEST_DOES_NOT_EXIST_XYZ"
            "#,
        )
        .unwrap();
        let err = config.vendors.resolve_credentials("cisco").unwrap_err();
        assert!(
            err.to_string().contains("DELVE_TEST_DOES_NOT_EXIST_XYZ"),
            "error should name the missing env var, got: {err}"
        );
    }

    #[test]
    fn credentials_for_one_vendor_do_not_leak_into_another() {
        let config: Config = toml::from_str(
            r#"
            [vendors.credentials.cisco]
            username = "alice"

            [vendors.credentials.netgear]
            username = "bob"
            "#,
        )
        .unwrap();
        let cisco = config.vendors.resolve_credentials("cisco").unwrap();
        let netgear = config.vendors.resolve_credentials("netgear").unwrap();
        assert_eq!(cisco.get("username"), Some(&"alice".to_string()));
        assert_eq!(netgear.get("username"), Some(&"bob".to_string()));
    }

    #[test]
    fn http_client_config_falls_back_to_framework_defaults_when_unset() {
        let config: Config = toml::from_str("").unwrap();
        let http = config.transport.http_client_config();
        let defaults = delve_core::context::HttpClientConfig::default();
        assert_eq!(http.user_agent, defaults.user_agent);
        assert_eq!(http.request_timeout, defaults.request_timeout);
        assert_eq!(http.connect_timeout, defaults.connect_timeout);
    }

    #[test]
    fn http_client_config_honors_explicit_overrides() {
        let config: Config = toml::from_str(
            r#"
            [transport]
            user_agent = "my-scraper/2.0 (contact: ops@example.test)"
            request_timeout_secs = 60
            connect_timeout_secs = 5
            "#,
        )
        .unwrap();
        let http = config.transport.http_client_config();
        assert_eq!(
            http.user_agent,
            "my-scraper/2.0 (contact: ops@example.test)"
        );
        assert_eq!(http.request_timeout, std::time::Duration::from_secs(60));
        assert_eq!(http.connect_timeout, std::time::Duration::from_secs(5));
    }

    #[test]
    fn http_client_config_allows_overriding_only_one_field() {
        // Setting just user_agent must not clobber the default timeouts.
        let config: Config = toml::from_str(
            r#"
            [transport]
            user_agent = "custom-agent/1.0"
            "#,
        )
        .unwrap();
        let http = config.transport.http_client_config();
        let defaults = delve_core::context::HttpClientConfig::default();
        assert_eq!(http.user_agent, "custom-agent/1.0");
        assert_eq!(http.request_timeout, defaults.request_timeout);
        assert_eq!(http.connect_timeout, defaults.connect_timeout);
    }

    #[test]
    fn rate_limits_falls_back_to_the_framework_default_when_unset() {
        let config: Config = toml::from_str("").unwrap();
        let (default, overrides) = config.transport.rate_limits();
        let expected = delve_core::context::RateLimit::default();
        assert_eq!(default.min_interval, expected.min_interval);
        assert!(overrides.is_empty());
    }

    #[test]
    fn rate_limits_honors_an_explicit_default_interval() {
        let config: Config = toml::from_str(
            r#"
            [transport]
            min_request_interval_ms = 2500
            "#,
        )
        .unwrap();
        let (default, _) = config.transport.rate_limits();
        assert_eq!(default.min_interval, std::time::Duration::from_millis(2500));
    }

    #[test]
    fn rate_limits_per_vendor_overrides_do_not_affect_the_default_or_other_vendors() {
        let config: Config = toml::from_str(
            r#"
            [transport]
            min_request_interval_ms = 1000

            [transport.rate_limit_overrides]
            cisco = 5000
            "#,
        )
        .unwrap();
        let (default, overrides) = config.transport.rate_limits();
        assert_eq!(default.min_interval, std::time::Duration::from_millis(1000));
        assert_eq!(
            overrides.get("cisco").unwrap().min_interval,
            std::time::Duration::from_millis(5000)
        );
        assert!(
            !overrides.contains_key("netgear"),
            "a vendor with no override entry must not appear in the map"
        );
    }
}
