//! Single-token admin authentication for the HTTP UI and the gRPC
//! surface.
//!
//! Threat model is small: the orchestrator is meant to bind to
//! `127.0.0.1` by default, so the primary concern is preventing
//! casual access when a deployment exposes it over a network. There
//! is exactly **one** credential — a shared token loaded from
//! `BRARR_AUTH_TOKEN`. Anything more (user accounts, RBAC) is out of
//! scope for v1.
//!
//! ## Modes
//!
//! - [`AuthConfig::Disabled`] — token env var unset. All routes pass
//!   through unauthenticated. Logged at `warn!` once at startup so a
//!   production deployment doesn't silently leak.
//! - [`AuthConfig::Enabled`] — token set. UI requires a cookie that
//!   carries the same opaque value; gRPC requires
//!   `authorization: Bearer <token>` metadata.
//!
//! ## Token comparison
//!
//! Constant-time comparison via [`subtle::ConstantTimeEq`] so a
//! timing oracle can't enumerate tokens character by character. The
//! token itself is opaque to us — callers are expected to seed it
//! with at least 128 bits of randomness (e.g. `openssl rand -hex 32`).

use std::sync::Arc;

use axum::http::HeaderMap;
use subtle::ConstantTimeEq;

/// Cookie name used by the UI session.
pub const SESSION_COOKIE: &str = "brarr_session";

/// Auth mode picked at startup.
#[derive(Debug, Clone)]
pub enum AuthConfig {
    /// No token configured; every request is allowed through.
    Disabled,
    /// Token configured; every request must present it.
    Enabled(Arc<String>),
}

impl AuthConfig {
    /// Build from an optional raw token. `None` or an empty/whitespace
    /// string disables auth.
    #[must_use]
    pub fn from_optional(token: Option<&str>) -> Self {
        match token {
            Some(t) if !t.trim().is_empty() => Self::Enabled(Arc::new(t.trim().to_string())),
            _ => Self::Disabled,
        }
    }

    /// Is auth currently required?
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled(_))
    }

    /// Borrow the configured token, if any. Used by the push pipeline
    /// to embed `?apikey=<token>` into download URLs *arr will hit
    /// later — without this the proxy returns 401 when the *arr
    /// download client tries to grab the .torrent / .nzb.
    #[must_use]
    pub fn token(&self) -> Option<&str> {
        match self {
            Self::Disabled => None,
            Self::Enabled(t) => Some(t.as_str()),
        }
    }

    /// Compare `candidate` to the configured token in constant time.
    /// Always `true` when auth is disabled.
    #[must_use]
    pub fn token_matches(&self, candidate: &str) -> bool {
        match self {
            Self::Disabled => true,
            Self::Enabled(t) => {
                let a = t.as_bytes();
                let b = candidate.as_bytes();
                a.len() == b.len() && a.ct_eq(b).into()
            }
        }
    }

    /// Extract a bearer token from an `Authorization` header.
    /// Returns `None` if the header is absent or malformed.
    #[must_use]
    pub fn bearer_from_headers(headers: &HeaderMap) -> Option<&str> {
        let raw = headers
            .get(axum::http::header::AUTHORIZATION)?
            .to_str()
            .ok()?;
        let rest = raw
            .strip_prefix("Bearer ")
            .or_else(|| raw.strip_prefix("bearer "))?;
        Some(rest.trim())
    }

    /// Extract an `apikey=...` value from a URI query string (the format
    /// Sonarr/Radarr use when calling a Newznab/Torznab indexer). Returns
    /// the first matching parameter so trailing duplicates don't shadow
    /// the leading one.
    #[must_use]
    pub fn apikey_from_query(query: Option<&str>) -> Option<&str> {
        let q = query?;
        for pair in q.split('&') {
            let mut it = pair.splitn(2, '=');
            let key = it.next()?;
            if key.eq_ignore_ascii_case("apikey") {
                return it.next();
            }
        }
        None
    }

    /// Extract the `brarr_session` cookie value, if present.
    #[must_use]
    pub fn cookie_from_headers(headers: &HeaderMap) -> Option<String> {
        let raw = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
        for pair in raw.split(';') {
            let pair = pair.trim();
            if let Some(rest) = pair.strip_prefix(&format!("{SESSION_COOKIE}=")) {
                return Some(rest.to_string());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn disabled_mode_accepts_anything() {
        let cfg = AuthConfig::from_optional(None);
        assert!(!cfg.is_enabled());
        assert!(cfg.token_matches("anything"));
        assert!(cfg.token_matches(""));
    }

    #[test]
    fn empty_or_whitespace_treated_as_disabled() {
        assert!(!AuthConfig::from_optional(Some("")).is_enabled());
        assert!(!AuthConfig::from_optional(Some("   ")).is_enabled());
    }

    #[test]
    fn enabled_mode_compares_exact() {
        let cfg = AuthConfig::from_optional(Some("s3cret"));
        assert!(cfg.is_enabled());
        assert!(cfg.token_matches("s3cret"));
        assert!(!cfg.token_matches("s3cre"));
        assert!(!cfg.token_matches("s3cret\0"));
        assert!(!cfg.token_matches("WRONG"));
    }

    #[test]
    fn bearer_parsing_accepts_both_cases() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer abc"),
        );
        assert_eq!(AuthConfig::bearer_from_headers(&h), Some("abc"));
        h.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("bearer xyz"),
        );
        assert_eq!(AuthConfig::bearer_from_headers(&h), Some("xyz"));
    }

    #[test]
    fn bearer_parsing_rejects_non_bearer() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Basic dXNlcjpwYXNz"),
        );
        assert_eq!(AuthConfig::bearer_from_headers(&h), None);
    }

    #[test]
    fn cookie_parsing_picks_named_cookie() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_static("foo=bar; brarr_session=tok123; baz=qux"),
        );
        assert_eq!(
            AuthConfig::cookie_from_headers(&h).as_deref(),
            Some("tok123")
        );
    }

    #[test]
    fn apikey_parsing_picks_first_match() {
        assert_eq!(
            AuthConfig::apikey_from_query(Some("foo=bar&apikey=abc123&t=caps")),
            Some("abc123")
        );
        assert_eq!(
            AuthConfig::apikey_from_query(Some("APIKEY=upper")),
            Some("upper"),
            "case-insensitive name match"
        );
        assert_eq!(AuthConfig::apikey_from_query(Some("apikey=")), Some(""));
        assert_eq!(AuthConfig::apikey_from_query(Some("t=caps")), None);
        assert_eq!(AuthConfig::apikey_from_query(None), None);
        assert_eq!(AuthConfig::apikey_from_query(Some("")), None);
    }

    #[test]
    fn cookie_parsing_returns_none_when_missing() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_static("other=value"),
        );
        assert_eq!(AuthConfig::cookie_from_headers(&h), None);
    }
}
