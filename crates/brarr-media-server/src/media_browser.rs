//! The `MediaBrowser` dialect — Emby, and Jellyfin, which forked it.
//!
//! One implementation for two products because there is one API. Read
//! from the Sonarr source: `Notifications/Jellyfin/` does not exist,
//! `MediaBrowserSettings` carries no server-type field, and the notifier
//! is simply named `"Emby / Jellyfin"`.
//!
//! ## The header is `X-MediaBrowser-Token`
//!
//! Not `X-Emby-Token`, which is the name everyone reaches for and which
//! appears nowhere in either \*arr. Sonarr sends the legacy header *and*
//! `Authorization: MediaBrowser Token="…"`; Radarr sends only the first
//! and survives because the alias has not been removed yet. Following
//! Sonarr costs one header and removes the day that alias goes away.
//!
//! ## Nothing here has to find a library
//!
//! `POST /Library/Media/Updated` takes a path and the server works out
//! which library owns it — unlike Plex, where the refresh is addressed to
//! a section and brarr has to pick one. Libraries are still listed on the
//! connection test: it is an administrative call, so a key that cannot
//! make it is a key that will fail the real work later, and finding that
//! out on the button beats finding out on the first import.

use std::collections::BTreeSet;

use serde::Deserialize;
use tracing::debug;
use url::Url;

use crate::error::truncate_body;
use crate::{
    Library, LibraryUpdate, MediaServer, MediaServerConfig, MediaServerError, MediaServerKind,
    ServerFuture, ServerStatus, endpoint, http_client, resolve_targets,
};

/// What Emby/Jellyfin call a change that added a file.
///
/// Sonarr also sends `Modified` on rename and `Deleted` on removal; brarr
/// only reports new files, so this is the only one in use.
const UPDATE_TYPE_CREATED: &str = "Created";

/// Emby or Jellyfin, over the `MediaBrowser` API.
#[derive(Debug)]
pub struct MediaBrowserClient {
    config: MediaServerConfig,
    token: String,
    http: reqwest::Client,
}

impl MediaBrowserClient {
    /// Build a client from stored configuration.
    ///
    /// # Errors
    ///
    /// - [`MediaServerError::Config`] when no API key is stored.
    /// - [`MediaServerError::Transport`] if the TLS backend fails to
    ///   instantiate.
    pub fn new(config: MediaServerConfig) -> Result<Self, MediaServerError> {
        let kind = config.kind;
        let token = config
            .token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| MediaServerError::Config {
                kind,
                detail: format!(
                    "o {kind} precisa de uma API key (painel → Avançado → Chaves de API)"
                ),
            })?
            .to_owned();
        let http = http_client(kind)?;
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

    fn kind(&self) -> MediaServerKind {
        self.config.kind
    }

    /// Attach the credential the way Sonarr does — both headers.
    fn authorized(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.header("X-MediaBrowser-Token", &self.token)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("MediaBrowser Token=\"{}\"", self.token),
            )
            .header(reqwest::header::ACCEPT, "application/json")
    }

    /// Run a request and turn a refusal into [`MediaServerError::Auth`].
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<String, MediaServerError> {
        let kind = self.kind();
        let resp = self
            .authorized(req)
            .send()
            .await
            .map_err(|source| MediaServerError::Transport { kind, source })?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|source| MediaServerError::Transport { kind, source })?;
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(MediaServerError::Auth {
                kind,
                detail: format!("o {kind} recusou a API key"),
            });
        }
        if !status.is_success() {
            return Err(MediaServerError::Http {
                kind,
                status: status.as_u16(),
                body: truncate_body(&body),
            });
        }
        Ok(body)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, MediaServerError> {
        let url = self.endpoint(path)?;
        debug!(
            target: "brarr_media_server::media_browser",
            name = %self.config.name,
            path,
            "mediabrowser call"
        );
        let body = self.send(self.http.get(url)).await?;
        serde_json::from_str(&body).map_err(|source| MediaServerError::Decode {
            kind: self.kind(),
            source,
        })
    }

    /// Libraries the key can see, with the directories behind each.
    ///
    /// # Errors
    ///
    /// Propagates from [`Self::send`].
    pub async fn virtual_folders(&self) -> Result<Vec<Library>, MediaServerError> {
        let folders: Vec<VirtualFolderDto> = self.get_json("Library/VirtualFolders").await?;
        Ok(folders
            .into_iter()
            .map(|f| Library {
                id: f.item_id.unwrap_or_default(),
                title: f.name.unwrap_or_default(),
                locations: f.locations,
            })
            .collect())
    }

    fn endpoint(&self, path: &str) -> Result<Url, MediaServerError> {
        Ok(endpoint(&self.config.base_url, path)?)
    }
}

impl MediaServer for MediaBrowserClient {
    fn name(&self) -> &str {
        &self.config.name
    }

    fn kind(&self) -> MediaServerKind {
        self.config.kind
    }

    fn test_connection(&self) -> ServerFuture<'_, Result<ServerStatus, MediaServerError>> {
        Box::pin(async move {
            // `System/Info` and not `System/Info/Public`: the public one
            // answers without a key, so a green badge from it would prove
            // nothing — the same trap Plex sets with `/identity`.
            let info: SystemInfoDto = self.get_json("System/Info").await?;
            let libraries = self.virtual_folders().await?;
            Ok(ServerStatus {
                version: info.version.unwrap_or_default(),
                libraries,
            })
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
            // Libraries are listed here too, which this deliberately did
            // not do before. `Library/Media/Updated` resolves a path
            // server-side, but only a path the server recognises — and
            // without a mapping brarr's own spelling is not one. Asking
            // costs a GET and buys the same zero-configuration behaviour
            // the *arr get from re-anchoring.
            let libraries = self.virtual_folders().await?;
            if libraries.is_empty() {
                return Err(MediaServerError::NoMatchingLibrary {
                    kind: self.kind(),
                    path: updates[0].path.clone(),
                    known: crate::known_locations(&libraries),
                });
            }
            let mut paths: BTreeSet<String> = BTreeSet::new();
            for update in updates {
                for (_, path) in resolve_targets(&libraries, update) {
                    paths.insert(path);
                }
            }
            // `Updates` is a list in the payload, so one request carries
            // the whole pass. Sonarr sends one element per request only
            // because its loop sits outside the proxy call.
            let updates: Vec<_> = paths
                .iter()
                .map(|path| MediaUpdate {
                    path,
                    update_type: UPDATE_TYPE_CREATED,
                })
                .collect();
            let url = self.endpoint("Library/Media/Updated")?;
            debug!(
                target: "brarr_media_server::media_browser",
                name = %self.config.name,
                paths = updates.len(),
                "telling the server the library changed"
            );
            self.send(self.http.post(url).json(&MediaUpdatedBody { updates }))
                .await?;
            Ok(())
        })
    }

    fn rescan_all(&self) -> ServerFuture<'_, Result<(), MediaServerError>> {
        Box::pin(async move {
            let url = self.endpoint("Library/Refresh")?;
            self.send(self.http.post(url)).await?;
            Ok(())
        })
    }
}

#[derive(Debug, serde::Serialize)]
struct MediaUpdatedBody<'a> {
    #[serde(rename = "Updates")]
    updates: Vec<MediaUpdate<'a>>,
}

#[derive(Debug, serde::Serialize)]
struct MediaUpdate<'a> {
    #[serde(rename = "Path")]
    path: &'a str,
    #[serde(rename = "UpdateType")]
    update_type: &'a str,
}

#[derive(Debug, Deserialize)]
struct SystemInfoDto {
    #[serde(rename = "Version", default)]
    version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VirtualFolderDto {
    #[serde(rename = "Name", default)]
    name: Option<String>,
    #[serde(rename = "ItemId", default)]
    item_id: Option<String>,
    #[serde(rename = "Locations", default)]
    locations: Vec<String>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests assert on happy paths")]

    use super::*;

    #[test]
    fn the_update_payload_has_the_shape_sonarr_sends() {
        let body = MediaUpdatedBody {
            updates: vec![MediaUpdate {
                path: "/media/Filmes/Scary Movie (2000)",
                update_type: UPDATE_TYPE_CREATED,
            }],
        };
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"Updates":[{"Path":"/media/Filmes/Scary Movie (2000)","UpdateType":"Created"}]}"#
        );
    }

    #[test]
    fn a_virtual_folder_without_an_item_id_still_parses() {
        // Jellyfin and Emby differ on which of these they fill in, and a
        // library with no id is still a library with locations.
        let dto: VirtualFolderDto =
            serde_json::from_str(r#"{"Name":"Filmes","Locations":["/media/Filmes"]}"#).unwrap();
        assert_eq!(dto.item_id, None);
        assert_eq!(dto.locations, vec!["/media/Filmes".to_owned()]);
    }
}
