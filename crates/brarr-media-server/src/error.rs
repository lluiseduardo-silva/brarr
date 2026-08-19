//! Error type shared by every media-server implementation.

use thiserror::Error;

use crate::MediaServerKind;

/// Everything a [`crate::MediaServer`] call can fail with.
///
/// [`Self::Auth`] is separate from [`Self::Http`] for the same reason it
/// is in `brarr-download-client`, arrived at differently: Plex answers a
/// real `401`, but its `/identity` endpoint answers `200` to *anyone*, so
/// a connection test aimed at the obvious place would paint a wrong token
/// green. Keeping the variant forces every implementation to prove the
/// credential rather than prove the host is up.
#[derive(Debug, Error)]
pub enum MediaServerError {
    /// `reqwest` failed to reach the server or the response died
    /// mid-flight (DNS, TLS, connection refused, body decode).
    #[error("erro de transporte falando com {kind}: {source}")]
    Transport {
        /// Which server flavour was being contacted.
        kind: MediaServerKind,
        /// Underlying `reqwest` error.
        #[source]
        source: reqwest::Error,
    },
    /// Non-2xx response. The body is captured (truncated) so the admin UI
    /// can show the server's own rejection text.
    #[error("{kind} respondeu HTTP {status}: {body}")]
    Http {
        /// Server flavour.
        kind: MediaServerKind,
        /// HTTP status code.
        status: u16,
        /// Response body, first 1 KiB.
        body: String,
    },
    /// The credential was refused.
    #[error("{kind} recusou a credencial: {detail}")]
    Auth {
        /// Server flavour.
        kind: MediaServerKind,
        /// Server-supplied reason where one exists.
        detail: String,
    },
    /// Body parsed as JSON but didn't match the expected shape.
    #[error("{kind} devolveu JSON inesperado: {source}")]
    Decode {
        /// Server flavour.
        kind: MediaServerKind,
        /// Underlying serde error.
        #[source]
        source: serde_json::Error,
    },
    /// No configured library contains the path brarr wants scanned.
    ///
    /// Its own variant, and not a silent no-op, because it is the one
    /// failure the operator can always fix: it means the path mapping is
    /// missing or wrong. Radarr answers this case by refreshing *every*
    /// location of every section of the matching type — a speculative
    /// fan-out that makes a wrong mapping look like a working
    /// integration. Naming the translated path and the locations that do
    /// exist is what turns it back into a fixable configuration error.
    #[error("nenhuma biblioteca do {kind} cobre {path} (conhecidas: {known})")]
    NoMatchingLibrary {
        /// Server flavour.
        kind: MediaServerKind,
        /// Path as it was sent, after translation.
        path: String,
        /// Comma-separated locations the server reported.
        known: String,
    },
    /// The stored configuration cannot produce a working client — a row
    /// with no token. Caught before any network call.
    #[error("configuração inválida para {kind}: {detail}")]
    Config {
        /// Server flavour.
        kind: MediaServerKind,
        /// What is missing or malformed.
        detail: String,
    },
    /// `base_url` could not be joined with an endpoint path.
    #[error("URL inválida: {0}")]
    InvalidUrl(#[from] url::ParseError),
}

/// Cap a response body before it lands in an error message or a log line.
/// A misconfigured base URL pointed at some unrelated web server can
/// return a whole HTML page.
pub(crate) fn truncate_body(body: &str) -> String {
    const LIMIT: usize = 1024;
    if body.len() <= LIMIT {
        return body.to_owned();
    }
    // Slice on a char boundary — a UTF-8 body cut mid-sequence panics.
    let mut end = LIMIT;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &body[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_bodies_pass_through() {
        assert_eq!(truncate_body("Ok."), "Ok.");
    }

    #[test]
    fn truncation_never_splits_a_codepoint() {
        // The leading 'a' pushes every two-byte 'ç' to an odd offset, so
        // byte 1024 lands mid-sequence and a naive slice would panic.
        let body = format!("a{}", "ç".repeat(2048));
        let out = truncate_body(&body);
        assert!(out.ends_with('…'));
        assert_eq!(out.len(), 1023 + "…".len(), "backed off to the boundary");
    }
}
