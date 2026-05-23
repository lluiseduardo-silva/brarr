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

use std::net::IpAddr;
use std::sync::Arc;

use axum::http::HeaderMap;
use ipnet::{IpNet, Ipv4Net, Ipv6Net};
use subtle::ConstantTimeEq;

use crate::AppError;

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

/// Parsed list of IP/CIDR rules used by the auth bypass (an inbound
/// request whose peer matches one of these skips the cookie/apikey
/// check) and by the trusted-proxy gate that decides whether
/// `X-Forwarded-For` is honored.
///
/// Empty by default — nothing matches until [`Self::parse`] populates
/// it from a comma-separated spec.
#[derive(Debug, Clone, Default)]
pub struct TrustedPeers {
    nets: Vec<IpNet>,
}

impl TrustedPeers {
    /// Build from a comma-separated spec. Whitespace around tokens is
    /// trimmed; empty entries are skipped. Recognized forms:
    ///
    /// - bare IPv4/IPv6 (`192.168.1.50`, `::1`) — treated as a `/32`
    ///   or `/128`.
    /// - CIDR (`10.0.0.0/8`, `fd00::/8`).
    /// - symbolic `loopback` → `127.0.0.0/8` + `::1/128`.
    /// - symbolic `private` → RFC1918 (`10/8`, `172.16/12`,
    ///   `192.168/16`) + RFC4193 (`fc00::/7`).
    ///
    /// Hostnames are **not** accepted — DNS lookup at request time
    /// trades reliability for convenience, and the operator can
    /// resolve to an IP/CIDR themselves.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::InvalidInput`] on the first unparseable
    /// token, naming the bad entry so misconfiguration is loud.
    pub fn parse(spec: &str) -> Result<Self, AppError> {
        let mut nets = Vec::new();
        for raw in spec.split(',') {
            let token = raw.trim();
            if token.is_empty() {
                continue;
            }
            match token.to_ascii_lowercase().as_str() {
                "loopback" => {
                    nets.push(parse_cidr_literal("127.0.0.0/8")?);
                    nets.push(parse_cidr_literal("::1/128")?);
                }
                "private" => {
                    nets.push(parse_cidr_literal("10.0.0.0/8")?);
                    nets.push(parse_cidr_literal("172.16.0.0/12")?);
                    nets.push(parse_cidr_literal("192.168.0.0/16")?);
                    nets.push(parse_cidr_literal("fc00::/7")?);
                }
                _ => nets.push(parse_token(token)?),
            }
        }
        Ok(Self { nets })
    }

    /// True when no rules are configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nets.is_empty()
    }

    /// Number of parsed CIDR entries (after symbolic expansion).
    #[must_use]
    pub fn len(&self) -> usize {
        self.nets.len()
    }

    /// True when `ip` falls into any configured network.
    #[must_use]
    pub fn contains(&self, ip: IpAddr) -> bool {
        self.nets.iter().any(|n| n.contains(&ip))
    }
}

/// Auth-bypass configuration. Held alongside [`AuthConfig`] on
/// [`crate::AppState`] and consulted by the HTTP middlewares before
/// the cookie/apikey check.
///
/// Two parallel lists:
///
/// - `peers` — direct TCP peers that should skip auth entirely.
/// - `proxies` — direct TCP peers whose `X-Forwarded-For` header is
///   trusted (the leftmost untrusted hop is then matched against
///   `peers`).
///
/// gRPC is intentionally **not** affected: tonic doesn't surface the
/// peer address through a standard interceptor, and gRPC callers are
/// machines that already carry the bearer token. Re-evaluate if a
/// script on the trusted LAN ever needs passwordless gRPC.
#[derive(Debug, Clone, Default)]
pub struct BypassConfig {
    /// Peers that may skip auth.
    pub peers: TrustedPeers,
    /// Proxies whose `X-Forwarded-For` may be trusted.
    pub proxies: TrustedPeers,
}

impl BypassConfig {
    /// True when both lists are empty (feature off).
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        self.peers.is_empty() && self.proxies.is_empty()
    }
}

fn parse_cidr_literal(s: &str) -> Result<IpNet, AppError> {
    s.parse::<IpNet>()
        .map_err(|e| AppError::InvalidInput(format!("internal CIDR `{s}` failed to parse: {e}")))
}

fn parse_token(token: &str) -> Result<IpNet, AppError> {
    if let Ok(net) = token.parse::<IpNet>() {
        return Ok(net);
    }
    if let Ok(ip) = token.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => Ipv4Net::new(v4, 32)
                .map(IpNet::V4)
                .map_err(|e| AppError::InvalidInput(e.to_string())),
            IpAddr::V6(v6) => Ipv6Net::new(v6, 128)
                .map(IpNet::V6)
                .map_err(|e| AppError::InvalidInput(e.to_string())),
        };
    }
    Err(AppError::InvalidInput(format!(
        "trusted peer entry `{token}` is not an IP, CIDR, or known token (try `loopback` or `private`)"
    )))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

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

    #[test]
    fn trusted_peers_empty_spec_is_disabled() {
        let p = TrustedPeers::parse("").expect("empty spec parses");
        assert!(p.is_empty());
        assert_eq!(p.len(), 0);
        assert!(!p.contains("127.0.0.1".parse().expect("ip lit")));

        let p2 = TrustedPeers::parse("  , , ").expect("whitespace-only entries skipped");
        assert!(p2.is_empty());
    }

    #[test]
    fn trusted_peers_loopback_expands() {
        let p = TrustedPeers::parse("loopback").expect("loopback parses");
        assert_eq!(p.len(), 2);
        assert!(p.contains("127.0.0.1".parse().expect("ip")));
        assert!(p.contains("127.255.255.254".parse().expect("ip")));
        assert!(p.contains("::1".parse().expect("ip")));
        assert!(!p.contains("128.0.0.1".parse().expect("ip")));
        assert!(!p.contains("192.168.1.1".parse().expect("ip")));
    }

    #[test]
    fn trusted_peers_private_covers_rfc1918_and_rfc4193() {
        let p = TrustedPeers::parse("PRIVATE").expect("case-insensitive symbolic token");
        assert!(p.contains("10.0.0.1".parse().expect("ip")));
        assert!(p.contains("172.16.0.1".parse().expect("ip")));
        assert!(p.contains("172.31.255.254".parse().expect("ip")));
        assert!(!p.contains("172.32.0.1".parse().expect("ip")));
        assert!(p.contains("192.168.1.1".parse().expect("ip")));
        assert!(p.contains("fc00::1".parse().expect("ip")));
        assert!(p.contains("fdff::ffff".parse().expect("ip")));
        assert!(!p.contains("8.8.8.8".parse().expect("ip")));
        assert!(!p.contains("2001:db8::1".parse().expect("ip")));
    }

    #[test]
    fn trusted_peers_bare_ip_is_host_prefix() {
        let p = TrustedPeers::parse("192.168.1.50, ::1").expect("bare ips parse");
        assert!(p.contains("192.168.1.50".parse().expect("ip")));
        assert!(!p.contains("192.168.1.51".parse().expect("ip")));
        assert!(p.contains("::1".parse().expect("ip")));
    }

    #[test]
    fn trusted_peers_cidr_explicit() {
        let p = TrustedPeers::parse("10.0.0.0/24,2001:db8::/32").expect("cidr parses");
        assert!(p.contains("10.0.0.1".parse().expect("ip")));
        assert!(p.contains("10.0.0.255".parse().expect("ip")));
        assert!(!p.contains("10.0.1.1".parse().expect("ip")));
        assert!(p.contains("2001:db8::1".parse().expect("ip")));
        assert!(!p.contains("2001:db9::1".parse().expect("ip")));
    }

    #[test]
    fn trusted_peers_mixed_symbolic_and_literal() {
        let p = TrustedPeers::parse("loopback, 203.0.113.5, private").expect("mixed spec parses");
        // 2 loopback + 1 host + 4 private = 7
        assert_eq!(p.len(), 7);
        assert!(p.contains("127.0.0.1".parse().expect("ip")));
        assert!(p.contains("203.0.113.5".parse().expect("ip")));
        assert!(p.contains("10.5.5.5".parse().expect("ip")));
    }

    #[test]
    fn trusted_peers_rejects_garbage() {
        let err = TrustedPeers::parse("not-an-ip").expect_err("garbage should fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("not-an-ip"),
            "error message should name the bad token, got: {msg}"
        );

        let err2 = TrustedPeers::parse("192.168.1.0/99").expect_err("bad prefix should fail");
        assert!(format!("{err2}").contains("192.168.1.0/99"));
    }

    #[test]
    fn bypass_config_default_is_disabled() {
        let cfg = BypassConfig::default();
        assert!(cfg.is_disabled());
    }

    #[test]
    fn bypass_config_non_empty_peers_enables_it() {
        let cfg = BypassConfig {
            peers: TrustedPeers::parse("loopback").expect("ok"),
            proxies: TrustedPeers::default(),
        };
        assert!(!cfg.is_disabled());
    }
}
