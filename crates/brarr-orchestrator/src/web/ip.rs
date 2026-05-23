//! Client-IP resolution for the HTTP middlewares.
//!
//! Axum exposes the direct TCP peer through the
//! [`axum::extract::ConnectInfo`] extension, which is wired in
//! production by [`crate::web::serve`] via
//! `into_make_service_with_connect_info::<SocketAddr>`.
//!
//! When that peer is itself a configured trusted proxy, this module
//! peels back `X-Forwarded-For` to recover the original client IP —
//! walking the header right-to-left and returning the first hop that
//! is *not* a trusted proxy. Without that trust gate XFF spoofing
//! would be trivial, so we never honor the header from an unknown
//! peer.

use std::net::{IpAddr, SocketAddr};

use axum::extract::ConnectInfo;
use axum::extract::Request;

use crate::auth::TrustedPeers;

/// Resolve the effective caller IP for a request.
///
/// Returns `None` only when no `ConnectInfo` extension is attached —
/// that's the case in unit tests that build a `Router` directly and
/// call `oneshot` / `tower::ServiceExt` rather than going through
/// `into_make_service_with_connect_info`. Middlewares treat `None` the
/// same as "no bypass" and fall through to the cookie/apikey check.
#[must_use]
pub(crate) fn caller_ip(req: &Request, proxies: &TrustedPeers) -> Option<IpAddr> {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip())?;
    if proxies.is_empty() || !proxies.contains(peer) {
        return Some(peer);
    }
    // Peer is a trusted proxy — honor `X-Forwarded-For` if present.
    let raw = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|h| h.to_str().ok());
    if let Some(value) = raw {
        let parts: Vec<&str> = value.split(',').map(str::trim).collect();
        // Walk right→left and return the first hop that is NOT itself
        // a trusted proxy — that's the original client whose request
        // entered the proxy chain.
        for candidate in parts.iter().rev() {
            if let Ok(ip) = candidate.parse::<IpAddr>()
                && !proxies.contains(ip)
            {
                return Some(ip);
            }
        }
        // All hops were trusted proxies (unusual but possible). Fall
        // back to the leftmost address — by convention, the original
        // sender. Skip silently if it doesn't parse.
        if let Some(first) = parts.first().and_then(|s| s.parse::<IpAddr>().ok()) {
            return Some(first);
        }
    }
    Some(peer)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;

    fn req_with(peer: Option<SocketAddr>, xff: Option<&str>) -> Request {
        let mut builder = HttpRequest::builder().uri("/");
        if let Some(v) = xff {
            builder = builder.header("x-forwarded-for", v);
        }
        let mut req = builder.body(Body::empty()).expect("build request");
        if let Some(addr) = peer {
            req.extensions_mut().insert(ConnectInfo(addr));
        }
        req
    }

    #[test]
    fn returns_none_without_connect_info() {
        let req = req_with(None, None);
        let proxies = TrustedPeers::default();
        assert_eq!(caller_ip(&req, &proxies), None);
    }

    #[test]
    fn returns_peer_when_no_proxies_configured() {
        let peer: SocketAddr = "10.0.0.5:54321".parse().expect("addr");
        let req = req_with(Some(peer), Some("203.0.113.1"));
        let proxies = TrustedPeers::default();
        assert_eq!(
            caller_ip(&req, &proxies),
            Some("10.0.0.5".parse().expect("ip"))
        );
    }

    #[test]
    fn ignores_xff_when_peer_not_trusted_proxy() {
        let peer: SocketAddr = "203.0.113.99:443".parse().expect("addr");
        let req = req_with(Some(peer), Some("10.0.0.1"));
        let proxies = TrustedPeers::parse("10.0.0.0/24").expect("ok");
        // Peer 203.0.113.99 is NOT in the trusted proxy range, so XFF
        // is ignored and we report the direct peer.
        assert_eq!(
            caller_ip(&req, &proxies),
            Some("203.0.113.99".parse().expect("ip"))
        );
    }

    #[test]
    fn honors_xff_when_peer_is_trusted_proxy() {
        let peer: SocketAddr = "10.0.0.1:443".parse().expect("addr");
        let req = req_with(Some(peer), Some("203.0.113.50, 10.0.0.1"));
        let proxies = TrustedPeers::parse("10.0.0.0/24").expect("ok");
        assert_eq!(
            caller_ip(&req, &proxies),
            Some("203.0.113.50".parse().expect("ip"))
        );
    }

    #[test]
    fn skips_trusted_hops_in_xff_chain() {
        let peer: SocketAddr = "10.0.0.1:443".parse().expect("addr");
        // Original client → corporate proxy → our reverse proxy → us.
        let req = req_with(Some(peer), Some("203.0.113.50, 10.0.0.2, 10.0.0.1"));
        let proxies = TrustedPeers::parse("10.0.0.0/24").expect("ok");
        assert_eq!(
            caller_ip(&req, &proxies),
            Some("203.0.113.50".parse().expect("ip"))
        );
    }

    #[test]
    fn falls_back_to_peer_when_xff_missing() {
        let peer: SocketAddr = "10.0.0.1:443".parse().expect("addr");
        let req = req_with(Some(peer), None);
        let proxies = TrustedPeers::parse("10.0.0.0/24").expect("ok");
        assert_eq!(
            caller_ip(&req, &proxies),
            Some("10.0.0.1".parse().expect("ip"))
        );
    }
}
