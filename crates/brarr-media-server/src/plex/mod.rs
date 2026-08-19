//! The Plex dialect: list sections, pick one by path, ask it to rescan.
//!
//! ## Why the section is chosen by path
//!
//! A Plex refresh is addressed to a section, so something has to decide
//! *which*. Choosing by media type is the obvious move and it is wrong on
//! the ordinary case: a library with separate `Animes` and `Series`
//! shelves has two sections of type `show`, and every episode would go to
//! whichever one came first. The section that should hear about a file is
//! the one whose `Location` contains it, and Plex reports those
//! locations, so the answer is read rather than guessed.
//!
//! Radarr compares the *root folder* against `location.Path` for
//! equality, and when nothing matches it fans out — one refresh per
//! location of every section of the right type, most of them naming a
//! directory that does not exist there. This module matches on a prefix
//! instead (so a section pointed at a parent directory still wins) and
//! refuses rather than fans out: a path no section covers means the
//! operator's mapping is wrong, and a speculative fan-out is what makes a
//! wrong mapping look like a working integration.
//!
//! ## Authentication
//!
//! `X-Plex-Token`, sent as a **header** — verified against a real server
//! (1.43.3), and it keeps the credential out of URLs and access logs,
//! which is where the \*arr put it. A wrong token is a real `401`; the
//! trap is `/identity`, which answers `200` to anyone, so nothing here
//! uses it to prove a connection.

pub mod auth;

use std::collections::BTreeSet;

use serde::Deserialize;
use tracing::debug;
use url::Url;

pub use auth::{PinState, PlexIdentity, PlexLogin, PlexPin};

use crate::error::truncate_body;
use crate::{
    Library, LibraryUpdate, MediaServer, MediaServerConfig, MediaServerError, MediaServerKind,
    ServerFuture, ServerStatus, endpoint, http_client, known_locations, resolve_targets,
};

const KIND: MediaServerKind = MediaServerKind::Plex;

/// Plex Media Server, over its HTTP API.
#[derive(Debug)]
pub struct PlexClient {
    config: MediaServerConfig,
    token: String,
    http: reqwest::Client,
}

impl PlexClient {
    /// Build a client from stored configuration.
    ///
    /// # Errors
    ///
    /// - [`MediaServerError::Config`] when no token is stored. Plex has
    ///   no anonymous mode worth supporting: an unclaimed server would
    ///   answer, but this operator's is claimed and a blank token would
    ///   fail every call with a `401` that reads like a wrong password.
    /// - [`MediaServerError::Transport`] if the TLS backend fails to
    ///   instantiate.
    pub fn new(config: MediaServerConfig) -> Result<Self, MediaServerError> {
        let token = config
            .token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| MediaServerError::Config {
                kind: KIND,
                detail: "o Plex precisa de um token — use \"entrar com o Plex\" ou cole um"
                    .to_owned(),
            })?
            .to_owned();
        let http = http_client(KIND)?;
        Ok(Self {
            config,
            token,
            http,
        })
    }

    /// The configuration this client was built for.
    #[must_use]
    pub fn config(&self) -> &MediaServerConfig {
        &self.config
    }

    /// One authenticated GET, decoded as JSON.
    async fn get<T: serde::de::DeserializeOwned>(&self, url: Url) -> Result<T, MediaServerError> {
        let body = self.get_text(url).await?;
        serde_json::from_str(&body)
            .map_err(|source| MediaServerError::Decode { kind: KIND, source })
    }

    /// One authenticated GET, as text. The refresh endpoint answers with
    /// an empty body, so not every call can be decoded.
    async fn get_text(&self, url: Url) -> Result<String, MediaServerError> {
        debug!(
            target: "brarr_media_server::plex",
            name = %self.config.name,
            path = url.path(),
            "plex call"
        );
        let resp = self
            .http
            .get(url)
            .header("X-Plex-Token", &self.token)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|source| MediaServerError::Transport { kind: KIND, source })?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|source| MediaServerError::Transport { kind: KIND, source })?;
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(MediaServerError::Auth {
                kind: KIND,
                detail: "o Plex recusou o token; refaça o login ou cole um token novo".to_owned(),
            });
        }
        if !status.is_success() {
            return Err(MediaServerError::Http {
                kind: KIND,
                status: status.as_u16(),
                body: truncate_body(&body),
            });
        }
        Ok(body)
    }

    /// Every section the token can see.
    ///
    /// # Errors
    ///
    /// Propagates from [`Self::get`].
    pub async fn sections(&self) -> Result<Vec<Library>, MediaServerError> {
        let url = endpoint(&self.config.base_url, "library/sections")?;
        let payload: SectionsPayload = self.get(url).await?;
        Ok(payload
            .media_container
            .directory
            .into_iter()
            .map(|d| Library {
                id: d.key,
                title: d.title,
                locations: d.location.into_iter().map(|l| l.path).collect(),
            })
            .collect())
    }

    /// The server version, from the unauthenticated identity endpoint.
    async fn version(&self) -> Result<String, MediaServerError> {
        let url = endpoint(&self.config.base_url, "identity")?;
        let payload: IdentityPayload = self.get(url).await?;
        Ok(payload.media_container.version.unwrap_or_default())
    }

    /// Ask one section to rescan itself, top to bottom.
    ///
    /// The same endpoint as [`Self::refresh`] with the `path` left off —
    /// which is what the Plex web UI's "Scan Library Files" does.
    async fn refresh_all(&self, section_id: &str) -> Result<(), MediaServerError> {
        let url = endpoint(
            &self.config.base_url,
            &format!("library/sections/{section_id}/refresh"),
        )?;
        self.get_text(url).await?;
        Ok(())
    }

    /// Ask one section to rescan one directory.
    async fn refresh(&self, section_id: &str, path: &str) -> Result<(), MediaServerError> {
        let mut url = endpoint(
            &self.config.base_url,
            &format!("library/sections/{section_id}/refresh"),
        )?;
        url.query_pairs_mut().append_pair("path", path);
        // A GET, not a POST — read from the Sonarr source, where
        // `PlexServerProxy.Update` builds `HttpMethod.Get`.
        self.get_text(url).await?;
        Ok(())
    }
}

impl MediaServer for PlexClient {
    fn name(&self) -> &str {
        &self.config.name
    }

    fn kind(&self) -> MediaServerKind {
        KIND
    }

    fn test_connection(&self) -> ServerFuture<'_, Result<ServerStatus, MediaServerError>> {
        Box::pin(async move {
            // Sections first: it is the call that proves the token, and
            // the one whose failure the operator needs to hear about.
            let libraries = self.sections().await?;
            let version = self.version().await.unwrap_or_default();
            Ok(ServerStatus { version, libraries })
        })
    }

    fn notify_updated<'a>(
        &'a self,
        updates: &'a [LibraryUpdate],
    ) -> ServerFuture<'a, Result<(), MediaServerError>> {
        Box::pin(async move {
            if updates.is_empty() {
                return Ok(());
            }
            let libraries = self.sections().await?;
            if libraries.is_empty() {
                return Err(MediaServerError::NoMatchingLibrary {
                    kind: KIND,
                    path: updates[0].path.clone(),
                    known: known_locations(&libraries),
                });
            }
            // Deduped because tier 3 can send the same (section, path)
            // twice when one section is built from two locations.
            let mut sent: BTreeSet<(String, String)> = BTreeSet::new();
            for update in updates {
                for (library, path) in resolve_targets(&libraries, update) {
                    if sent.insert((library.id.clone(), path.clone())) {
                        self.refresh(&library.id, &path).await?;
                    }
                }
            }
            Ok(())
        })
    }

    fn rescan_all(&self) -> ServerFuture<'_, Result<(), MediaServerError>> {
        Box::pin(async move {
            for library in self.sections().await? {
                self.refresh_all(&library.id).await?;
            }
            Ok(())
        })
    }
}

#[derive(Debug, Deserialize)]
struct SectionsPayload {
    #[serde(rename = "MediaContainer")]
    media_container: SectionsContainer,
}

#[derive(Debug, Deserialize)]
struct SectionsContainer {
    #[serde(rename = "Directory", default)]
    directory: Vec<SectionDto>,
}

#[derive(Debug, Deserialize)]
struct SectionDto {
    /// A string in the payload, unlike every other id Plex emits —
    /// including `Location.id` in the very same object, which is a
    /// number. Kept as text rather than parsed and re-formatted.
    key: String,
    title: String,
    #[serde(rename = "Location", default)]
    location: Vec<LocationDto>,
}

#[derive(Debug, Deserialize)]
struct LocationDto {
    path: String,
}

#[derive(Debug, Deserialize)]
struct IdentityPayload {
    #[serde(rename = "MediaContainer")]
    media_container: IdentityContainer,
}

#[derive(Debug, Deserialize)]
struct IdentityContainer {
    #[serde(default)]
    version: Option<String>,
}
