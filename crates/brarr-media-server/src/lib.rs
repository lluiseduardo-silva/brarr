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

    /// Tell the server that these places changed.
    ///
    /// Granularity is the title's folder, never the individual file,
    /// which is what both \*arr send. Each [`LibraryUpdate`] carries both
    /// the translated absolute path and the tail relative to brarr's
    /// root, so [`resolve_targets`] can address a library exactly when a
    /// mapping makes that possible and re-anchor when it does not.
    ///
    /// # Errors
    ///
    /// - [`MediaServerError::NoMatchingLibrary`] when the server serves
    ///   no library at all — there is then nowhere to point.
    /// - [`MediaServerError::Auth`] when the token is refused.
    /// - [`MediaServerError::Transport`] when the host is unreachable.
    fn notify_updated<'a>(
        &'a self,
        updates: &'a [LibraryUpdate],
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

/// One place that changed, in enough detail to address any server.
///
/// Carrying the **relative** tail alongside the absolute path is what
/// lets this work with no configuration at all, and it is the trick both
/// \*arr use: `UpdateSectionPath` builds
/// `location.Path + separator + relativePath` and **never sends its own
/// absolute path**. The prefix is discarded and the tail is re-anchored
/// on whatever the media server says its own folder is — so a Sonarr
/// that sees `/data/Series` and a Plex that sees `/mnt/midias/Series`
/// agree without anybody typing a mapping.
///
/// brarr keeps the absolute path too, because when a mapping *is*
/// configured it can address exactly one library instead of guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryUpdate {
    /// The title's folder as brarr names it, already translated into the
    /// server's namespace by whatever mapping rules exist.
    pub path: String,
    /// The same folder relative to brarr's root — `Fringe`, or
    /// `Pantera Negra (2018)`.
    pub relative: String,
    /// The root's own last component — `Series`, `Filmes`. Used to
    /// prefer the library that is obviously the right shelf before
    /// falling back to all of them.
    pub root_name: String,
}

/// Where to tell a server to look, given what it says it serves.
///
/// Three tiers, and the order is the whole design:
///
/// 1. **A library covers the translated path** — address it directly.
///    One call, no guessing. This is what a path mapping buys.
/// 2. **Nothing covers it, but a library's folder ends in the same name
///    as brarr's root** (`…/Filmes` for a root called `Filmes`) —
///    re-anchor the tail there. Still one call, still no configuration.
/// 3. **Not even that** — re-anchor onto every location, which is
///    exactly what Radarr and Sonarr do. Some of those name a directory
///    that does not exist in that library; the server ignores them.
///
/// Returns one entry per `(library, path)` to notify.
#[must_use]
pub fn resolve_targets<'a>(
    libraries: &'a [Library],
    update: &LibraryUpdate,
) -> Vec<(&'a Library, String)> {
    if let Some(library) = pick_library(libraries, &update.path) {
        return vec![(library, update.path.clone())];
    }
    let named: Vec<(&Library, String)> = libraries
        .iter()
        .flat_map(|library| {
            library
                .locations
                .iter()
                .filter(|location| ends_with_component(location, &update.root_name))
                .map(move |location| (library, rebase(location, &update.relative)))
        })
        .collect();
    if !named.is_empty() {
        return named;
    }
    libraries
        .iter()
        .flat_map(|library| {
            library
                .locations
                .iter()
                .map(move |location| (library, rebase(location, &update.relative)))
        })
        .collect()
}

/// `location` + the server's own separator + `relative`.
///
/// The separator is read from the location, never from the host running
/// brarr — a Linux brarr feeding a Plex on Windows has to write
/// backslashes, and the location is the only thing that knows.
fn rebase(location: &str, relative: &str) -> String {
    let separator = if location.contains('\\') { '\\' } else { '/' };
    let trimmed = location.trim_end_matches(['/', '\\']);
    let tail = relative.replace(['/', '\\'], &separator.to_string());
    if tail.is_empty() {
        return trimmed.to_owned();
    }
    format!("{trimmed}{separator}{tail}")
}

/// Whether `location`'s last component is exactly `name`.
fn ends_with_component(location: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let trimmed = location.trim_end_matches(['/', '\\']);
    let last = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    if trimmed.contains('\\') {
        last.eq_ignore_ascii_case(name)
    } else {
        last == name
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
pub(crate) fn known_locations(libraries: &[Library]) -> String {
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
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "tests assert on happy paths"
    )]

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

    fn update(path: &str, relative: &str, root: &str) -> LibraryUpdate {
        LibraryUpdate {
            path: path.to_owned(),
            relative: relative.to_owned(),
            root_name: root.to_owned(),
        }
    }

    #[test]
    fn a_mapped_path_addresses_exactly_one_library() {
        let sections = real_sections();
        let targets = resolve_targets(
            &sections,
            &update("/mnt/midias/Series/Fringe", "Fringe", "Series"),
        );
        assert_eq!(targets.len(), 1, "tier 1 does not guess");
        assert_eq!(targets[0].0.id, "2");
        assert_eq!(targets[0].1, "/mnt/midias/Series/Fringe");
    }

    #[test]
    fn an_unmapped_path_is_re_anchored_onto_the_shelf_with_the_same_name() {
        // This is the case that made the *arr work with no configuration
        // at all, and that brarr used to refuse: brarr writes
        // `/midias/Series/...`, Plex only knows `/mnt/midias/...`.
        let sections = real_sections();
        let targets = resolve_targets(
            &sections,
            &update("/midias/Series/Fringe", "Fringe", "Series"),
        );
        assert_eq!(
            targets.len(),
            1,
            "the root is called Series and exactly one shelf ends in Series"
        );
        assert_eq!(targets[0].0.id, "2");
        assert_eq!(
            targets[0].1, "/mnt/midias/Series/Fringe",
            "the absolute prefix is discarded and the tail re-anchored, which is              literally what UpdateSectionPath builds"
        );
    }

    #[test]
    fn a_root_matching_no_shelf_name_falls_back_to_every_shelf() {
        // Radarr's own behaviour, and the reason it is survivable: the
        // paths that name a directory the shelf does not have are simply
        // ignored by the server.
        let sections = real_sections();
        let targets = resolve_targets(
            &sections,
            &update("/midias/Cartoons/Ducktales", "Ducktales", "Cartoons"),
        );
        assert_eq!(targets.len(), 3);
        let paths: Vec<&str> = targets.iter().map(|(_, p)| p.as_str()).collect();
        assert!(paths.contains(&"/mnt/midias/Series/Ducktales"));
        assert!(paths.contains(&"/mnt/midias/Filmes/Ducktales"));
    }

    #[test]
    fn re_anchoring_writes_the_far_sides_separator() {
        let windows = vec![library("1", "Movies", &[r"C:\Media\Movies"])];
        let targets = resolve_targets(
            &windows,
            &update("/midias/Filmes/Heat (1995)", "Heat (1995)", "Movies"),
        );
        assert_eq!(targets[0].1, r"C:\Media\Movies\Heat (1995)");
    }

    #[test]
    fn a_nested_tail_keeps_its_shape() {
        let sections = real_sections();
        let targets = resolve_targets(
            &sections,
            &update("/midias/Series/Fringe", "Fringe/Season 02", "Series"),
        );
        assert_eq!(targets[0].1, "/mnt/midias/Series/Fringe/Season 02");
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
