#![allow(
    clippy::doc_markdown,
    reason = "qBittorrent and SABnzbd are product names, written the way their vendors write them; backticking every mention would make the prose unreadable"
)]

//! `brarr-download-client` — the programs that actually move the bytes.
//!
//! Until now the pipeline ended at `POST /api/v3/release/push`: brarr
//! scored a release and handed it to Radarr/Sonarr, which handed it to
//! *their* download client. That hand-off is what made the double grab
//! possible — the \*arr also runs its own indexer search, so two
//! independent pipelines converge on the same client and neither knows
//! about the other. Talking to the download client directly is what
//! makes brarr the only agent.
//!
//! ## Two clients, one per protocol
//!
//! | Kind | Protocol | Auth |
//! |------|----------|------|
//! | qBittorrent | torrent | `POST api/v2/auth/login` → `SID` cookie |
//! | SABnzbd | usenet | `?apikey=` query parameter |
//!
//! One each covers the provider families that already exist (UNIT3D and
//! Torznab on the torrent side, Newznab on the usenet side). Transmission
//! and Deluge are deliberately out of the first cut.
//!
//! ## Both can refuse credentials inside a `200 OK`
//!
//! Neither client is REST-shaped about auth. qBittorrent answers the
//! login POST with the literal body `Ok.` or `Fails.` — status `200`
//! either way — and SABnzbd answers `{"status": false, "error": "API Key
//! Incorrect"}`, also `200`. Checking `resp.status()` alone would report
//! a wrong password as a healthy connection, which is why
//! [`DownloadClientError::Auth`] exists as its own variant and both
//! implementations look inside the body before declaring success.
//!
//! ## Scope of this cut
//!
//! [`DownloadClient::test_connection`] only. Handing a `.torrent`/`.nzb`
//! over and following the queue is the next step, and it reuses
//! everything here — the qBittorrent session in particular, since every
//! subsequent call carries the same cookie.

mod error;
mod infohash;
mod qbittorrent;
mod sabnzbd;

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use url::Url;

pub use error::DownloadClientError;
pub use qbittorrent::QbittorrentClient;
pub use sabnzbd::SabnzbdClient;

/// Heap-allocated future returned by [`DownloadClient`] methods.
///
/// Native `async fn` in trait is stable, but the resulting trait is not
/// `dyn`-compatible — and the orchestrator wants to hold whatever the
/// operator configured as a `Box<dyn DownloadClient>`. Same trade-off,
/// and the same shape, as `brarr_core::TrackerProvider`.
pub type ClientFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Which download program a row is configured for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DownloadClientKind {
    /// qBittorrent, over its WebUI API v2 (qBittorrent 4.1+).
    Qbittorrent,
    /// SABnzbd, over its `/api` query interface.
    Sabnzbd,
}

impl DownloadClientKind {
    /// Persisted label — matches the `download_clients.kind` CHECK
    /// constraint.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Qbittorrent => "qbittorrent",
            Self::Sabnzbd => "sabnzbd",
        }
    }

    /// Display name as the program spells it, for the admin UI.
    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Qbittorrent => "qBittorrent",
            Self::Sabnzbd => "SABnzbd",
        }
    }

    /// Parse from the persisted label.
    #[must_use]
    pub fn from_label(s: &str) -> Option<Self> {
        match s {
            "qbittorrent" => Some(Self::Qbittorrent),
            "sabnzbd" => Some(Self::Sabnzbd),
            _ => None,
        }
    }

    /// Transport this kind serves. A function of the kind rather than a
    /// stored column — a copy in the database is one more thing that can
    /// disagree with itself.
    #[must_use]
    pub fn protocol(self) -> Protocol {
        match self {
            Self::Qbittorrent => Protocol::Torrent,
            Self::Sabnzbd => Protocol::Usenet,
        }
    }

    /// Every kind, in admin-UI order. Lets callers build a `<select>`
    /// without hard-coding the list a second time.
    #[must_use]
    pub fn all() -> [Self; 2] {
        [Self::Qbittorrent, Self::Sabnzbd]
    }
}

impl fmt::Display for DownloadClientKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// Transport a release travels over. Mirrors the orchestrator's
/// `grabs.protocol` column; kept crate-local so this crate does not
/// depend on the orchestrator (the dependency runs the other way).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
    /// `BitTorrent` — UNIT3D trackers and Torznab indexers.
    Torrent,
    /// Usenet — Newznab indexers.
    Usenet,
}

impl Protocol {
    /// Persisted label, matching `grabs.protocol`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Torrent => "torrent",
            Self::Usenet => "usenet",
        }
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Static configuration for one download client.
///
/// Borrowed-style like `brarr_arr::ArrInstance`: the orchestrator owns
/// the canonical row and hands a snapshot over. Which credential fields
/// matter depends on [`Self::kind`] — see [`DownloadClientKind`].
#[derive(Debug, Clone)]
pub struct DownloadClientConfig {
    /// Operator-chosen display name (`"qbittorrent-main"`). Used in logs
    /// and the admin UI.
    pub name: String,
    /// Which program this is.
    pub kind: DownloadClientKind,
    /// Base URL of the web interface (`http://10.0.1.246:8080/`). A
    /// path prefix is honoured, so a reverse-proxied
    /// `https://host/sabnzbd/` works.
    pub base_url: Url,
    /// qBittorrent WebUI username. Ignored by SABnzbd.
    pub username: Option<String>,
    /// qBittorrent WebUI password. Ignored by SABnzbd.
    pub password: Option<String>,
    /// SABnzbd apikey (Config → General). Ignored by qBittorrent.
    pub api_key: Option<String>,
    /// Category / label to file downloads under. `None` leaves the
    /// client's own default in place. Unused until releases are actually
    /// handed over, but part of the configuration the operator edits.
    pub category: Option<String>,
}

/// What a healthy [`DownloadClient::test_connection`] reports back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientStatus {
    /// Version string as the client spells it (`"v5.0.4"`, `"4.3.2"`).
    /// Empty when the client answered but exposed no version.
    pub version: String,
}

/// The release itself, as handed to [`DownloadClient::add`].
///
/// brarr fetches the `.torrent` / `.nzb` rather than passing the
/// provider URL through for the client to fetch. It costs one request,
/// and it buys the difference between "the tracker refused brarr's
/// token" and a torrent that silently sits in the client erroring — the
/// client would answer `Ok.` either way.
#[derive(Debug, Clone, Copy)]
pub enum ReleaseFile<'a> {
    /// Bytes brarr already fetched and validated.
    Bytes(&'a [u8]),
    /// A magnet URI. Nothing to fetch — the client resolves it itself.
    /// Torrent-only; a Usenet client rejects it.
    Magnet(&'a str),
}

/// What the client answered when it accepted a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedRelease {
    /// Client-side handle for the download.
    ///
    /// SABnzbd names what it queued (`nzo_id`). qBittorrent answers a
    /// bare `Ok.` and names nothing — so the torrent side computes the
    /// **infohash** from the file it just uploaded, which is the same
    /// identity qBittorrent itself keys on and the same one Radarr uses.
    /// `None` only when the release could not be identified at all (a
    /// magnet spelling its hash in base32, say); the download still runs,
    /// it just cannot be followed.
    pub client_item_id: Option<String>,
}

/// Where a download has got to, as the client sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadState {
    /// Accepted but not started — queued, paused, or waiting on
    /// propagation.
    Queued,
    /// Bytes are moving, or the client is checking/moving files.
    Downloading,
    /// The download itself is finished. For a torrent that includes
    /// seeding, which never ends on its own.
    Completed,
    /// The client gave up. [`DownloadStatus::detail`] carries its reason.
    Failed,
}

/// One download's progress. Everything except [`Self::state`] is
/// optional because the two clients expose different subsets — SABnzbd
/// reports speed only for the queue as a whole, so a per-item figure
/// would be an invention.
#[derive(Debug, Clone, PartialEq)]
pub struct DownloadStatus {
    /// Lifecycle position.
    pub state: DownloadState,
    /// `0.0..=1.0`.
    pub progress: f32,
    /// Total size in bytes.
    pub size_bytes: Option<u64>,
    /// Current download rate in bytes per second.
    pub speed_bytes: Option<u64>,
    /// Seconds remaining, as the client estimates it.
    pub eta_seconds: Option<u64>,
    /// Where the files are landing. The import phase will need this.
    pub save_path: Option<String>,
    /// Client-supplied explanation, mostly for [`DownloadState::Failed`].
    pub detail: Option<String>,
}

/// Something brarr can hand a release to.
///
/// Two implementations ([`QbittorrentClient`], [`SabnzbdClient`]), which
/// is what justifies the trait — a single-impl trait would be ceremony.
pub trait DownloadClient: Send + Sync {
    /// Operator-chosen display name.
    fn name(&self) -> &str;

    /// Which program this client speaks to.
    fn kind(&self) -> DownloadClientKind;

    /// Authenticate and read back the client's version.
    ///
    /// Success means the credentials were accepted, not merely that the
    /// host answered — see the crate-level note on `200 OK` refusals.
    ///
    /// # Errors
    ///
    /// - [`DownloadClientError::Auth`] when the credentials are refused.
    /// - [`DownloadClientError::Transport`] when the host is unreachable.
    /// - [`DownloadClientError::Http`] on an unexpected status code.
    fn test_connection(&self) -> ClientFuture<'_, Result<ClientStatus, DownloadClientError>>;

    /// Hand a release over to the client to download.
    ///
    /// `name` is the release title, used for the upload's file name and
    /// as the download's display name where the client accepts one. The
    /// configured [`DownloadClientConfig::category`] is applied here.
    ///
    /// Returning `Ok` means the client accepted the release — again, not
    /// merely that it answered.
    ///
    /// # Errors
    ///
    /// - [`DownloadClientError::Auth`] when the credentials are refused.
    /// - [`DownloadClientError::Http`] when the client rejects the file
    ///   (an invalid torrent, a truncated nzb).
    /// - [`DownloadClientError::Config`] when the file kind cannot go to
    ///   this client at all — a magnet handed to SABnzbd.
    /// - [`DownloadClientError::Transport`] when the host is unreachable.
    fn add<'a>(
        &'a self,
        name: &'a str,
        file: ReleaseFile<'a>,
    ) -> ClientFuture<'a, Result<AddedRelease, DownloadClientError>>;

    /// Ask the client how one download is going.
    ///
    /// `client_item_id` is whatever [`AddedRelease::client_item_id`]
    /// carried — an infohash for torrents, an `nzo_id` for usenet.
    ///
    /// `Ok(None)` means the client does not know this download: it was
    /// removed, or it has not appeared yet. Both clients take a moment
    /// to list something they just accepted, so a single `None` is not
    /// evidence that the download is gone.
    ///
    /// # Errors
    ///
    /// - [`DownloadClientError::Auth`] when the credentials are refused.
    /// - [`DownloadClientError::Transport`] when the host is unreachable.
    /// - [`DownloadClientError::Decode`] when the payload does not parse.
    fn status<'a>(
        &'a self,
        client_item_id: &'a str,
    ) -> ClientFuture<'a, Result<Option<DownloadStatus>, DownloadClientError>>;
}

/// Build the client matching `config.kind`.
///
/// # Errors
///
/// - [`DownloadClientError::Config`] when a credential the kind requires
///   is missing.
/// - [`DownloadClientError::Transport`] if the TLS backend fails to
///   instantiate (system-level, rare).
pub fn build(config: DownloadClientConfig) -> Result<Box<dyn DownloadClient>, DownloadClientError> {
    match config.kind {
        DownloadClientKind::Qbittorrent => Ok(Box::new(QbittorrentClient::new(config)?)),
        DownloadClientKind::Sabnzbd => Ok(Box::new(SabnzbdClient::new(config)?)),
    }
}

/// Join `path` onto a base URL, preserving any path prefix the operator
/// configured.
///
/// A base of `https://host/sabnzbd` (no trailing slash) has to become
/// `https://host/sabnzbd/api`, not `https://host/api` — which is what
/// [`Url::join`] does on its own, because it treats the last segment as
/// a file name and replaces it.
fn endpoint(base: &Url, path: &str) -> Result<Url, url::ParseError> {
    let mut base = base.clone();
    if !base.path().ends_with('/') {
        let mut with_slash = base.path().to_owned();
        with_slash.push('/');
        base.set_path(&with_slash);
    }
    base.join(path)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests assert on happy paths")]

    use super::*;

    #[test]
    fn kind_labels_round_trip() {
        for kind in DownloadClientKind::all() {
            assert_eq!(DownloadClientKind::from_label(kind.label()), Some(kind));
        }
        assert_eq!(DownloadClientKind::from_label("transmission"), None);
    }

    #[test]
    fn protocol_follows_from_kind() {
        assert_eq!(
            DownloadClientKind::Qbittorrent.protocol(),
            Protocol::Torrent
        );
        assert_eq!(DownloadClientKind::Sabnzbd.protocol(), Protocol::Usenet);
    }

    #[test]
    fn endpoint_keeps_a_reverse_proxy_path_prefix() {
        let base = Url::parse("https://host.example/sabnzbd").unwrap();
        assert_eq!(
            endpoint(&base, "api").unwrap().as_str(),
            "https://host.example/sabnzbd/api",
            "a missing trailing slash must not swallow the prefix"
        );
    }

    #[test]
    fn endpoint_handles_a_trailing_slash_and_a_bare_host() {
        let base = Url::parse("http://10.0.1.246:8080/").unwrap();
        assert_eq!(
            endpoint(&base, "api/v2/app/version").unwrap().as_str(),
            "http://10.0.1.246:8080/api/v2/app/version"
        );
    }
}
