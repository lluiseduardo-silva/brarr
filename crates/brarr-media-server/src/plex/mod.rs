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

use serde::Deserialize;
use tracing::debug;
use url::Url;

pub use auth::{PinState, PlexIdentity, PlexLogin, PlexPin};

use crate::error::truncate_body;
use crate::{
    Library, MediaServer, MediaServerConfig, MediaServerError, MediaServerKind, ServerFuture,
    ServerStatus, endpoint, http_client,
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
        paths: &'a [String],
    ) -> ServerFuture<'a, Result<(), MediaServerError>> {
        Box::pin(async move {
            if paths.is_empty() {
                return Ok(());
            }
            let libraries = self.sections().await?;
            // The work that can be done is done before the complaint: a
            // second title in the same pass must not be skipped because
            // the first one had no mapping.
            let mut unmatched: Option<&str> = None;
            for path in paths {
                match pick_library(&libraries, path) {
                    Some(library) => self.refresh(&library.id, path).await?,
                    None => unmatched = unmatched.or(Some(path.as_str())),
                }
            }
            if let Some(path) = unmatched {
                return Err(MediaServerError::NoMatchingLibrary {
                    kind: KIND,
                    path: path.to_owned(),
                    known: known_locations(&libraries),
                });
            }
            Ok(())
        })
    }
}

/// The library whose location contains `path`, longest location first.
///
/// Longest wins for the same reason it does in the orchestrator's path
/// mapping: with a section on `/mnt/midias` and another on
/// `/mnt/midias/Animes`, returning whichever came first would resolve by
/// the order Plex happened to list them.
#[must_use]
pub fn pick_library<'a>(libraries: &'a [Library], path: &str) -> Option<&'a Library> {
    libraries
        .iter()
        .filter_map(|library| {
            library
                .locations
                .iter()
                .filter(|location| covers(location, path))
                .map(String::len)
                .max()
                .map(|len| (len, library))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, library)| library)
}

/// `true` when `path` is `location` or sits under it.
///
/// The boundary matters: `/data/down` must not cover `/data/downloads`,
/// which a bare `starts_with` would wave through. Unlike the
/// orchestrator's `remote_path`, both sides here are written by the *same
/// machine* — the location comes from the server and the path was
/// translated into that server's namespace — so the flavour is inferred
/// once, from the location, exactly as Sonarr does
/// (`location.Path.Contains('\\') ? "\\" : "/"`).
///
/// Case folding is ASCII-only on the Windows side. A Windows share whose
/// directories differ only by the case of a non-ASCII letter would be
/// missed; no such path exists in practice, and the alternative is a
/// Unicode-folding dependency for a comparison that already has an
/// operator-visible fallback.
fn covers(location: &str, path: &str) -> bool {
    let windows = location.contains('\\');
    let trimmed = location.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        // A location of `/` covers everything, which makes it useless as
        // a discriminator and dangerous as a default. Refuse it.
        return false;
    }
    let (location, target, separator) = if windows {
        (
            trimmed.to_ascii_lowercase().replace('/', "\\"),
            path.to_ascii_lowercase().replace('/', "\\"),
            '\\',
        )
    } else {
        // POSIX splits on `/` only: a backslash is a legal filename
        // character on Linux, and `AC\DC` is one directory.
        (trimmed.to_owned(), path.to_owned(), '/')
    };
    if target == location {
        return true;
    }
    target
        .strip_prefix(&location)
        .is_some_and(|rest| rest.starts_with(separator))
}

/// Locations across every library, for the "nothing matched" message.
fn known_locations(libraries: &[Library]) -> String {
    let mut all: Vec<&str> = libraries
        .iter()
        .flat_map(|l| l.locations.iter().map(String::as_str))
        .collect();
    all.sort_unstable();
    if all.is_empty() {
        return "nenhuma".to_owned();
    }
    all.join(", ")
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "tests assert on happy paths")]

    use super::*;

    fn library(id: &str, title: &str, locations: &[&str]) -> Library {
        Library {
            id: id.to_owned(),
            title: title.to_owned(),
            locations: locations.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    /// The three sections this operator really has. Two are `show`, which
    /// is the whole reason the type cannot pick.
    fn real_sections() -> Vec<Library> {
        vec![
            library("3", "Filmes", &["/mnt/midias/Filmes"]),
            library("1", "Animes", &["/mnt/midias/Animes"]),
            library("2", "Series", &["/mnt/midias/Series"]),
        ]
    }

    #[test]
    fn an_episode_reaches_the_shelf_it_lives_on() {
        let sections = real_sections();
        let picked = pick_library(&sections, "/mnt/midias/Series/Fringe/Season 02")
            .expect("a section covers it");
        assert_eq!(picked.id, "2", "Series, not the other `show` section");

        let anime = pick_library(&sections, "/mnt/midias/Animes/Bleach/Season 01")
            .expect("a section covers it");
        assert_eq!(anime.id, "1");

        let movie = pick_library(&sections, "/mnt/midias/Filmes/Scary Movie (2000)")
            .expect("a section covers it");
        assert_eq!(movie.id, "3");
    }

    #[test]
    fn an_uncovered_path_picks_nothing_rather_than_the_first_section() {
        assert!(pick_library(&real_sections(), "/midias/Filmes/X").is_none());
    }

    #[test]
    fn the_longest_location_wins() {
        let sections = vec![
            library("10", "Tudo", &["/mnt/midias"]),
            library("11", "Animes", &["/mnt/midias/Animes"]),
        ];
        let picked =
            pick_library(&sections, "/mnt/midias/Animes/Bleach").expect("a section covers it");
        assert_eq!(picked.id, "11", "the specific shelf, not the catch-all");
    }

    #[test]
    fn a_prefix_has_to_end_on_a_component_boundary() {
        assert!(covers("/data/down", "/data/down/x"));
        assert!(covers("/data/down", "/data/down"));
        assert!(
            !covers("/data/down", "/data/downloads/x"),
            "a bare starts_with would wave this through"
        );
    }

    #[test]
    fn a_backslash_is_a_filename_on_posix() {
        assert!(
            !covers("/mnt/midias", "\\mnt\\midias\\Filmes"),
            "a POSIX location must not match a Windows-shaped path"
        );
        // `AC\DC` is one directory on Linux, not two.
        assert!(covers("/music/AC\\DC", "/music/AC\\DC/Back in Black"));
    }

    #[test]
    fn windows_locations_are_case_insensitive_and_take_both_separators() {
        assert!(covers("C:\\Media\\Movies", "c:/media/movies/Heat (1995)"));
        assert!(covers("\\\\NAS\\midia", "\\\\NAS\\midia\\Filmes"));
        assert!(
            !covers("\\\\NAS\\midia", "/NAS/midia/Filmes"),
            "a UNC root is a root, not a POSIX path"
        );
    }

    #[test]
    fn a_root_location_is_refused_rather_than_matching_everything() {
        assert!(!covers("/", "/anything"));
    }

    #[test]
    fn trailing_separators_on_a_location_do_not_change_the_answer() {
        assert!(covers("/mnt/midias/Series/", "/mnt/midias/Series/Fringe"));
    }

    #[test]
    fn known_locations_names_them_all_for_the_error() {
        assert_eq!(
            known_locations(&real_sections()),
            "/mnt/midias/Animes, /mnt/midias/Filmes, /mnt/midias/Series"
        );
        assert_eq!(known_locations(&[]), "nenhuma");
    }
}
