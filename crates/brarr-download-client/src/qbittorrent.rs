//! qBittorrent WebUI API v2 client (qBittorrent 4.1+).
//!
//! ## Session handling
//!
//! qBittorrent is cookie-authenticated, not header-authenticated:
//! `POST api/v2/auth/login` with a username/password form returns a
//! `SID` cookie that every subsequent call must carry. The SID is held
//! behind a [`tokio::sync::Mutex`] so a burst of concurrent calls
//! performs one login rather than one login each — the lock is held
//! across the login round-trip on purpose.
//!
//! Two behaviours the code has to survive:
//!
//! - **The WebUI can be configured to skip authentication** for
//!   localhost or a whitelisted subnet. Then there is no username to
//!   send and no cookie to hold, and requests simply work unauthenticated
//!   — so an empty username is treated as "bypass", not as an error.
//! - **A SID does not live forever.** It is dropped when qBittorrent
//!   restarts and after an idle timeout, and the server answers `403`
//!   rather than a redirect. Every request retries exactly once through
//!   a fresh login on a `403`.

use std::time::Duration;

use reqwest::Client as HttpClient;
use reqwest::header::{HeaderMap, SET_COOKIE};
use tokio::sync::Mutex;
use tracing::debug;
use url::Url;

use crate::error::truncate_body;
use crate::{
    ClientFuture, ClientStatus, DownloadClient, DownloadClientConfig, DownloadClientError,
    DownloadClientKind, endpoint,
};

/// The kind every error in this module carries.
const KIND: DownloadClientKind = DownloadClientKind::Qbittorrent;

/// qBittorrent answers its login POST with one of these two literals,
/// status `200` either way.
const LOGIN_OK: &str = "Ok.";

/// HTTP client for one qBittorrent instance.
#[derive(Debug)]
pub struct QbittorrentClient {
    config: DownloadClientConfig,
    http: HttpClient,
    /// Current `SID` cookie value. `None` means "not logged in yet", or
    /// "this instance has authentication bypassed".
    session: Mutex<Option<String>>,
}

impl QbittorrentClient {
    /// Build a client over `config`.
    ///
    /// Credentials are not required: a WebUI with "bypass authentication
    /// for clients on localhost" enabled has none to give.
    ///
    /// # Errors
    ///
    /// Returns [`DownloadClientError::Transport`] if `reqwest` cannot
    /// instantiate its TLS backend (system-level, rare).
    pub fn new(config: DownloadClientConfig) -> Result<Self, DownloadClientError> {
        let http = HttpClient::builder()
            .user_agent(concat!("brarr/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|source| DownloadClientError::Transport { kind: KIND, source })?;
        Ok(Self {
            config,
            http,
            session: Mutex::new(None),
        })
    }

    /// The configuration this client was built for.
    #[must_use]
    pub fn config(&self) -> &DownloadClientConfig {
        &self.config
    }

    /// `POST api/v2/auth/login`. Returns the fresh `SID`, or `None` when
    /// the instance has no username configured (authentication bypass).
    async fn login(&self) -> Result<Option<String>, DownloadClientError> {
        let Some(username) = self
            .config
            .username
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
        else {
            debug!(
                target: "brarr_download_client",
                name = %self.config.name,
                "no username configured — assuming the WebUI bypasses authentication"
            );
            return Ok(None);
        };
        let password = self.config.password.as_deref().unwrap_or_default();
        let url = endpoint(&self.config.base_url, "api/v2/auth/login")?;

        let resp = self
            .http
            .post(url)
            .form(&[("username", username), ("password", password)])
            .send()
            .await
            .map_err(|source| DownloadClientError::Transport { kind: KIND, source })?;

        let status = resp.status();
        // Read the cookie before the body — `text()` consumes the response.
        let sid = sid_from_headers(resp.headers());
        let body = resp
            .text()
            .await
            .map_err(|source| DownloadClientError::Transport { kind: KIND, source })?;

        if status.as_u16() == 403 {
            // qBittorrent bans the caller IP for a while after repeated
            // failures. Saying so beats "HTTP 403", which reads like a
            // permissions problem the operator cannot act on.
            return Err(DownloadClientError::Auth {
                kind: KIND,
                detail: "login recusado — usuário/senha errados ou IP temporariamente banido por tentativas repetidas".to_owned(),
            });
        }
        if !status.is_success() {
            return Err(DownloadClientError::Http {
                kind: KIND,
                status: status.as_u16(),
                body: truncate_body(&body),
            });
        }
        // `Fails.` on bad credentials — same 200 as success.
        if !body.trim().eq_ignore_ascii_case(LOGIN_OK) {
            return Err(DownloadClientError::Auth {
                kind: KIND,
                detail: truncate_body(body.trim()),
            });
        }
        Ok(sid)
    }

    /// Current session, logging in first when there is none.
    ///
    /// The mutex is held across the login so concurrent callers queue
    /// behind one round-trip instead of racing into several.
    async fn session_id(&self) -> Result<Option<String>, DownloadClientError> {
        let mut guard = self.session.lock().await;
        if guard.is_none() {
            *guard = self.login().await?;
        }
        Ok(guard.clone())
    }

    /// Forget the current session so the next call logs in again.
    async fn invalidate_session(&self) {
        *self.session.lock().await = None;
    }

    /// One GET, returning `(status, body)` without interpreting either.
    async fn send_get(
        &self,
        url: Url,
        sid: Option<&str>,
    ) -> Result<(u16, String), DownloadClientError> {
        let mut req = self.http.get(url);
        if let Some(sid) = sid {
            req = req.header(reqwest::header::COOKIE, format!("SID={sid}"));
        }
        let resp = req
            .send()
            .await
            .map_err(|source| DownloadClientError::Transport { kind: KIND, source })?;
        let status = resp.status().as_u16();
        let body = resp
            .text()
            .await
            .map_err(|source| DownloadClientError::Transport { kind: KIND, source })?;
        Ok((status, body))
    }

    /// Authenticated GET returning the body as text, retrying once
    /// through a fresh login when the session has expired.
    async fn authorized_get(&self, path: &str) -> Result<String, DownloadClientError> {
        let url = endpoint(&self.config.base_url, path)?;
        let sid = self.session_id().await?;
        let (status, body) = self.send_get(url.clone(), sid.as_deref()).await?;

        let (status, body) = if status == 403 && sid.is_some() {
            debug!(
                target: "brarr_download_client",
                name = %self.config.name,
                "SID rejected — re-authenticating and retrying once"
            );
            self.invalidate_session().await;
            let fresh = self.session_id().await?;
            self.send_get(url, fresh.as_deref()).await?
        } else {
            (status, body)
        };

        if status == 403 {
            return Err(DownloadClientError::Auth {
                kind: KIND,
                detail: "a WebUI exige login e as credenciais configuradas não foram aceitas"
                    .to_owned(),
            });
        }
        if !(200..300).contains(&status) {
            return Err(DownloadClientError::Http {
                kind: KIND,
                status,
                body: truncate_body(&body),
            });
        }
        Ok(body)
    }
}

impl DownloadClient for QbittorrentClient {
    fn name(&self) -> &str {
        &self.config.name
    }

    fn kind(&self) -> DownloadClientKind {
        KIND
    }

    fn test_connection(&self) -> ClientFuture<'_, Result<ClientStatus, DownloadClientError>> {
        Box::pin(async move {
            // Plain text, e.g. `v5.0.4` — not JSON.
            let version = self.authorized_get("api/v2/app/version").await?;
            Ok(ClientStatus {
                version: version.trim().to_owned(),
            })
        })
    }
}

/// Pull the `SID` value out of the response's `Set-Cookie` headers.
///
/// Hand-parsed rather than pulled in through `reqwest`'s cookie store:
/// one cookie, one name, and doing it here keeps the session an explicit
/// field that the 403-retry path can reset.
fn sid_from_headers(headers: &HeaderMap) -> Option<String> {
    for value in headers.get_all(SET_COOKIE) {
        let Ok(raw) = value.to_str() else { continue };
        for part in raw.split(';') {
            let part = part.trim();
            if let Some(sid) = part.strip_prefix("SID=") {
                if !sid.is_empty() {
                    return Some(sid.to_owned());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests assert on happy paths")]

    use super::*;

    fn headers_with(raw: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.append(SET_COOKIE, raw.parse().unwrap());
        h
    }

    #[test]
    fn sid_is_extracted_from_a_full_cookie_line() {
        let h = headers_with("SID=8ndkPPS3D+; HttpOnly; path=/");
        assert_eq!(sid_from_headers(&h).as_deref(), Some("8ndkPPS3D+"));
    }

    #[test]
    fn an_unrelated_cookie_is_ignored() {
        let h = headers_with("theme=dark; path=/");
        assert!(sid_from_headers(&h).is_none());
    }

    #[test]
    fn an_empty_sid_does_not_count_as_a_session() {
        // qBittorrent clears the cookie this way on logout; treating it
        // as a session would send `Cookie: SID=` forever.
        let h = headers_with("SID=; Max-Age=0; path=/");
        assert!(sid_from_headers(&h).is_none());
    }
}
