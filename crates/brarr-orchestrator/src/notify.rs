//! Telling the media servers that the library changed.
//!
//! The last piece of the \*arr that brarr had not taken over. Radarr and
//! Sonarr call it Connect → Plex Media Server and Connect →
//! Emby/Jellyfin; without it, a file brarr just imported stays invisible
//! until the media server happens to scan on its own.
//!
//! ## One trigger, and it is not `mark_imported`
//!
//! Four call sites write `grabs::mark_imported`, and only one of them
//! means a file arrived: the automatic importer. The other three record
//! files that were *already on disk* — `arr_import::record` adopting a
//! Sonarr catalogue, the manual adoption screen, and `AlreadyPresent`,
//! which is brarr finding a file already sitting at the destination. The
//! media server indexed those long ago, and `arr_import` runs for every
//! series every half hour with no cap at all: 468 titles on this stack.
//! Hooking the funnel would trade one useful notification for thousands
//! of useless ones.
//!
//! ## Best-effort, always
//!
//! Nothing here can fail an import. A media server that is down, a token
//! that expired, a mapping nobody wrote — all of them land in
//! `media_servers.last_error` and the pass carries on. The file is on
//! disk and the catalogue is right either way; the only thing at stake
//! is how soon it shows up in Plex, and a media server rescans on its
//! own eventually.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::db::{media_server_mappings, media_servers, root_folders};
use crate::remote_path;
use crate::state::AppState;

/// How long a pending Plex sign-in is remembered.
///
/// A plex.tv PIN reports `expiresIn: 1799` — thirty minutes. This is
/// slightly longer on purpose: the mailbox must outlive the PIN so the
/// screen can say *expirou* rather than losing the attempt and having to
/// say nothing at all.
pub const PLEX_LOGIN_TTL: Duration = Duration::from_secs(35 * 60);

/// How often the sign-in fragment asks whether the token has landed.
///
/// Sonarr polls every five seconds. Three is what the rest of this app
/// already uses for a badge the operator is looking straight at
/// (`SCAN_POLL`), and it stays far away from the rate limit plex.tv
/// documents for its auth endpoints — a full thirty-minute wait is 600
/// requests, against the one-second polling that Jellyseerr runs in
/// production.
pub const PLEX_LOGIN_POLL: Duration = Duration::from_secs(3);

/// A Plex sign-in waiting on the operator.
///
/// Holds the PIN's id and plex.tv's own deadline. Neither \*arr bounds
/// its poll — Sonarr's loop asks forever — so an operator who closes the
/// tab leaves a spinner turning. The deadline is what stops it, and it
/// comes from plex.tv rather than from a number chosen here.
#[derive(Debug, Clone)]
pub struct PendingPlexLogin {
    /// plex.tv's id for the PIN.
    pub pin_id: i64,
    /// The code, kept so the sign-in link can be rendered again without
    /// burning a second PIN if the fragment is re-requested.
    pub code: String,
    /// When to stop asking.
    pub deadline: Instant,
}

impl PendingPlexLogin {
    /// `true` once plex.tv would have forgotten the PIN anyway.
    #[must_use]
    pub fn is_expired(&self, now: Instant) -> bool {
        now >= self.deadline
    }
}

/// This install's plex.tv identity, generated on first use.
///
/// Read-then-write rather than upsert: the value must never change once
/// a token exists under it. A race between two first-time callers would
/// have both generate one, and the loser's write would orphan the
/// winner's PIN — so the read after the write is what decides, and both
/// end up using the same string.
///
/// # Errors
///
/// Returns [`crate::AppError::Database`] when the setting cannot be read
/// or written.
pub async fn plex_identity(
    pool: &crate::db::Pool,
) -> Result<brarr_media_server::PlexIdentity, crate::AppError> {
    use crate::db::settings;

    let stored = |row: Option<settings::SettingRow>| {
        row.map(|r| r.value)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };

    if let Some(existing) = stored(settings::get(pool, settings::KEY_PLEX_CLIENT_IDENTIFIER).await?)
    {
        return Ok(brarr_media_server::PlexIdentity::new(existing));
    }
    let generated = uuid::Uuid::new_v4().to_string();
    settings::set(pool, settings::KEY_PLEX_CLIENT_IDENTIFIER, &generated).await?;
    // Read back rather than trust the write: if another request got
    // there first, its value is the one every future call will see.
    let effective = stored(settings::get(pool, settings::KEY_PLEX_CLIENT_IDENTIFIER).await?)
        .unwrap_or(generated);
    info!(
        target: "brarr_orchestrator::notify",
        "generated this install's plex.tv client identifier"
    );
    Ok(brarr_media_server::PlexIdentity::new(effective.trim()))
}

/// Tell every enabled server about the titles that gained a file.
///
/// `files` are paths brarr just wrote, in brarr's own namespace. They
/// are reduced to title folders, deduplicated, translated per server,
/// and delivered in one call per server.
pub async fn imported(state: &AppState, files: &[PathBuf]) {
    if files.is_empty() {
        return;
    }
    let servers = match media_servers::list_enabled(state.pool()).await {
        Ok(servers) if servers.is_empty() => {
            debug!(
                target: "brarr_orchestrator::notify",
                "no media server configured; nothing to tell"
            );
            return;
        }
        Ok(servers) => servers,
        Err(e) => {
            warn!(target: "brarr_orchestrator::notify", error = %e, "could not read the media servers");
            return;
        }
    };

    let roots = match root_folders::list_all(state.pool()).await {
        Ok(roots) => roots.into_iter().map(|r| r.path).collect::<Vec<_>>(),
        Err(e) => {
            warn!(target: "brarr_orchestrator::notify", error = %e, "could not read the root folders");
            return;
        }
    };

    // A `BTreeSet` rather than a `HashSet`: the order the servers are
    // told in is then stable, which makes a log line from two passes
    // comparable and a test able to assert on the payload.
    let folders: BTreeSet<PathBuf> = files
        .iter()
        .map(|file| title_folder(&roots, file))
        .collect();
    let folders: Vec<PathBuf> = folders.into_iter().collect();

    for server in servers {
        let rules = match media_server_mappings::rules_for_server(state.pool(), server.id).await {
            Ok(rules) => rules,
            Err(e) => {
                warn!(
                    target: "brarr_orchestrator::notify",
                    server = %server.name,
                    error = %e,
                    "could not read the path mappings"
                );
                continue;
            }
        };
        let translated: Vec<String> = folders
            .iter()
            .map(|folder| remote_path::to_remote(&rules, folder).remote)
            .collect();

        let client = match server.to_config().and_then(|config| {
            brarr_media_server::build(config)
                .map_err(|e| crate::AppError::InvalidInput(format!("{}: {e}", server.name)))
        }) {
            Ok(client) => client,
            Err(e) => {
                record_failure(state, server.id, &server.name, &e.to_string()).await;
                continue;
            }
        };

        match client.notify_updated(&translated).await {
            Ok(()) => {
                info!(
                    target: "brarr_orchestrator::notify",
                    server = %server.name,
                    kind = server.kind.label(),
                    folders = translated.len(),
                    "told the media server the library changed"
                );
                if let Err(e) = media_servers::mark_notified(state.pool(), server.id).await {
                    warn!(target: "brarr_orchestrator::notify", error = %e, "could not record the notification");
                }
            }
            Err(e) => record_failure(state, server.id, &server.name, &e.to_string()).await,
        }
    }
}

async fn record_failure(state: &AppState, id: uuid::Uuid, name: &str, error: &str) {
    warn!(
        target: "brarr_orchestrator::notify",
        server = %name,
        error = %error,
        "could not tell the media server; the file is fine and it will be found on the next scan"
    );
    if let Err(e) = media_servers::mark_notify_error(state.pool(), id, error).await {
        warn!(target: "brarr_orchestrator::notify", error = %e, "could not record the failure");
    }
}

/// The title's own folder — what both \*arr send, and never the file.
///
/// A media server is told where to look, and looking at one file is
/// nearly always wrong: an episode arrives inside
/// `{root}/Series/Season 02/`, and a scan of the season folder alone
/// misses the artwork and the `.nfo` beside it. The unit is the first
/// component under the root folder, which is how Sonarr computes it
/// too (`rootFolderPath.GetRelativePath(series.Path)`).
///
/// Falls back to the file's own parent when no root contains it. That
/// should not happen — this is only ever called with a path the importer
/// just wrote under a root — and guessing the parent is both harmless
/// and better than dropping the notification.
fn title_folder(roots: &[PathBuf], file: &Path) -> PathBuf {
    let best = roots
        .iter()
        .filter(|root| remote_path::is_under(file, root))
        // Longest wins, for the same reason it does in the path
        // matcher: nested roots must resolve to the specific one.
        .max_by_key(|root| root.as_os_str().len());

    match best {
        Some(root) => match file.strip_prefix(root).ok().and_then(|rest| {
            rest.components()
                .next()
                .map(|first| root.join(first.as_os_str()))
        }) {
            Some(folder) => folder,
            // The file *is* the root. Nothing sane produces this; the
            // root itself is the only honest answer.
            None => root.clone(),
        },
        None => file.parent().unwrap_or(file).to_path_buf(),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert on happy paths"
)]
mod tests {
    use super::*;

    fn roots() -> Vec<PathBuf> {
        vec![
            PathBuf::from("/midias/Filmes"),
            PathBuf::from("/midias/Series"),
            PathBuf::from("/midias/Animes"),
        ]
    }

    #[test]
    fn a_movie_reduces_to_its_own_folder() {
        assert_eq!(
            title_folder(
                &roots(),
                Path::new("/midias/Filmes/Scary Movie (2000)/Scary.Movie.2000.mkv")
            ),
            PathBuf::from("/midias/Filmes/Scary Movie (2000)")
        );
    }

    #[test]
    fn an_episode_reduces_to_the_series_folder_not_the_season() {
        // The season folder alone would miss the artwork and the .nfo
        // that live one level up — and it is not what either *arr sends.
        assert_eq!(
            title_folder(
                &roots(),
                Path::new("/midias/Series/Fringe/Season 02/Fringe - S02E01.mkv")
            ),
            PathBuf::from("/midias/Series/Fringe")
        );
    }

    #[test]
    fn the_most_specific_root_wins() {
        let roots = vec![PathBuf::from("/midias"), PathBuf::from("/midias/Animes")];
        assert_eq!(
            title_folder(&roots, Path::new("/midias/Animes/Bleach/Season 01/x.mkv")),
            PathBuf::from("/midias/Animes/Bleach"),
            "the nested root, not the catch-all one component higher"
        );
    }

    #[test]
    fn a_file_under_no_root_still_produces_something_to_scan() {
        assert_eq!(
            title_folder(&roots(), Path::new("/outro/lugar/Filme/f.mkv")),
            PathBuf::from("/outro/lugar/Filme")
        );
    }

    #[test]
    fn two_episodes_of_one_series_are_one_folder() {
        let files = [
            PathBuf::from("/midias/Series/Fringe/Season 02/Fringe - S02E01.mkv"),
            PathBuf::from("/midias/Series/Fringe/Season 02/Fringe - S02E02.mkv"),
            PathBuf::from("/midias/Filmes/Heat (1995)/Heat.mkv"),
        ];
        let folders: BTreeSet<PathBuf> = files
            .iter()
            .map(|file| title_folder(&roots(), file))
            .collect();
        assert_eq!(
            folders.len(),
            2,
            "a pass that lands three files must not fire three refreshes for two titles"
        );
    }
}
