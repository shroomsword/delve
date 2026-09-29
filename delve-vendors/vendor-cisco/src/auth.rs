//! OAuth2 `client_credentials` token acquisition against Cisco's identity
//! service, used to authenticate calls to Cisco's Support APIs.
//!
//! **⚠️ Structurally confident, endpoint-unverified.** The `client_credentials`
//! grant flow implemented here (POST client_id/client_secret, get back a
//! bearer token, cache it until it's near expiry) is a standard OAuth2
//! pattern and Cisco's published API documentation describes using exactly
//! this flow for their Support APIs — that part isn't a guess. What *is*
//! unverified is [`TOKEN_URL`] itself and the exact shape of
//! [`TokenResponse`]: this was written without network access to Cisco's
//! docs or a live endpoint to test against, from general familiarity with
//! Cisco's API Console / OAuth2 documentation, which may be stale. Before
//! relying on this, check the token URL and response fields against
//! whatever Cisco's API Console currently shows for your registered
//! application (developer.cisco.com), and update the `// VERIFY:` markers
//! below accordingly.
//!
//! This module deliberately doesn't know about credential *storage* —
//! callers pass `client_id`/`client_secret` in on every call, sourced from
//! `ScrapeContext::require_credential` (see the README's "Credentials"
//! section). It only owns the token exchange and the in-memory cache.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use delve_core::context::ScrapeContext;
use delve_core::plugin::PluginError;
use serde::Deserialize;

// VERIFY: confirm this is still Cisco's current OAuth2 token endpoint for
// your registered API Console application before using this plugin for
// real. Cisco has migrated identity infrastructure before.
const TOKEN_URL: &str = "https://cloudsso.cisco.com/as/token.oauth2";

// VERIFY: confirm these field names against a real token response from
// Cisco's API Console for your app — `access_token` and `expires_in` are
// standard OAuth2 fields and likely correct, but Cisco-specific additions
// or renamings are plausible and unconfirmed here.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default = "default_expires_in")]
    expires_in: u64, // seconds
}

fn default_expires_in() -> u64 {
    3600 // conservative fallback if the field is ever absent
}

struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

/// Caches the OAuth2 access token in memory so repeated API calls within
/// one dig don't each pay for a fresh token exchange — only the first call
/// (or the first call after the cached token has gone stale) actually hits
/// [`TOKEN_URL`].
pub struct TokenCache {
    cached: Mutex<Option<CachedToken>>,
}

impl TokenCache {
    pub fn new() -> Self {
        Self { cached: Mutex::new(None) }
    }

    /// Returns a valid access token, authenticating (or re-authenticating)
    /// as needed. Calls `ctx.throttle()` itself immediately before the
    /// token request, but only on an actual cache miss — a cache hit
    /// returns without making any request at all, so it doesn't consume
    /// part of the rate-limit budget for nothing.
    pub async fn get_token(
        &self,
        ctx: &ScrapeContext,
        client_id: &str,
        client_secret: &str,
    ) -> Result<String, PluginError> {
        if let Some(token) = self.cached_token_if_valid(Instant::now()) {
            return Ok(token);
        }

        ctx.throttle().await;
        let response = ctx
            .http_client()
            .post(TOKEN_URL)
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", client_id),
                ("client_secret", client_secret),
            ])
            .send()
            .await
            .map_err(PluginError::Transport)?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(PluginError::Rejected(format!(
                "Cisco OAuth2 token request failed with status {status} — check that the \
                 client_id/client_secret credentials are valid and that {TOKEN_URL} is still \
                 correct (see this module's doc comment)"
            )));
        }

        let token: TokenResponse = response
            .json()
            .await
            .map_err(|e| PluginError::Parse(format!("failed to parse Cisco OAuth2 token response: {e}")))?;

        self.store_token(token.access_token.clone(), token.expires_in, Instant::now());
        Ok(token.access_token)
    }

    /// Pure lookup against the cache — split out from `get_token` so it's
    /// testable without any HTTP involved.
    fn cached_token_if_valid(&self, now: Instant) -> Option<String> {
        let guard = self.cached.lock().unwrap();
        guard.as_ref().filter(|t| t.expires_at > now).map(|t| t.access_token.clone())
    }

    /// Renews a little early rather than exactly at expiry, so a request
    /// that starts just before the token's real expiry doesn't race a
    /// token that goes stale mid-flight. Split out from `get_token`, same
    /// reason as `cached_token_if_valid`.
    fn store_token(&self, access_token: String, expires_in_secs: u64, now: Instant) {
        const SAFETY_MARGIN: Duration = Duration::from_secs(60);
        let ttl = Duration::from_secs(expires_in_secs).saturating_sub(SAFETY_MARGIN);
        let expires_at = now + ttl;

        let mut guard = self.cached.lock().unwrap();
        *guard = Some(CachedToken { access_token, expires_at });
    }
}

impl Default for TokenCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_freshly_stored_token_is_returned_as_valid() {
        let cache = TokenCache::new();
        let now = Instant::now();
        cache.store_token("tok-123".to_string(), 3600, now);
        assert_eq!(cache.cached_token_if_valid(now), Some("tok-123".to_string()));
    }

    #[test]
    fn a_token_is_still_valid_well_before_its_expiry() {
        let cache = TokenCache::new();
        let now = Instant::now();
        cache.store_token("tok-123".to_string(), 3600, now);
        let five_minutes_later = now + Duration::from_secs(300);
        assert_eq!(cache.cached_token_if_valid(five_minutes_later), Some("tok-123".to_string()));
    }

    #[test]
    fn a_token_is_treated_as_expired_once_past_its_stored_expiry() {
        let cache = TokenCache::new();
        let now = Instant::now();
        cache.store_token("tok-123".to_string(), 3600, now);
        // 3600s TTL minus the 60s safety margin = valid for 3540s; well
        // past that entirely, regardless of the exact margin.
        let long_after = now + Duration::from_secs(4000);
        assert_eq!(cache.cached_token_if_valid(long_after), None);
    }

    #[test]
    fn the_safety_margin_expires_the_token_before_its_literal_ttl() {
        let cache = TokenCache::new();
        let now = Instant::now();
        cache.store_token("tok-123".to_string(), 3600, now);
        // At exactly 3600s (the literal TTL) the token must already read
        // as expired, because of the 60s safety margin — this is the
        // whole point of renewing early rather than racing real expiry.
        let at_literal_ttl = now + Duration::from_secs(3600);
        assert_eq!(cache.cached_token_if_valid(at_literal_ttl), None);
    }

    #[test]
    fn no_token_stored_yet_is_a_cache_miss() {
        let cache = TokenCache::new();
        assert_eq!(cache.cached_token_if_valid(Instant::now()), None);
    }

    #[test]
    fn a_very_short_expires_in_does_not_panic_via_underflow() {
        // expires_in (2s) smaller than the 60s safety margin — must
        // saturate to zero TTL, not underflow/panic on Duration subtraction.
        let cache = TokenCache::new();
        let now = Instant::now();
        cache.store_token("tok-123".to_string(), 2, now);
        assert_eq!(cache.cached_token_if_valid(now), None, "a TTL shorter than the safety margin must saturate to already-expired");
    }

    #[test]
    fn storing_a_new_token_replaces_the_cached_one() {
        let cache = TokenCache::new();
        let now = Instant::now();
        cache.store_token("tok-old".to_string(), 3600, now);
        cache.store_token("tok-new".to_string(), 3600, now);
        assert_eq!(cache.cached_token_if_valid(now), Some("tok-new".to_string()));
    }
}
