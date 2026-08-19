//! Signing in to Plex — the PIN flow at plex.tv.
//!
//! Jellyfin and Emby hand out a key the operator pastes. Plex does not:
//! the credential is an account token, and getting one means sending a
//! person to plex.tv, having them approve a short-lived PIN, and then
//! reading the token back. Four calls:
//!
//! 1. `POST /api/v2/pins?strong=true` → `{ id, code }`, `authToken` null.
//! 2. The operator's browser goes to `app.plex.tv/auth#!?…&code=…`.
//! 3. `GET /api/v2/pins/{id}` until `authToken` is filled in.
//! 4. `GET /api/v2/ping` from time to time, so an idle token is not
//!    expired out from under us.
//!
//! ## Three things the Sonarr source settles
//!
//! - **`X-Plex-Client-Identifier` is generated once and kept forever** —
//!   `ConfigService.cs:392` stores a GUID with `persist: true`. It ties
//!   the PIN to the app, so a value that changes between creating the PIN
//!   and redeeming it orphans the token. brarr keeps it in `settings`.
//! - **`#!` is literal.** Sonarr carries a workaround comment for it
//!   (*"#! is stripped out of the URL when building"*); the fragment here
//!   is built by hand for the same reason.
//! - **There is no callback, and that is a fix rather than a shortcut.**
//!   Sonarr used to pass a `forwardUrl` and land the browser on an
//!   `oauth.html` that called back into the opener. Plex then started
//!   sending `Cross-Origin-Opener-Policy` on `app.plex.tv`, which severs
//!   the popup from its opener: `window.opener` reads `null`, the
//!   callback throws, and the operator is left staring at that page's
//!   placeholder text (Sonarr issue #8126). The fix was PR #8170 —
//!   *"we can avoid this being an issue for future changes by polling
//!   instead of communicating between tabs"*. brarr starts where that
//!   ended, and being server-rendered it goes further: the poll happens
//!   here, not in the browser, which also sidesteps the CORS and DNS
//!   failures the \*arr inherit from doing the pin call client-side.
//!
//! ## Two deliberate divergences
//!
//! - The \*arr announce themselves as `X-Plex-Platform: Windows`, version
//!   `7`, on every operating system. That is legacy hardcoding, not a
//!   requirement, and brarr sends what it actually is.
//! - **The poll stops.** Neither \*arr bounds it: Sonarr's loop keeps
//!   asking every five seconds forever, and a PIN lives 30 minutes
//!   (`expiresIn: 1799`), so an operator who closes the tab leaves a
//!   spinner turning until they navigate away. [`PlexPin`] carries the
//!   lifetime plex.tv reports so the caller can expire the attempt on
//!   plex.tv's own clock.

use serde::Deserialize;
use tracing::{debug, trace};
use url::Url;

use crate::error::truncate_body;
use crate::{MediaServerError, MediaServerKind, http_client};

const KIND: MediaServerKind = MediaServerKind::Plex;

/// Where the PIN flow lives. Overridable so the tests can point it at a
/// mock server.
const PLEX_TV: &str = "https://plex.tv";

/// Where the operator's browser goes to approve the PIN.
const PLEX_AUTH_APP: &str = "https://app.plex.tv/auth";

/// How brarr introduces itself to plex.tv. These end up in the account's
/// device list, so they are the operator-facing name of this integration.
const PRODUCT: &str = "brarr";
const PLATFORM: &str = "Linux";

/// Identity brarr presents to plex.tv.
///
/// The `client_identifier` is the load-bearing field: it must be the same
/// string on every call, forever, or a token stops being recognisably
/// ours.
#[derive(Debug, Clone)]
pub struct PlexIdentity {
    /// Stable per-install identifier, generated once and persisted.
    pub client_identifier: String,
    /// Name shown in the account's device list.
    pub device_name: String,
    /// brarr's own version.
    pub version: String,
}

impl PlexIdentity {
    /// Build an identity from the persisted client identifier.
    #[must_use]
    pub fn new(client_identifier: impl Into<String>) -> Self {
        Self {
            client_identifier: client_identifier.into(),
            device_name: PRODUCT.to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }

    /// The `X-Plex-*` set every plex.tv call carries.
    fn params(&self) -> Vec<(&'static str, String)> {
        vec![
            ("X-Plex-Client-Identifier", self.client_identifier.clone()),
            ("X-Plex-Product", PRODUCT.to_owned()),
            ("X-Plex-Platform", PLATFORM.to_owned()),
            ("X-Plex-Platform-Version", self.version.clone()),
            ("X-Plex-Device-Name", self.device_name.clone()),
            ("X-Plex-Version", self.version.clone()),
        ]
    }
}

/// A PIN waiting to be approved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlexPin {
    /// plex.tv's id for the PIN, used to poll it.
    pub id: i64,
    /// The short code the sign-in URL carries.
    pub code: String,
    /// Lifetime plex.tv reported, in seconds.
    ///
    /// Sonarr deserialises only `{Id, Code, AuthToken}` and throws this
    /// away; brarr keeps it so the mailbox holding a pending login can
    /// expire when the PIN does instead of on a number picked here.
    pub expires_in_seconds: Option<i64>,
}

/// What one poll of a PIN found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinState {
    /// Still waiting on the human.
    Pending,
    /// Approved — the account token.
    Authorized(String),
    /// plex.tv no longer knows this PIN: it timed out, or it was already
    /// redeemed. Its own state rather than an error, because the screen
    /// has something specific and useful to say about it.
    Expired,
}

/// Client for the plex.tv side of the sign-in.
#[derive(Debug)]
pub struct PlexLogin {
    identity: PlexIdentity,
    base: Url,
    auth_app: String,
    http: reqwest::Client,
}

impl PlexLogin {
    /// Build a login client for this install's identity.
    ///
    /// # Errors
    ///
    /// - [`MediaServerError::Transport`] if the TLS backend fails to
    ///   instantiate.
    /// - [`MediaServerError::InvalidUrl`] never in practice — the base is
    ///   a constant.
    pub fn new(identity: PlexIdentity) -> Result<Self, MediaServerError> {
        Ok(Self {
            identity,
            base: Url::parse(PLEX_TV)?,
            auth_app: PLEX_AUTH_APP.to_owned(),
            http: http_client(KIND)?,
        })
    }

    /// Point both plex.tv endpoints somewhere else. Tests only.
    #[must_use]
    pub fn with_base_url(mut self, base: &str, auth_app: &str) -> Self {
        if let Ok(url) = Url::parse(base) {
            self.base = url;
        }
        auth_app.clone_into(&mut self.auth_app);
        self
    }

    /// Ask plex.tv for a fresh PIN.
    ///
    /// # Errors
    ///
    /// - [`MediaServerError::Transport`] when plex.tv is unreachable.
    /// - [`MediaServerError::Http`] on a non-2xx answer.
    /// - [`MediaServerError::Decode`] when the payload does not parse.
    pub async fn create_pin(&self) -> Result<PlexPin, MediaServerError> {
        let mut url = self.endpoint("api/v2/pins")?;
        {
            let mut query = url.query_pairs_mut();
            for (key, value) in self.identity.params() {
                query.append_pair(key, &value);
            }
            // `strong` asks for a longer, less guessable code.
            query.append_pair("strong", "true");
        }
        let body = self.send(self.http.post(url)).await?;
        let dto: PinDto = serde_json::from_str(&body)
            .map_err(|source| MediaServerError::Decode { kind: KIND, source })?;
        debug!(
            target: "brarr_media_server::plex_auth",
            pin_id = dto.id,
            expires_in = dto.expires_in,
            "plex pin created"
        );
        Ok(PlexPin {
            id: dto.id,
            code: dto.code.unwrap_or_default(),
            expires_in_seconds: dto.expires_in,
        })
    }

    /// Where to send the operator's browser.
    ///
    /// The `#!` and everything after it is a fragment, so it never
    /// reaches a server — which is also why the code cannot be leaked by
    /// an access log on the way. Plex's own forum post documents the
    /// plain `#?` form and both are in wide use; `#!?` is what Radarr and
    /// python-plexapi send, so it is the better-exercised of the two.
    ///
    /// No `forwardUrl`: the operator comes back to the brarr tab, which
    /// never navigated away, and the token arrives by polling.
    #[must_use]
    pub fn sign_in_url(&self, code: &str) -> String {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.append_pair("clientID", &self.identity.client_identifier);
        query.append_pair("code", code);
        query.append_pair("context[device][product]", PRODUCT);
        query.append_pair("context[device][platform]", PLATFORM);
        query.append_pair("context[device][platformVersion]", &self.identity.version);
        query.append_pair("context[device][version]", &self.identity.version);
        query.append_pair("context[device][deviceName]", &self.identity.device_name);
        format!("{}#!?{}", self.auth_app, query.finish())
    }

    /// Check whether the PIN has been approved yet.
    ///
    /// # Errors
    ///
    /// - [`MediaServerError::Transport`] when plex.tv is unreachable.
    /// - [`MediaServerError::Http`] on an unexpected status.
    /// - [`MediaServerError::Decode`] when the payload does not parse.
    pub async fn poll_pin(&self, pin_id: i64) -> Result<PinState, MediaServerError> {
        let mut url = self.endpoint(&format!("api/v2/pins/{pin_id}"))?;
        {
            let mut query = url.query_pairs_mut();
            for (key, value) in self.identity.params() {
                query.append_pair(key, &value);
            }
        }
        let resp = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|source| MediaServerError::Transport { kind: KIND, source })?;
        // plex.tv forgets a PIN once it lapses or is redeemed, and
        // answers `404`. That is an outcome, not a failure.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(PinState::Expired);
        }
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|source| MediaServerError::Transport { kind: KIND, source })?;
        if !status.is_success() {
            return Err(MediaServerError::Http {
                kind: KIND,
                status: status.as_u16(),
                body: truncate_body(&body),
            });
        }
        let dto: PinDto = serde_json::from_str(&body)
            .map_err(|source| MediaServerError::Decode { kind: KIND, source })?;
        match dto.auth_token.as_deref().map(str::trim) {
            Some(token) if !token.is_empty() => Ok(PinState::Authorized(token.to_owned())),
            _ => {
                trace!(target: "brarr_media_server::plex_auth", pin_id, "pin still pending");
                Ok(PinState::Pending)
            }
        }
    }

    /// Tell plex.tv the token is still in use.
    ///
    /// Best-effort and deliberately infallible: Plex expires idle tokens,
    /// so this is worth doing, but a plex.tv outage must not turn into a
    /// failed library notification. Sonarr swallows every exception here
    /// for the same reason.
    ///
    /// Returning `bool` rather than `Result` is also an admission: the
    /// sources disagree about what this endpoint *is*. Radarr and
    /// python-plexapi both document it as refreshing the token, while the
    /// community API spec describes a health check needing no auth at
    /// all. Infallible and cheap is the shape that is correct under
    /// either reading.
    pub async fn ping(&self, token: &str) -> bool {
        let Ok(mut url) = self.endpoint("api/v2/ping") else {
            return false;
        };
        {
            let mut query = url.query_pairs_mut();
            for (key, value) in self.identity.params() {
                query.append_pair(key, &value);
            }
            query.append_pair("X-Plex-Token", token);
        }
        match self.http.get(url).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(e) => {
                trace!(target: "brarr_media_server::plex_auth", error = %e, "plex.tv ping failed");
                false
            }
        }
    }

    fn endpoint(&self, path: &str) -> Result<Url, MediaServerError> {
        Ok(crate::endpoint(&self.base, path)?)
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<String, MediaServerError> {
        let resp = req
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|source| MediaServerError::Transport { kind: KIND, source })?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|source| MediaServerError::Transport { kind: KIND, source })?;
        if !status.is_success() {
            return Err(MediaServerError::Http {
                kind: KIND,
                status: status.as_u16(),
                body: truncate_body(&body),
            });
        }
        Ok(body)
    }
}

#[derive(Debug, Deserialize)]
struct PinDto {
    id: i64,
    #[serde(default)]
    code: Option<String>,
    #[serde(rename = "authToken", default)]
    auth_token: Option<String>,
    #[serde(rename = "expiresIn", default)]
    expires_in: Option<i64>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests assert on happy paths")]

    use super::*;

    fn identity() -> PlexIdentity {
        PlexIdentity::new("11111111-2222-3333-4444-555555555555")
    }

    #[test]
    fn the_sign_in_url_keeps_the_literal_hashbang() {
        let login = PlexLogin::new(identity()).unwrap();
        let url = login.sign_in_url("ABCD");
        assert!(url.starts_with("https://app.plex.tv/auth#!?"), "got {url}");
        assert!(url.contains("code=ABCD"));
        assert!(url.contains("clientID=11111111-2222-3333-4444-555555555555"));
    }

    #[test]
    fn the_sign_in_url_encodes_the_bracketed_context_keys() {
        let login = PlexLogin::new(identity()).unwrap();
        let url = login.sign_in_url("ABCD");
        assert!(
            url.contains("context%5Bdevice%5D%5Bproduct%5D=brarr"),
            "brackets have to survive as escapes, not raw: {url}"
        );
    }

    #[test]
    fn every_call_carries_the_same_client_identifier() {
        let params = identity().params();
        let id = params
            .iter()
            .find(|(k, _)| *k == "X-Plex-Client-Identifier")
            .map(|(_, v)| v.as_str());
        assert_eq!(id, Some("11111111-2222-3333-4444-555555555555"));
        assert!(
            params
                .iter()
                .any(|(k, v)| *k == "X-Plex-Product" && v == "brarr")
        );
    }

    #[test]
    fn a_pin_payload_without_a_token_reads_as_pending() {
        let dto: PinDto =
            serde_json::from_str(r#"{"id":42,"code":"WXYZ","authToken":null}"#).unwrap();
        assert_eq!(dto.id, 42);
        assert_eq!(dto.auth_token, None);
    }
}
