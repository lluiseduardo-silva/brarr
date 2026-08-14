//! Hand a reserved grab to a download client.
//!
//! This is the step that replaces `POST /api/v3/release/push`: brarr no
//! longer asks an \*arr to fetch the release, it fetches the file itself
//! and gives it to qBittorrent or SABnzbd directly.
//!
//! ```text
//!   grabs::reserve()  ← the barrier; already won by the caller
//!         │
//!         ▼
//!   fetch_release_file()   ── GET the .torrent/.nzb, size-capped,
//!         │                   and check it is actually one
//!         ▼
//!   pick_for_protocol()    ── which client serves this transport
//!         │
//!         ▼
//!   DownloadClient::add()
//!         │
//!         ▼
//!   grabs::mark_sent()     ── or release / fail, see below
//! ```
//!
//! ## Why brarr fetches the file instead of passing the URL along
//!
//! Both clients accept a URL and would fetch it themselves. They would
//! also answer `Ok.` for a URL that returns the tracker's HTML login
//! page, and the operator would be left with a download that never
//! starts and no error anywhere. Fetching here means a refused token is
//! a brarr error, attached to the grab, at the moment it happens.
//!
//! ## Failure is either transient or permanent, and the difference matters
//!
//! A failed hand-off has two possible remedies and they are opposites:
//!
//! - **Transient** (client down, tracker unreachable, credentials
//!   refused): the *release* is fine. The reservation is deleted, which
//!   frees the barrier key so the next sweep can try this same release
//!   again.
//! - **Permanent** (the client rejected the file, the URL is gone, no
//!   client serves this transport): trying the same release again would
//!   fail the same way. The grab is marked `failed`, which keeps the
//!   barrier key occupied — so the scanner moves on to the next-best
//!   release instead of retrying this one forever.
//!
//! Marking everything failed would blacklist a release because a client
//! was restarting; releasing everything would retry a corrupt torrent
//! every half hour. Hence the split.

use std::time::Duration;

use brarr_download_client::{DownloadClientError, ReleaseFile};
use tracing::{info, warn};

use crate::db::download_clients::{self, DownloadClientRow};
use crate::db::grabs::{self, Grab, GrabStatus, Protocol};
use crate::{AppError, AppState};

/// Ceiling on a fetched release file. A `.torrent` is kilobytes and a
/// large `.nzb` a few megabytes; anything past this is a misconfigured
/// URL pointing at something else entirely, and reading it all would be
/// the only damage done.
const MAX_RELEASE_FILE_BYTES: usize = 8 * 1024 * 1024;

/// Timeout for fetching the release file from the provider.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Result of one hand-off attempt. The grab's row is already updated to
/// match by the time this returns — the caller only reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// The client accepted the release; the grab is `sent`.
    Sent {
        /// Which client took it.
        client_name: String,
        /// Client-side handle, when the client returns one.
        client_item_id: Option<String>,
    },
    /// Failed for a reason that may not repeat. The reservation was
    /// released, so this release can be tried again.
    Retryable(String),
    /// Failed for a reason that will repeat. The grab is `failed` and
    /// keeps its barrier key, so the scanner moves to the next release.
    Permanent(String),
}

impl DeliveryOutcome {
    /// `true` for [`Self::Sent`].
    #[must_use]
    pub fn is_sent(&self) -> bool {
        matches!(self, Self::Sent { .. })
    }

    /// Operator-facing reason, empty on success.
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Self::Sent { .. } => "",
            Self::Retryable(r) | Self::Permanent(r) => r,
        }
    }
}

/// Deliver one already-reserved grab.
///
/// # Errors
///
/// Only [`AppError::Database`], and only when recording the outcome
/// fails. Everything else — a dead client, a refused token, a corrupt
/// file — is a [`DeliveryOutcome`], not an `Err`: those are results the
/// operator needs to see, not bugs to propagate.
pub async fn deliver(state: &AppState, grab: &Grab) -> Result<DeliveryOutcome, AppError> {
    // The global switch, checked here and not only in the loop: a button
    // on a screen has to be as paused as the sweep behind it. `Retryable`
    // rather than `Permanent` releases the reservation, so a pause
    // consumes no barrier key and leaves nothing to clean up after.
    if crate::db::settings::is_paused(state.pool()).await {
        return Ok(DeliveryOutcome::Retryable(
            "o brarr está pausado — nada é entregue a cliente de download".to_owned(),
        ));
    }
    let Some(transport) = protocol(grab) else {
        return record(
            state,
            grab,
            DeliveryOutcome::Permanent(
                "um arquivo local já está no disco — não há o que entregar".to_owned(),
            ),
        )
        .await;
    };
    let picked = download_clients::pick_for_protocol(state.pool(), transport).await?;
    let Some(client_row) = picked else {
        // Nothing to deliver to. Release rather than fail: the moment the
        // operator adds a client, this release becomes grabbable again,
        // and failing would have blacklisted it meanwhile.
        return record(
            state,
            grab,
            DeliveryOutcome::Retryable(format!(
                "nenhum cliente de download ativo para {}",
                grab.protocol.label()
            )),
        )
        .await;
    };

    let Some(url) = grab.download_url.as_deref() else {
        return record(
            state,
            grab,
            DeliveryOutcome::Permanent("a release não expôs URL de download".to_owned()),
        )
        .await;
    };

    // A magnet needs no fetching, and nothing about it can be validated
    // here anyway — the client resolves it.
    let fetched = if url.starts_with("magnet:") {
        None
    } else {
        match fetch_release_file(url, transport).await {
            Ok(bytes) => Some(bytes),
            Err(FetchFailure::Transient(reason)) => {
                return record(state, grab, DeliveryOutcome::Retryable(reason)).await;
            }
            Err(FetchFailure::Permanent(reason)) => {
                return record(state, grab, DeliveryOutcome::Permanent(reason)).await;
            }
        }
    };

    let file = match fetched.as_deref() {
        Some(bytes) => ReleaseFile::Bytes(bytes),
        None => ReleaseFile::Magnet(url),
    };

    let outcome = match brarr_download_client::build(client_row.to_config()) {
        Ok(client) => match client.add(&grab.release_name, file).await {
            Ok(added) => DeliveryOutcome::Sent {
                client_name: client_row.name.clone(),
                client_item_id: added.client_item_id,
            },
            Err(e) => classify_client_error(&client_row, &e),
        },
        // A client row that cannot build one (a SABnzbd row whose apikey
        // was cleared) is a configuration problem, not a bad release.
        Err(e) => DeliveryOutcome::Retryable(format!("{}: {e}", client_row.name)),
    };

    record_with_client(state, grab, &client_row, outcome).await
}

/// The transport a grab travels over, as the download-client crate
/// spells it. The two enums are deliberately separate types — see
/// `brarr_download_client::Protocol`.
///
/// `None` for [`Protocol::Local`]: an adopted file has no transport
/// because it was never downloaded. Such a grab is created already
/// `imported`, so delivery should never see one.
fn protocol(grab: &Grab) -> Option<brarr_download_client::Protocol> {
    match grab.protocol {
        Protocol::Torrent => Some(brarr_download_client::Protocol::Torrent),
        Protocol::Usenet => Some(brarr_download_client::Protocol::Usenet),
        Protocol::Local => None,
    }
}

/// Map a client error onto the retry decision. See the module docs.
fn classify_client_error(row: &DownloadClientRow, e: &DownloadClientError) -> DeliveryOutcome {
    let detail = format!("{}: {e}", row.name);
    match e {
        // The box is unreachable or the credentials are wrong: the
        // release is innocent, and blacklisting it would be wrong.
        DownloadClientError::Transport { .. }
        | DownloadClientError::Auth { .. }
        | DownloadClientError::InvalidUrl(_) => DeliveryOutcome::Retryable(detail),
        // The client looked at this file and said no.
        DownloadClientError::Http { .. }
        | DownloadClientError::Decode { .. }
        | DownloadClientError::Config { .. } => DeliveryOutcome::Permanent(detail),
    }
}

async fn record(
    state: &AppState,
    grab: &Grab,
    outcome: DeliveryOutcome,
) -> Result<DeliveryOutcome, AppError> {
    record_inner(state, grab, None, outcome).await
}

async fn record_with_client(
    state: &AppState,
    grab: &Grab,
    client: &DownloadClientRow,
    outcome: DeliveryOutcome,
) -> Result<DeliveryOutcome, AppError> {
    record_inner(state, grab, Some(client), outcome).await
}

async fn record_inner(
    state: &AppState,
    grab: &Grab,
    client: Option<&DownloadClientRow>,
    outcome: DeliveryOutcome,
) -> Result<DeliveryOutcome, AppError> {
    match &outcome {
        DeliveryOutcome::Sent { client_item_id, .. } => {
            let client_id = match client {
                Some(c) => c.id,
                // Unreachable: `Sent` is only built alongside a client.
                None => {
                    return Err(AppError::InvalidInput(
                        "delivery reported sent without a client".into(),
                    ));
                }
            };
            grabs::mark_sent(state.pool(), grab.id, client_id, client_item_id.as_deref()).await?;
            info!(
                target: "brarr_orchestrator::deliver",
                grab_id = %grab.id,
                release = %grab.release_name,
                client = client.map(|c| c.name.as_str()).unwrap_or_default(),
                client_item_id = client_item_id.as_deref().unwrap_or("-"),
                "release handed to the download client"
            );
        }
        DeliveryOutcome::Retryable(reason) => {
            warn!(
                target: "brarr_orchestrator::deliver",
                grab_id = %grab.id,
                release = %grab.release_name,
                reason = %reason,
                "hand-off failed; releasing the reservation so it can be tried again"
            );
            grabs::release_reservation(state.pool(), grab.id).await?;
        }
        DeliveryOutcome::Permanent(reason) => {
            warn!(
                target: "brarr_orchestrator::deliver",
                grab_id = %grab.id,
                release = %grab.release_name,
                reason = %reason,
                "hand-off failed for good; the next sweep will pick another release"
            );
            grabs::set_status(state.pool(), grab.id, GrabStatus::Failed, Some(reason)).await?;
        }
    }
    Ok(outcome)
}

/// Why a fetch failed, in the same two flavours as a delivery.
enum FetchFailure {
    Transient(String),
    Permanent(String),
}

/// Download the `.torrent` / `.nzb` and confirm it is one.
///
/// The URL carries whatever credential the provider needs (UNIT3D embeds
/// the token, Newznab the apikey), so this is a plain GET.
async fn fetch_release_file(
    url: &str,
    protocol: brarr_download_client::Protocol,
) -> Result<Vec<u8>, FetchFailure> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("brarr/", env!("CARGO_PKG_VERSION")))
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(|e| FetchFailure::Transient(format!("cliente HTTP: {e}")))?;

    let mut resp = http
        .get(url)
        .send()
        .await
        .map_err(|e| FetchFailure::Transient(format!("baixando a release: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        // A tracker that answers 4xx for this URL will answer 4xx for it
        // tomorrow too — a wrong token, or a release that was taken down.
        return Err(FetchFailure::Permanent(format!(
            "o provider respondeu HTTP {} ao baixar a release",
            status.as_u16()
        )));
    }

    // Read in chunks so a URL pointing at something enormous is cut off
    // rather than buffered whole.
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        let chunk = resp
            .chunk()
            .await
            .map_err(|e| FetchFailure::Transient(format!("lendo a release: {e}")))?;
        let Some(chunk) = chunk else { break };
        if bytes.len() + chunk.len() > MAX_RELEASE_FILE_BYTES {
            return Err(FetchFailure::Permanent(format!(
                "arquivo maior que o limite de {} MiB — a URL provavelmente não aponta para um {}",
                MAX_RELEASE_FILE_BYTES / (1024 * 1024),
                expected_kind(protocol),
            )));
        }
        bytes.extend_from_slice(&chunk);
    }

    if !looks_like(&bytes, protocol) {
        return Err(FetchFailure::Permanent(format!(
            "o conteúdo baixado não é um {} (começa com {:?}) — o provider provavelmente devolveu uma página de erro com status 200",
            expected_kind(protocol),
            preview(&bytes),
        )));
    }
    Ok(bytes)
}

fn expected_kind(protocol: brarr_download_client::Protocol) -> &'static str {
    match protocol {
        brarr_download_client::Protocol::Torrent => ".torrent",
        brarr_download_client::Protocol::Usenet => ".nzb",
    }
}

/// Cheap shape check on the fetched bytes.
///
/// Trackers answer an expired session with their HTML login page and a
/// `200`, and both download clients would take that file and sit on it.
/// A bencoded torrent starts with `d`; an nzb is XML, so the first
/// non-whitespace byte is `<`.
fn looks_like(bytes: &[u8], protocol: brarr_download_client::Protocol) -> bool {
    let trimmed = trim_leading(bytes);
    match protocol {
        brarr_download_client::Protocol::Torrent => trimmed.first() == Some(&b'd'),
        brarr_download_client::Protocol::Usenet => trimmed.first() == Some(&b'<'),
    }
}

/// Skip ASCII whitespace and a UTF-8 BOM.
fn trim_leading(bytes: &[u8]) -> &[u8] {
    let rest = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let start = rest
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(rest.len());
    &rest[start..]
}

/// First few bytes as a lossy string, for the error message.
fn preview(bytes: &[u8]) -> String {
    let end = bytes.len().min(24);
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert on happy paths"
)]
mod tests {
    use super::*;

    #[test]
    fn a_bencoded_torrent_is_recognised() {
        assert!(looks_like(
            b"d8:announce20:https://x/anneee",
            brarr_download_client::Protocol::Torrent
        ));
    }

    #[test]
    fn an_html_error_page_is_not_a_torrent() {
        // The failure this check exists for: HTTP 200, HTML body.
        assert!(!looks_like(
            b"<!DOCTYPE html><html><body>Login</body></html>",
            brarr_download_client::Protocol::Torrent
        ));
    }

    #[test]
    fn an_nzb_is_recognised_through_whitespace_and_a_bom() {
        assert!(looks_like(
            b"\xEF\xBB\xBF\n  <?xml version=\"1.0\"?><nzb/>",
            brarr_download_client::Protocol::Usenet
        ));
    }

    #[test]
    fn a_torrent_is_not_accepted_as_an_nzb() {
        assert!(!looks_like(
            b"d8:announce",
            brarr_download_client::Protocol::Usenet
        ));
        assert!(!looks_like(
            b"<?xml version=\"1.0\"?>",
            brarr_download_client::Protocol::Torrent
        ));
    }

    #[test]
    fn an_empty_body_is_not_a_release_file() {
        assert!(!looks_like(b"", brarr_download_client::Protocol::Torrent));
        assert!(!looks_like(
            b"   \n",
            brarr_download_client::Protocol::Usenet
        ));
    }

    #[test]
    fn a_dead_client_is_retryable_and_a_refused_file_is_not() {
        let row = DownloadClientRow {
            id: uuid::Uuid::new_v4(),
            name: "qb".to_owned(),
            kind: brarr_download_client::DownloadClientKind::Qbittorrent,
            base_url: url::Url::parse("http://x/").unwrap(),
            username: None,
            password: None,
            api_key: None,
            category: None,
            enabled: true,
            priority: 1,
            created_at: time::OffsetDateTime::now_utc(),
        };
        let refused = DownloadClientError::Http {
            kind: brarr_download_client::DownloadClientKind::Qbittorrent,
            status: 200,
            body: "Fails.".to_owned(),
        };
        assert!(matches!(
            classify_client_error(&row, &refused),
            DeliveryOutcome::Permanent(_)
        ));
        let refused_creds = DownloadClientError::Auth {
            kind: brarr_download_client::DownloadClientKind::Qbittorrent,
            detail: "Fails.".to_owned(),
        };
        assert!(
            matches!(
                classify_client_error(&row, &refused_creds),
                DeliveryOutcome::Retryable(_)
            ),
            "a wrong password must not blacklist the release"
        );
    }

    #[test]
    fn the_reason_travels_with_the_outcome() {
        let sent = DeliveryOutcome::Sent {
            client_name: "qb".to_owned(),
            client_item_id: None,
        };
        assert!(sent.is_sent());
        assert_eq!(sent.reason(), "");
        assert_eq!(
            DeliveryOutcome::Retryable("cliente fora do ar".to_owned()).reason(),
            "cliente fora do ar"
        );
    }
}
