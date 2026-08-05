//! Error type shared by every download-client implementation.

use thiserror::Error;

use crate::DownloadClientKind;

/// Everything a [`crate::DownloadClient`] call can fail with.
///
/// [`Self::Auth`] is deliberately separate from [`Self::Http`]: both
/// clients can refuse credentials while answering `200 OK` (SABnzbd
/// returns `{"status": false, "error": "API Key Incorrect"}`, qBittorrent
/// answers the string `Fails.`), so status code alone cannot tell the
/// operator "your password is wrong" apart from "the box is down".
#[derive(Debug, Error)]
pub enum DownloadClientError {
    /// `reqwest` failed to reach the client or the response died
    /// mid-flight (DNS, TLS, connection refused, body decode).
    #[error("transport error talking to {kind}: {source}")]
    Transport {
        /// Which client flavour was being contacted.
        kind: DownloadClientKind,
        /// Underlying `reqwest` error.
        #[source]
        source: reqwest::Error,
    },
    /// Non-2xx response. The body is captured (truncated) so the admin
    /// UI can show the client's own rejection text.
    #[error("{kind} returned HTTP {status}: {body}")]
    Http {
        /// Client flavour.
        kind: DownloadClientKind,
        /// HTTP status code.
        status: u16,
        /// Response body, first 1 KiB.
        body: String,
    },
    /// Credentials were refused. See the type-level note — this often
    /// arrives inside a `200 OK`.
    #[error("{kind} refused the credentials: {detail}")]
    Auth {
        /// Client flavour.
        kind: DownloadClientKind,
        /// Client-supplied reason, verbatim where one exists.
        detail: String,
    },
    /// Body parsed as JSON but didn't match the expected shape.
    #[error("{kind} returned malformed JSON: {source}")]
    Decode {
        /// Client flavour.
        kind: DownloadClientKind,
        /// Underlying serde error.
        #[source]
        source: serde_json::Error,
    },
    /// The stored configuration cannot produce a working client — a
    /// SABnzbd row with no apikey, say. Caught before any network call.
    #[error("invalid configuration for {kind}: {detail}")]
    Config {
        /// Client flavour.
        kind: DownloadClientKind,
        /// What is missing or malformed.
        detail: String,
    },
    /// `base_url` could not be joined with an endpoint path.
    #[error("invalid URL: {0}")]
    InvalidUrl(#[from] url::ParseError),
}

/// Cap a response body before it lands in an error message or a log
/// line. A misconfigured base URL pointed at some unrelated web server
/// can return a whole HTML page.
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
    fn long_bodies_are_capped() {
        let out = truncate_body(&"x".repeat(4096));
        assert_eq!(out.chars().count(), 1025, "1024 chars plus the ellipsis");
    }

    #[test]
    fn truncation_never_splits_a_codepoint() {
        // 'ç' is two bytes, and the leading 'a' pushes every one of them
        // to an odd offset — so byte 1024 lands mid-sequence and a naive
        // slice would panic.
        let body = format!("a{}", "ç".repeat(2048));
        let out = truncate_body(&body);
        assert!(out.ends_with('…'));
        assert_eq!(
            out.len(),
            1023 + "…".len(),
            "backed off to the previous boundary"
        );
    }
}
