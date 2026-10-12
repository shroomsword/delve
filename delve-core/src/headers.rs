//! Request headers the user configures for the requests delve makes to a
//! vendor (`[transport.headers]` and `[transport.header_overrides.<vendor>]`
//! in `delve-cli`'s config).
//!
//! The values can be secrets (a session cookie, an API key), so they are
//! marked sensitive on the way in and never appear in `Debug` output; an
//! error names the header and never its value.

use std::fmt;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use thiserror::Error;

/// Headers that frame the request or that delve's HTTP client manages
/// itself. Setting one could not do what the user means, and some would
/// corrupt the request, so each is refused with the reason.
const REFUSED: &[(&str, &str)] = &[
    ("host", "it is set from the request URL"),
    ("content-length", "it is set from the request body"),
    ("transfer-encoding", "it frames the request body"),
    ("connection", "it is managed by the HTTP client"),
    ("keep-alive", "it is managed by the HTTP client"),
    ("te", "it is managed by the HTTP client"),
    ("trailer", "it frames the request body"),
    ("upgrade", "it would change the protocol"),
    ("expect", "it is managed by the HTTP client"),
    ("proxy-authorization", "proxies are set under [transport]"),
    ("proxy-connection", "proxies are set under [transport]"),
    (
        "accept-encoding",
        "delve decodes the responses it asks for, and would not decode another encoding",
    ),
];

#[derive(Debug, Error, PartialEq, Eq)]
pub enum InvalidHeader {
    #[error("header name '{0}' is not a valid HTTP header name")]
    Name(String),
    #[error("header '{0}' cannot be set: {1}")]
    Refused(String, &'static str),
    /// The value is left out of the message: it may be a secret.
    #[error("the value of header '{0}' is not valid in an HTTP header (it may contain a line break or a control character)")]
    Value(String),
}

/// The headers to send with every request of one vendor.
#[derive(Clone, Default)]
pub struct RequestHeaders(HeaderMap);

impl RequestHeaders {
    pub fn new() -> Self {
        Self::default()
    }

    /// Checks that `name` could be set, and that `value`, if it can be known
    /// now, is a valid value. (A value that is still an `env:` reference is
    /// not known yet; the caller passes `None`.)
    pub fn check(name: &str, value: Option<&str>) -> Result<HeaderName, InvalidHeader> {
        let header = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| InvalidHeader::Name(name.into()))?;
        if let Some((_, why)) = REFUSED.iter().find(|(n, _)| *n == header.as_str()) {
            return Err(InvalidHeader::Refused(name.into(), why));
        }
        if let Some(value) = value {
            HeaderValue::from_str(value).map_err(|_| InvalidHeader::Value(name.into()))?;
        }
        Ok(header)
    }

    /// Sets `name` to `value`, replacing a header of the same name (names are
    /// case-insensitive). The value is marked sensitive.
    pub fn insert(&mut self, name: &str, value: &str) -> Result<(), InvalidHeader> {
        let header = Self::check(name, Some(value))?;
        let mut value =
            HeaderValue::from_str(value).map_err(|_| InvalidHeader::Value(name.into()))?;
        value.set_sensitive(true);
        self.0.insert(header, value);
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The value of header `name`, if it is set and is text.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).and_then(|value| value.to_str().ok())
    }

    /// The names that are set, lowercase.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(HeaderName::as_str)
    }

    /// Sets these headers on `map`, replacing headers of the same name.
    pub(crate) fn apply_to(&self, map: &mut HeaderMap) {
        for (name, value) in &self.0 {
            map.insert(name.clone(), value.clone());
        }
    }
}

/// Names only: the values may be secrets.
impl fmt::Debug for RequestHeaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_is_set_and_a_same_named_one_replaces_it_whatever_the_case() {
        let mut headers = RequestHeaders::new();
        headers.insert("Accept-Language", "en-US").unwrap();
        headers.insert("accept-language", "de").unwrap();
        assert_eq!(headers.names().collect::<Vec<_>>(), ["accept-language"]);
        let mut map = HeaderMap::new();
        headers.apply_to(&mut map);
        assert_eq!(map["accept-language"], "de");
    }

    #[test]
    fn a_value_is_read_back_by_name() {
        let mut headers = RequestHeaders::new();
        headers.insert("X-Api-Key", "k1").unwrap();
        assert_eq!(headers.get("x-api-key"), Some("k1"));
        assert_eq!(headers.get("cookie"), None);
    }

    #[test]
    fn applying_replaces_a_header_already_in_the_map() {
        let mut headers = RequestHeaders::new();
        headers.insert("User-Agent", "mine/1.0").unwrap();
        let mut map = HeaderMap::new();
        map.insert("user-agent", HeaderValue::from_static("delve/0.2.0"));
        headers.apply_to(&mut map);
        assert_eq!(map["user-agent"], "mine/1.0");
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn values_are_sensitive_and_debug_shows_names_only() {
        let mut headers = RequestHeaders::new();
        headers.insert("Cookie", "session=hunter2").unwrap();
        let mut map = HeaderMap::new();
        headers.apply_to(&mut map);
        assert!(map["cookie"].is_sensitive());
        let debug = format!("{headers:?}");
        assert_eq!(debug, r#"["cookie"]"#);
        assert!(!debug.contains("hunter2"));
    }

    #[test]
    fn the_headers_that_frame_the_request_are_refused_with_a_reason() {
        for name in [
            "Host",
            "content-length",
            "Transfer-Encoding",
            "Connection",
            "Keep-Alive",
            "TE",
            "Trailer",
            "Upgrade",
            "Expect",
            "Proxy-Authorization",
            "Proxy-Connection",
            "Accept-Encoding",
        ] {
            let err = RequestHeaders::new().insert(name, "x").unwrap_err();
            assert!(matches!(err, InvalidHeader::Refused(..)), "{name}: {err}");
            assert!(err.to_string().contains(name), "{err}");
        }
    }

    #[test]
    fn other_headers_that_matter_to_a_server_are_allowed() {
        let mut headers = RequestHeaders::new();
        for name in [
            "Cookie",
            "Authorization",
            "User-Agent",
            "Referer",
            "X-Api-Key",
        ] {
            headers.insert(name, "x").unwrap();
        }
        assert_eq!(headers.names().count(), 5);
    }

    #[test]
    fn a_bad_name_is_an_error() {
        for name in ["", "Has Space", "Bad:Name", "Ünicode", ":authority"] {
            assert_eq!(
                RequestHeaders::new().insert(name, "x").unwrap_err(),
                InvalidHeader::Name(name.into()),
                "{name:?}"
            );
        }
    }

    #[test]
    fn a_bad_value_is_an_error_that_does_not_contain_the_value() {
        let err = RequestHeaders::new()
            .insert("Cookie", "a=b\r\nInjected: yes")
            .unwrap_err();
        assert_eq!(err, InvalidHeader::Value("Cookie".into()));
        assert!(!err.to_string().contains("Injected"), "{err}");
    }

    #[test]
    fn check_without_a_value_checks_only_the_name() {
        assert!(RequestHeaders::check("Cookie", None).is_ok());
        assert!(RequestHeaders::check("Host", None).is_err());
        assert!(RequestHeaders::check("Cookie", Some("a=b\n")).is_err());
    }
}
