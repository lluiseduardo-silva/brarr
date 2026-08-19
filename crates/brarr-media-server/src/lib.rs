//! `brarr-media-server` — telling Plex, Jellyfin and Emby that the
//! library changed.
//!
//! This is the last thing the \*arr did that brarr had not taken over.
//! Radarr and Sonarr call it *Connect → Plex Media Server* and *Connect →
//! Emby/Jellyfin*; without it a file brarr just imported stays invisible
//! until the media server happens to scan on its own.
//!
//! ## Three kinds, two dialects
//!
//! | Kind | API | Auth | "the library changed" |
//! |------|-----|------|-----------------------|
//! | Plex | Plex | `X-Plex-Token` | `GET library/sections/{id}/refresh?path=…` |
//! | Jellyfin | `MediaBrowser` | `X-MediaBrowser-Token` | `POST /Library/Media/Updated` |
//! | Emby | `MediaBrowser` | `X-MediaBrowser-Token` | `POST /Library/Media/Updated` |
//!
//! Jellyfin and Emby share one implementation because they share one API
//! — read from the Sonarr source, where `Notifications/Jellyfin/` does
//! not exist and `MediaBrowserSettings` carries no server-type field.
//! [`MediaServerKind`] still names all three, because the operator has to
//! say what they are pointing at; which dialect that implies is derived
//! by [`MediaServerKind::api`] and never stored, the same rule the
//! download clients apply to their transport.
//!
//! ## The path is the address
//!
//! Neither dialect takes "rescan everything". Both are told *where*
//! something changed, and the path has to be written the way the media
//! server sees it — which is generally not the way brarr sees it, since
//! the two mount the same directory in different places. Translation is
//! the caller's job (the orchestrator owns the mapping rules); this crate
//! takes paths already in the server's namespace.
//!
//! For Plex there is one more step, and it is the one that is easy to get
//! wrong: the refresh is addressed to a *section*, and picking the
//! section by media type breaks the moment a library has two sections of
//! the same type — which is the common case, since separate `Animes` and
//! `Series` shelves are both `show`. [`plex`] picks by path instead.

mod error;
mod media_browser;
pub mod plex;

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use url::Url;

pub use error::MediaServerError;
pub use media_browser::MediaBrowserClient;
pub use plex::{PlexClient, PlexIdentity, PlexPin};

/// Heap-allocated future returned by [`MediaServer`] methods.
///
/// Native `async fn` in trait is stable, but the resulting trait is not
/// `dyn`-compatible — and the orchestrator wants to hold whatever the
/// operator configured as a `Box<dyn MediaServer>`. Same trade-off, and
/// the same shape, as `brarr_core::TrackerProvider`.
pub type ServerFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Which media server a row is configured for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaServerKind {
    /// Plex Media Server.
    Plex,
    /// Jellyfin.
    Jellyfin,
    /// Emby.
    Emby,
}

impl MediaServerKind {
    /// Persisted label — matches the `media_servers.kind` CHECK
    /// constraint.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Plex => "plex",
            Self::Jellyfin => "jellyfin",
            Self::Emby => "emby",
        }
    }

    /// Display name as the vendor spells it, for the admin UI.
    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Plex => "Plex",
            Self::Jellyfin => "Jellyfin",
            Self::Emby => "Emby",
        }
    }

    /// Parse from the persisted label.
    #[must_use]
    pub fn from_label(s: &str) -> Option<Self> {
        match s {
            "plex" => Some(Self::Plex),
            "jellyfin" => Some(Self::Jellyfin),
            "emby" => Some(Self::Emby),
            _ => None,
        }
    }

    /// Which HTTP dialect this kind speaks.
    ///
    /// Derived, never stored — a column would be a second source of truth
    /// able to disagree with `kind`, and the two can never legitimately
    /// differ.
    #[must_use]
    pub fn api(self) -> MediaServerApi {
        match self {
            Self::Plex => MediaServerApi::Plex,
            Self::Jellyfin | Self::Emby => MediaServerApi::MediaBrowser,
        }
    }

    /// `true` when this kind authenticates through the plex.tv PIN flow
    /// rather than a key the operator pastes.
    #[must_use]
    pub fn uses_plex_login(self) -> bool {
        matches!(self, Self::Plex)
    }

    /// Every kind, in admin-UI order. Lets callers build a `<select>`
    /// without hard-coding the list a second time.
    #[must_use]
    pub fn all() -> [Self; 3] {
        [Self::Plex, Self::Jellyfin, Self::Emby]
    }
}

impl fmt::Display for MediaServerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// The HTTP dialect behind a [`MediaServerKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaServerApi {
    /// Plex's own API.
    Plex,
    /// The Emby API, which Jellyfin forked and still speaks.
    MediaBrowser,
}

impl MediaServerApi {
    /// Short label, for logs and the admin UI.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Plex => "plex",
            Self::MediaBrowser => "mediabrowser",
        }
    }
}

/// Everything needed to talk to one configured server.
#[derive(Debug, Clone)]
pub struct MediaServerConfig {
    /// Operator-chosen display name.
    pub name: String,
    /// Which server this is.
    pub kind: MediaServerKind,
    /// Base URL, including any reverse-proxy path prefix.
    pub base_url: Url,
    /// `X-Plex-Token` or `X-MediaBrowser-Token`, depending on the kind.
    pub token: Option<String>,
}

/// One library the server serves, and where it lives on the server's
/// filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Library {
    /// Server-side identifier. A Plex section `key` — which is a
    /// **string** in the payload even though every other id there is a
    /// number, so it is kept as one rather than parsed and re-formatted.
    pub id: String,
    /// Operator-visible name.
    pub title: String,
    /// Directories this library is built from, as the server writes them.
    pub locations: Vec<String>,
}

/// What a healthy [`MediaServer::test_connection`] reports back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStatus {
    /// Version string as the server spells it. Empty when the server
    /// answered but exposed no version.
    pub version: String,
    /// Libraries the credential can see.
    ///
    /// Fetched on both dialects even though only Plex needs it to address
    /// a refresh: listing libraries is an administrative call on
    /// Emby/Jellyfin too, so a key that cannot do it is a key that will
    /// fail the real work later. Better on the button than on the first
    /// import.
    pub libraries: Vec<Library>,
}

/// Something brarr can tell about a change to the library.
///
/// Two implementations ([`PlexClient`], [`MediaBrowserClient`]), which is
/// what justifies the trait — a single-impl trait would be ceremony.
pub trait MediaServer: Send + Sync {
    /// Operator-chosen display name.
    fn name(&self) -> &str;

    /// Which server this talks to.
    fn kind(&self) -> MediaServerKind;

    /// Prove the credential and read back the version and libraries.
    ///
    /// Success means the token was accepted, not merely that the host
    /// answered — see the note on [`MediaServerError::Auth`].
    ///
    /// # Errors
    ///
    /// - [`MediaServerError::Auth`] when the token is refused.
    /// - [`MediaServerError::Transport`] when the host is unreachable.
    /// - [`MediaServerError::Http`] on an unexpected status code.
    /// - [`MediaServerError::Decode`] when the payload does not parse.
    fn test_connection(&self) -> ServerFuture<'_, Result<ServerStatus, MediaServerError>>;

    /// Tell the server that `paths` changed.
    ///
    /// Each entry is a directory **in the server's namespace** — the
    /// caller has already translated it. Granularity is the title's
    /// folder, never the individual file, which is what both \*arr send.
    ///
    /// # Errors
    ///
    /// - [`MediaServerError::NoMatchingLibrary`] when no configured
    ///   library covers a path (Plex only — the `MediaBrowser` dialect
    ///   resolves the path server-side).
    /// - [`MediaServerError::Auth`] when the token is refused.
    /// - [`MediaServerError::Transport`] when the host is unreachable.
    fn notify_updated<'a>(
        &'a self,
        paths: &'a [String],
    ) -> ServerFuture<'a, Result<(), MediaServerError>>;
}

/// Build the client matching `config.kind`.
///
/// # Errors
///
/// - [`MediaServerError::Config`] when the token the kind requires is
///   missing.
/// - [`MediaServerError::Transport`] if the TLS backend fails to
///   instantiate (system-level, rare).
pub fn build(config: MediaServerConfig) -> Result<Box<dyn MediaServer>, MediaServerError> {
    match config.kind.api() {
        MediaServerApi::Plex => Ok(Box::new(PlexClient::new(config)?)),
        MediaServerApi::MediaBrowser => Ok(Box::new(MediaBrowserClient::new(config)?)),
    }
}

/// Join `path` onto a base URL, preserving any path prefix the operator
/// configured.
///
/// A base of `https://host/jellyfin` (no trailing slash) has to become
/// `https://host/jellyfin/System/Info`, not `https://host/System/Info` —
/// which is what [`Url::join`] does on its own, because it treats the
/// last segment as a file name and replaces it.
fn endpoint(base: &Url, path: &str) -> Result<Url, url::ParseError> {
    let mut base = base.clone();
    if !base.path().ends_with('/') {
        let mut with_slash = base.path().to_owned();
        with_slash.push('/');
        base.set_path(&with_slash);
    }
    base.join(path)
}

/// Shared HTTP client builder, so both dialects get the same timeout.
fn http_client(kind: MediaServerKind) -> Result<reqwest::Client, MediaServerError> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|source| MediaServerError::Transport { kind, source })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests assert on happy paths")]

    use super::*;

    #[test]
    fn kind_labels_round_trip() {
        for kind in MediaServerKind::all() {
            assert_eq!(MediaServerKind::from_label(kind.label()), Some(kind));
        }
        assert_eq!(MediaServerKind::from_label("kodi"), None);
    }

    #[test]
    fn jellyfin_and_emby_share_a_dialect_and_plex_does_not() {
        assert_eq!(
            MediaServerKind::Jellyfin.api(),
            MediaServerApi::MediaBrowser
        );
        assert_eq!(MediaServerKind::Emby.api(), MediaServerApi::MediaBrowser);
        assert_eq!(MediaServerKind::Plex.api(), MediaServerApi::Plex);
    }

    #[test]
    fn only_plex_signs_in_through_plex_tv() {
        assert!(MediaServerKind::Plex.uses_plex_login());
        assert!(!MediaServerKind::Jellyfin.uses_plex_login());
        assert!(!MediaServerKind::Emby.uses_plex_login());
    }

    #[test]
    fn endpoint_keeps_a_reverse_proxy_path_prefix() {
        let base = Url::parse("https://host.example/jellyfin").unwrap();
        assert_eq!(
            endpoint(&base, "System/Info").unwrap().as_str(),
            "https://host.example/jellyfin/System/Info",
            "a missing trailing slash must not swallow the prefix"
        );
    }
}
