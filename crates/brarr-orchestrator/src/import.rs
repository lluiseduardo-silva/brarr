//! The import — the only code in brarr that writes to the operator's
//! files.
//!
//! A finished download sits in the client's folder under whatever name
//! the release group chose. This moves it into the library under a name
//! Plex and Jellyfin can read, and records where it went.
//!
//! ```text
//!   grabs (status = completed)
//!         │
//!         ▼
//!   client.status() ── where did the client put it?
//!         │
//!         ▼
//!   pick_video()    ── which file is the release
//!         │
//!         ▼
//!   destination()   ── {root}/Título (Ano)/Título (Ano) - S01E02.mkv
//!         │
//!         ▼
//!   place()         ── hardlink, or copy, or move
//!         │
//!         ▼
//!   grabs::mark_imported()
//! ```
//!
//! ## brarr does not have to parse anything
//!
//! Radarr and Sonarr guess what a file is from its name, because a file
//! can arrive from anywhere. brarr only ever imports files it grabbed
//! itself, and the grab already carries `item_id` and `episode_id`. The
//! title, year and episode number come from the catalogue, not from a
//! regex over the release name. This is the single largest simplification
//! available in this whole block — do not throw it away.
//!
//! ## Rules that exist to protect files
//!
//! - **Never overwrite.** The destination is created with `create_new`,
//!   so the check and the write are one operation and a race cannot slip
//!   between them. An existing file fails the import; it does not become
//!   a silent replacement.
//! - **Never leave a partial file.** A copy that fails mid-way deletes
//!   what it wrote. Half a video in the library is worse than none,
//!   because nothing downstream can tell it apart from a whole one.
//! - **Never remove from the download client.** Private trackers need
//!   the file to keep seeding; `hardlink` is the default precisely
//!   because it leaves the client's copy in place at no cost in disk.
//! - **Never guess a destination.** No root folder configured means the
//!   import waits, not that it picks somewhere plausible.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::db::grabs::{self, Grab, GrabStatus};
use crate::db::library::{self, LibraryItem};
use crate::db::root_folders::{self, RootFolder};
use crate::db::{download_clients, path_mappings, settings};
use crate::{AppError, AppState};

/// How often the importer looks for finished downloads.
pub const IMPORT_INTERVAL: Duration = Duration::from_secs(60);

/// Imports attempted per pass. A cross-device copy of a 60 GB remux
/// takes minutes; the cap keeps one slow pass from starving the next.
pub const MAX_IMPORTS_PER_PASS: usize = 3;

/// Delay before the first pass, so it doesn't pile onto the startup burst.
const STARTUP_DELAY: Duration = Duration::from_secs(120);

/// Settings key for [`ImportMode`].
pub const KEY_IMPORT_MODE: &str = "import_mode";

/// Extensions brarr treats as the release itself.
const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "avi", "m4v", "mpg", "mpeg", "mov", "wmv", "ts", "m2ts", "webm",
];

/// How a file gets from the download folder into the library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImportMode {
    /// Two names for one set of bytes. Costs no disk, and the download
    /// client keeps its copy — which is what keeps a private tracker's
    /// ratio alive. Only possible within one filesystem; falls back to
    /// [`Self::Copy`] when the library is on another device.
    #[default]
    Hardlink,
    /// A second, independent copy. Doubles the space and keeps seeding.
    Copy,
    /// Moves the bytes. Frees the space immediately and **stops the
    /// seed** — on a private tracker that costs ratio, irreversibly.
    Move,
}

impl ImportMode {
    /// Persisted label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Hardlink => "hardlink",
            Self::Copy => "copy",
            Self::Move => "move",
        }
    }

    /// Parse from the persisted label, falling back to the default for
    /// anything unrecognised — a bad setting must not stop imports.
    #[must_use]
    pub fn from_label(s: &str) -> Self {
        match s.trim() {
            "copy" => Self::Copy,
            "move" => Self::Move,
            _ => Self::Hardlink,
        }
    }
}

/// What one import attempt did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportOutcome {
    /// The file is in the library.
    Imported {
        /// Where it landed.
        path: PathBuf,
        /// How it got there — which may not be what was configured, if
        /// a hardlink was impossible.
        mode: ImportMode,
    },
    /// A file was already sitting at the destination brarr computed for
    /// this grab, so the coordinate is covered and the grab records that
    /// file rather than the one that just arrived.
    ///
    /// **Not a failure, and specifically not this release's failure.**
    /// It used to be [`Self::Permanent`], which marks the grab `failed`
    /// — and `failed` deliberately *keeps* the barrier key occupied so
    /// the scanner moves on to the next release. The next release then
    /// downloaded, landed on the same file, and failed the same way, one
    /// per sweep, forever: 55 grabs and 413.7 GiB on this operator's
    /// install before it was found. The destination encodes the episode
    /// (or the film), not the release, so a file there answers the
    /// question the sweep was asking.
    ///
    /// Never-overwrite is untouched: adopting reads the path and writes
    /// nothing.
    AlreadyPresent {
        /// The file that was already there. Becomes the grab's
        /// `imported_path`, which is what [`crate::verify`] then watches
        /// — so if the operator deletes it, brarr goes looking again.
        path: PathBuf,
    },
    /// Could not import *yet*: no root folder configured, the client is
    /// unreachable. The grab stays `completed` and the next pass retries.
    Waiting(String),
    /// Will never work: no video in the download, nothing importable in
    /// it. The grab is marked `failed`.
    Permanent(String),
}

/// Counts from one pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportSummary {
    /// Grabs considered.
    pub considered: usize,
    /// Files placed in the library.
    pub imported: usize,
    /// Grabs whose destination already held a file, recorded against it.
    ///
    /// Counted apart from [`Self::imported`] because nothing was
    /// written: a run that is all adoptions means brarr is downloading
    /// what it already has, which is a configuration story, not a
    /// success story.
    pub adopted: usize,
    /// Grabs left for the next pass.
    pub waiting: usize,
    /// Grabs marked failed.
    pub failed: usize,
}

/// Spawn the background importer. Its own task, not part of the queue
/// sync: a cross-device copy takes minutes, and download tracking must
/// not stop while it runs.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let state = Arc::new(state);
    info!(
        target: "brarr_orchestrator::import",
        interval_secs = IMPORT_INTERVAL.as_secs(),
        "starting the importer"
    );
    tokio::spawn(async move {
        sleep(STARTUP_DELAY).await;
        loop {
            match import_pending(&state).await {
                Ok(summary) if summary.considered > 0 => info!(
                    target: "brarr_orchestrator::import",
                    considered = summary.considered,
                    imported = summary.imported,
                    waiting = summary.waiting,
                    failed = summary.failed,
                    "import pass complete"
                ),
                Ok(_) => debug!(target: "brarr_orchestrator::import", "nothing to import"),
                Err(e) => {
                    warn!(target: "brarr_orchestrator::import", error = %e, "import pass failed");
                }
            }
            sleep(IMPORT_INTERVAL).await;
        }
    })
}

/// Import every finished download, up to [`MAX_IMPORTS_PER_PASS`].
///
/// # Errors
///
/// Returns [`AppError::Database`] when the grab list cannot be read.
/// Per-grab problems become an [`ImportOutcome`], not an error.
pub async fn import_pending(state: &AppState) -> Result<ImportSummary, AppError> {
    // Nothing is written to the operator's files while paused.
    if crate::db::settings::is_paused(state.pool()).await {
        return Ok(ImportSummary::default());
    }
    let pending = grabs::awaiting_import(state.pool()).await?;
    let mut summary = ImportSummary::default();
    for grab in pending.into_iter().take(MAX_IMPORTS_PER_PASS) {
        summary.considered += 1;
        match import_grab(state, &grab).await? {
            ImportOutcome::Imported { path, mode } => {
                summary.imported += 1;
                info!(
                    target: "brarr_orchestrator::import",
                    grab_id = %grab.id,
                    release = %grab.release_name,
                    destination = %path.display(),
                    mode = mode.label(),
                    "imported"
                );
            }
            ImportOutcome::AlreadyPresent { path } => {
                summary.adopted += 1;
                // `warn!`, not `info!`: brarr downloaded something it
                // already had. The file is fine and the catalogue is now
                // right, but the traffic was wasted and the operator is
                // the only one who can say why the coordinate was not
                // covered in the first place.
                warn!(
                    target: "brarr_orchestrator::import",
                    grab_id = %grab.id,
                    release = %grab.release_name,
                    destination = %path.display(),
                    "the destination already held a file; adopted it and did not overwrite"
                );
            }
            ImportOutcome::Waiting(reason) => {
                summary.waiting += 1;
                debug!(
                    target: "brarr_orchestrator::import",
                    grab_id = %grab.id,
                    reason = %reason,
                    "not importable yet"
                );
            }
            ImportOutcome::Permanent(reason) => {
                summary.failed += 1;
                warn!(
                    target: "brarr_orchestrator::import",
                    grab_id = %grab.id,
                    release = %grab.release_name,
                    reason = %reason,
                    "import failed for good"
                );
            }
        }
    }
    Ok(summary)
}

/// Import one grab, recording the result on the row.
///
/// # Errors
///
/// Returns [`AppError::Database`] when reading the catalogue or writing
/// the outcome fails.
pub async fn import_grab(state: &AppState, grab: &Grab) -> Result<ImportOutcome, AppError> {
    // Stamped before the attempt, not after: a pass that dies part-way
    // must not leave this grab first in line forever.
    grabs::mark_import_attempted(state.pool(), grab.id).await?;

    let outcome = plan_and_place(state, grab).await?;
    match &outcome {
        // Both record the grab against a file in the library. The only
        // difference is who put it there, and that is a counter and a
        // log line, not a different row state: `blocks_search` has to go
        // true either way or the sweep comes straight back.
        ImportOutcome::Imported { path, .. } | ImportOutcome::AlreadyPresent { path } => {
            grabs::mark_imported(state.pool(), grab.id, &path.to_string_lossy()).await?;
        }
        // Left as `completed` on purpose: the next pass tries again, and
        // `blocks_search` already keeps the scanner off the item. The
        // reason is persisted because waiting used to be invisible — it
        // lived in a debug! log, and five finished downloads sat on disk
        // with nothing in the UI able to say why.
        ImportOutcome::Waiting(reason) => {
            grabs::set_import_wait_reason(state.pool(), grab.id, Some(reason)).await?;
        }
        ImportOutcome::Permanent(reason) => {
            grabs::set_status(state.pool(), grab.id, GrabStatus::Failed, Some(reason)).await?;
        }
    }
    Ok(outcome)
}

async fn plan_and_place(state: &AppState, grab: &Grab) -> Result<ImportOutcome, AppError> {
    let item = match library::get_by_id(state.pool(), grab.item_id).await {
        Ok(item) => item,
        Err(AppError::NotFound(_)) => {
            return Ok(ImportOutcome::Permanent(
                "o item saiu da biblioteca antes do import".to_owned(),
            ));
        }
        Err(e) => return Err(e),
    };

    let Some(root) = resolve_root(state, &item).await? else {
        return Ok(ImportOutcome::Waiting(format!(
            "nenhuma pasta raiz configurada para {}",
            item.media_type.label()
        )));
    };

    let located = match locate_download(state, grab).await? {
        Ok(located) => located,
        Err(reason) => return Ok(ImportOutcome::Waiting(reason)),
    };
    // A path in a namespace this machine cannot open is not worth a
    // syscall, and "does not exist" would be a useless thing to tell the
    // operator about it.
    if !located.usable {
        return Ok(ImportOutcome::Waiting(unreachable_path_message(&located)));
    }

    let episode = match grab.episode_id {
        Some(id) => library::episodes(state.pool(), item.id)
            .await?
            .into_iter()
            .find(|e| e.id == id),
        None => None,
    };
    // **The name on disk is the catalogue's coordinate**, and that is
    // only safe because the catalogue's coordinate is now the one
    // releases use. It used to be translated here for the same reason
    // the search was: Dragon Ball Super is one season of 131 to TMDB and
    // five arcs to everybody else, so writing `Season 01/… S01E15` beside
    // 130 files named `Season 2/… S02E01` gave Plex a second show. A tree
    // built by whoever numbers it that way writes the neighbours' name
    // without being told.
    let marker = episode.as_ref().and_then(|e| {
        Some((
            u16::try_from(e.season_number).ok()?,
            u16::try_from(e.episode_number).ok()?,
        ))
    });

    let plan = Placement {
        root: root.path.clone(),
        title: item.title.clone(),
        year: item.year,
        episode: marker,
        mode: configured_mode(state).await,
    };

    // Everything past here touches the filesystem, so it runs on the
    // blocking pool: a 60 GB copy on a runtime worker would stall every
    // other task in the process.
    let source = located.path.clone();
    let outcome = tokio::task::spawn_blocking(move || place_download(&source, &plan))
        .await
        .map_err(|e| AppError::InvalidInput(format!("tarefa de import falhou: {e}")))?;

    // `place_download` only knows a path it could not open. Only here do
    // we know *which* client reported it, what it said verbatim, and
    // whether a mapping rewrote it — which is the difference between
    // "não existe mais" and a sentence the operator can act on.
    Ok(match outcome {
        ImportOutcome::Waiting(_) if !matches!(locate_state(&located), LocateState::Translated) => {
            ImportOutcome::Waiting(unreachable_path_message(&located))
        }
        other => other,
    })
}

/// Whether a mapping rewrote the path, for message selection.
enum LocateState {
    /// A mapping fired.
    Translated,
    /// The client's path came through untouched.
    Verbatim,
}

fn locate_state(located: &Located) -> LocateState {
    if located.applied.is_some() {
        LocateState::Translated
    } else {
        LocateState::Verbatim
    }
}

/// The sentence an operator can act on when brarr cannot see a finished
/// download.
///
/// "não existe mais" was the old message, and it was actively
/// misleading: the file existed, fully downloaded, the whole time. What
/// the operator needs is what the client said, what brarr looked for,
/// and the name of the thing that reconciles the two.
fn unreachable_path_message(located: &Located) -> String {
    match &located.applied {
        Some(rule) => format!(
            "o mapeamento {} → {} traduziu o caminho de {} para {}, e o brarr não conseguiu abrir. \
             O download está lá — confira se o lado local do mapeamento é o caminho certo \
             dentro do contêiner do brarr.",
            rule.remote_prefix,
            rule.local_prefix.display(),
            located.client_name,
            located.path.display(),
        ),
        None => format!(
            "{} salvou em {}, e esse caminho não existe para o brarr. \
             Os dois montam o mesmo disco em lugares diferentes: cadastre um mapeamento \
             de caminho em /download-clients dizendo o que {} é para o brarr.",
            located.client_name, located.reported, located.reported,
        ),
    }
}

/// Root folder for this item — its own override first, then the rule.
pub(crate) async fn resolve_root(
    state: &AppState,
    item: &LibraryItem,
) -> Result<Option<RootFolder>, AppError> {
    if let Some(configured) = item
        .root_folder
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        // The operator pinned this item somewhere. Honour it only if it
        // is still a registered root folder — an arbitrary path from an
        // old row is not somewhere to start writing files.
        let all = root_folders::list_all(state.pool()).await?;
        if let Some(found) = all.into_iter().find(|f| f.path == Path::new(configured)) {
            return Ok(Some(found));
        }
    }
    root_folders::resolve(state.pool(), item.media_type).await
}

/// Where a download is, in brarr's namespace, plus what it took to get
/// there — so a failure downstream can name the real cause.
#[derive(Debug, Clone)]
struct Located {
    /// The path to open, already translated.
    path: PathBuf,
    /// What the client actually said, kept verbatim for the message.
    reported: String,
    /// Client display name.
    client_name: String,
    /// The mapping that fired, if any.
    applied: Option<crate::remote_path::AppliedRule>,
    /// `false` when the path cannot be opened on this machine at all —
    /// relative, or written in a namespace this host does not speak.
    usable: bool,
}

/// Ask the download client where it put the files, and translate the
/// answer into brarr's namespace.
///
/// **This is the only place a client-supplied path enters brarr.**
/// `verify` stats `grabs.imported_path`, which brarr wrote itself, and
/// nothing else reads `save_path`. Keeping it to one site is what makes
/// the mapping trustworthy.
///
/// The outer `Result` is a real failure; the inner one carries a reason
/// to wait — an unreachable client is not the grab's fault.
async fn locate_download(
    state: &AppState,
    grab: &Grab,
) -> Result<Result<Located, String>, AppError> {
    let (Some(client_id), Some(item_id)) = (grab.client_id, grab.client_item_id.as_deref()) else {
        return Ok(Err(
            "o grab não registrou cliente ou identificador".to_owned()
        ));
    };
    let row = match download_clients::get_by_id(state.pool(), client_id).await {
        Ok(row) => row,
        Err(AppError::NotFound(_)) => {
            return Ok(Err("o cliente de download foi removido".to_owned()));
        }
        Err(e) => return Err(e),
    };
    let client = match brarr_download_client::build(row.to_config()) {
        Ok(c) => c,
        Err(e) => return Ok(Err(format!("{}: {e}", row.name))),
    };
    let reported = match client.status(item_id).await {
        Ok(Some(status)) => match status.save_path {
            Some(path) if !path.is_empty() => path,
            _ => {
                return Ok(Err(format!(
                    "{} não informou onde salvou o download",
                    row.name
                )));
            }
        },
        Ok(None) => return Ok(Err(format!("{} não conhece mais este download", row.name))),
        Err(e) => return Ok(Err(format!("{}: {e}", row.name))),
    };

    let mappings = path_mappings::for_client(state.pool(), client_id).await?;
    let rules: Vec<_> = mappings
        .iter()
        .map(path_mappings::PathMapping::rule)
        .collect();
    let translation = crate::remote_path::translate(&rules, &reported);
    Ok(Ok(Located {
        path: translation.local.clone(),
        reported,
        client_name: row.name,
        usable: crate::remote_path::is_usable(&translation),
        applied: translation.applied,
    }))
}

async fn configured_mode(state: &AppState) -> ImportMode {
    match settings::get(state.pool(), KEY_IMPORT_MODE).await {
        Ok(Some(row)) => ImportMode::from_label(&row.value),
        // A missing or unreadable setting is the default, not a stall.
        _ => ImportMode::default(),
    }
}

// ---------------------------------------------------------------------
// Everything below is synchronous and filesystem-only, so it can be
// tested against a temporary directory without a pool or a client.
// ---------------------------------------------------------------------

/// Why [`pick_video`] could not choose a file.
///
/// The split is the whole point: one of these means "fix your
/// configuration and I will succeed", the other means "this download
/// will never be importable". Collapsing them is what marked five
/// finished downloads as failed while their files sat on disk.
#[derive(Debug)]
enum PickError {
    /// The path could not be opened — it is not there, or brarr is not
    /// allowed to look, or it names a place on another machine. Always a
    /// reason to **wait**: the file may be perfectly fine and one
    /// mapping away.
    NotVisible(std::io::Error),
    /// The path opened and what is inside cannot be imported. Permanent:
    /// retrying reads the same bytes forever.
    BadContent(String),
}

/// Everything `place_download` needs, as one value.
///
/// A struct rather than seven parameters because `clippy.toml` sets
/// `too-many-arguments-threshold = 6` and the old signature was already
/// at six.
#[derive(Debug, Clone)]
struct Placement {
    /// Destination root.
    root: PathBuf,
    /// Catalogue title.
    title: String,
    /// Release year, for the folder name.
    year: Option<i32>,
    /// Season and episode, for a series.
    episode: Option<(u16, u16)>,
    /// Hardlink, copy or move.
    mode: ImportMode,
}

/// Pick the file, build the destination, and place it.
fn place_download(source: &Path, plan: &Placement) -> ImportOutcome {
    let video = match pick_video(source, plan.episode) {
        Ok(path) => path,
        // The caller turns this into a sentence that names the mapping;
        // here we only know it could not be opened.
        Err(PickError::NotVisible(e)) => {
            return ImportOutcome::Waiting(format!("não consegui abrir {}: {e}", source.display()));
        }
        Err(PickError::BadContent(reason)) => return ImportOutcome::Permanent(reason),
    };
    let extension = video.extension().map_or_else(
        || "mkv".to_owned(),
        |e| e.to_string_lossy().to_ascii_lowercase(),
    );
    let destination = destination(&plan.root, &plan.title, plan.year, plan.episode, &extension);
    // Outermost component first: the season rule reads the item folder,
    // so it has to be told which item folder before it can look inside.
    let destination = reuse_existing_item_folder(&plan.root, &destination);
    let destination = reuse_existing_season_folder(&destination);

    // Asked before placing, not discovered by failing to place. `place`
    // keeps its own check for the race, but a file already there is an
    // answer about the coordinate, and only here is there enough context
    // to say so — see `ImportOutcome::AlreadyPresent`.
    if destination.exists() {
        return ImportOutcome::AlreadyPresent { path: destination };
    }

    match place(&video, &destination, plan.mode) {
        Ok(used) => ImportOutcome::Imported {
            path: destination,
            mode: used,
        },
        Err(reason) => ImportOutcome::Permanent(reason),
    }
}

/// The release's video file inside a finished download.
///
/// `source` may be the file itself (a single-file torrent) or a folder.
/// The rule is *the largest video that is not a sample*, with one
/// refinement for episodes: when several videos are present and one
/// names the episode, that one wins — a season pack that slipped through
/// must not import its first file as episode 7.
fn pick_video(source: &Path, episode: Option<(u16, u16)>) -> Result<PathBuf, PickError> {
    // One `metadata` call at the top, instead of `is_file()` then
    // `is_dir()`. Those two throw the error away, so "the directory is
    // not there" and "I am not allowed to look" came out identical — and
    // both were treated as permanent, which burned a release per sweep
    // over what was really a container configuration problem.
    let meta = match std::fs::metadata(source) {
        Ok(meta) => meta,
        Err(e) => return Err(PickError::NotVisible(e)),
    };
    if meta.is_file() {
        return if is_video(source) {
            Ok(source.to_path_buf())
        } else {
            Err(PickError::BadContent(format!(
                "{} não é um arquivo de vídeo",
                source.display()
            )))
        };
    }
    if !meta.is_dir() {
        return Err(PickError::BadContent(format!(
            "{} não é nem arquivo nem diretório",
            source.display()
        )));
    }

    // The top-level listing is the other half of the same problem: a
    // release folder brarr can `stat` but not `read_dir` (the client
    // runs as a different uid) yields zero candidates, which used to
    // read as "no video in here" — permanent — instead of "I cannot see
    // inside". Subdirectories stay tolerant: one unreadable `Subs/` must
    // not sink an import whose video is right there.
    if let Err(e) = std::fs::read_dir(source) {
        return Err(PickError::NotVisible(e));
    }

    let mut candidates: Vec<(PathBuf, u64)> = Vec::new();
    collect_videos(source, &mut candidates, 0);
    candidates.retain(|(path, _)| !looks_like_sample(path, source));
    if candidates.is_empty() {
        return Err(PickError::BadContent(format!(
            "nenhum arquivo de vídeo encontrado em {}",
            source.display()
        )));
    }

    if let Some((season, number)) = episode {
        let named: Vec<&(PathBuf, u64)> = candidates
            .iter()
            .filter(|(path, _)| {
                path.file_name().is_some_and(|n| {
                    crate::scan::title_matches_episode(&n.to_string_lossy(), season, number)
                })
            })
            .collect();
        if named.len() == 1 {
            return Ok(named[0].0.clone());
        }
        if named.is_empty() && candidates.len() > 1 {
            return Err(PickError::BadContent(format!(
                "{} vídeos em {} e nenhum identifica S{season:02}E{number:02}",
                candidates.len(),
                source.display()
            )));
        }
    }

    candidates.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    Ok(candidates[0].0.clone())
}

/// Depth-limited walk. Release folders nest one or two levels at most
/// (`Release/Subs/`, `Release/CD1/`); anything deeper is not a layout
/// worth chasing, and an unbounded walk over a symlink loop is not a
/// risk worth taking.
pub(crate) fn collect_videos(dir: &Path, out: &mut Vec<(PathBuf, u64)>, depth: u8) {
    const MAX_DEPTH: u8 = 3;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            if depth < MAX_DEPTH {
                collect_videos(&path, out, depth + 1);
            }
        } else if meta.is_file() && is_video(&path) {
            out.push((path, meta.len()));
        }
    }
}

pub(crate) fn is_video(path: &Path) -> bool {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|ext| VIDEO_EXTENSIONS.contains(&ext.as_str()))
}

/// `sample` as a whole token anywhere **below the download folder** —
/// `Sample/`, `release-sample.mkv`, `release.sample.mkv`.
///
/// Two things this gets right that the obvious version does not:
///
/// - Only the part below `base` is examined. An operator whose downloads
///   land in `/mnt/sample-disk/` would otherwise have every single
///   import refuse, because an ancestor directory said "sample".
/// - Tokens, not substrings, so a release from a group called `SAMPLES`
///   is not mistaken for a sample of itself.
pub(crate) fn looks_like_sample(path: &Path, base: &Path) -> bool {
    let relative = path.strip_prefix(base).unwrap_or(path);
    relative.components().any(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .to_ascii_lowercase()
            .split(['.', '-', '_', ' ', '(', ')', '[', ']'])
            .any(|token| token == "sample")
    })
}

/// Build the destination path, Plex/Jellyfin style.
///
/// - movie: `{root}/Título (Ano)/Título (Ano).mkv`
/// - episode: `{root}/Título (Ano)/Season 01/Título - S01E02.mkv`
pub(crate) fn destination(
    root: &Path,
    title: &str,
    year: Option<i32>,
    episode: Option<(u16, u16)>,
    extension: &str,
) -> PathBuf {
    let folder = match year {
        Some(y) => sanitize(&format!("{title} ({y})")),
        None => sanitize(title),
    };
    let mut path = root.join(&folder);
    match episode {
        Some((season, number)) => {
            path.push(format!("Season {season:02}"));
            path.push(sanitize(&format!(
                "{title} - S{season:02}E{number:02}.{extension}"
            )));
        }
        None => path.push(sanitize(&format!("{folder}.{extension}"))),
    }
    path
}

/// Swap in the series/film folder that is already there, when its name
/// differs from ours only by the `(year)` suffix.
///
/// The same idea as [`reuse_existing_season_folder`], one component
/// further out, and it is the one that actually mattered: brarr writes
/// `Título (2022)` while Sonarr wrote `Título`, so the season rule never
/// got a chance to run — it looks *inside* the item folder, and brarr had
/// just decided to create a brand new one. Measured on this operator's
/// disk: eleven series with two folders side by side, one of them holding
/// a single episode.
///
/// Conservative in three ways, and the third was put there by the disk.
///
/// It only replaces the component when the folder brarr would write
/// **does not exist**, so it can neither invent a destination nor move a
/// library that is already consistent. It compares whole names, not
/// prefixes, so `Dune` cannot adopt `Dune Part Two`'s folder. And **the
/// year is removed from one side only**: a candidate matches when it is
/// this exact title with no year at all, never when it is the same title
/// under a *different* year.
///
/// That last rule is not hypothetical. Stripping the year from both
/// sides looked right and this operator's film library refutes it in two
/// places — `Resident Evil (2002)` beside `Resident Evil (2023)`, and
/// `Scary Movie (2000)` beside `Scary Movie (2026)`. Those are different
/// films that share a title, and the symmetric rule would have filed a
/// third `Resident Evil` inside the 2002 folder, where the movie naming
/// scheme puts a second film in a folder named after the first.
///
/// Two candidates is a question, not an answer: with no year on the item
/// and both `Título (1978)` and `Título (2003)` on disk, the pick would
/// come down to readdir order. Ambiguity leaves the path alone, the same
/// way [`pick_video`] refuses a multi-video folder it cannot resolve.
fn reuse_existing_item_folder(root: &Path, path: &Path) -> PathBuf {
    let Ok(rest) = path.strip_prefix(root) else {
        return path.to_path_buf();
    };
    let mut components = rest.components();
    let Some(std::path::Component::Normal(wanted)) = components.next() else {
        return path.to_path_buf();
    };
    if root.join(wanted).is_dir() {
        return path.to_path_buf();
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return path.to_path_buf();
    };
    let wanted = wanted.to_string_lossy();
    let mut found: Option<std::ffi::OsString> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !entry.path().is_dir() || !names_one_title(&wanted, &name.to_string_lossy()) {
            continue;
        }
        if found.is_some() {
            // More than one year-variant on disk and no way to choose.
            return path.to_path_buf();
        }
        found = Some(name);
    }
    match found {
        Some(name) => root.join(name).join(components.as_path()),
        None => path.to_path_buf(),
    }
}

/// Whether two folder names differ only by *the presence* of a year.
///
/// One side keeps its suffix and the other must have none — which is
/// what makes `Resident Evil (2002)` and `Resident Evil (2023)` two
/// titles rather than one.
fn names_one_title(wanted: &str, candidate: &str) -> bool {
    candidate.eq_ignore_ascii_case(strip_year_suffix(wanted))
        || strip_year_suffix(candidate).eq_ignore_ascii_case(wanted)
}

/// A folder name without its trailing ` (2022)`, if it has one.
///
/// Only a four-digit group in parentheses at the very end counts, so
/// `Blade Runner 2049` keeps its number — that is part of the title, and
/// stripping it would let `Blade Runner` adopt its folder.
fn strip_year_suffix(name: &str) -> &str {
    let trimmed = name.trim_end();
    let Some(open) = trimmed.strip_suffix(')').and_then(|s| s.rfind('(')) else {
        return trimmed;
    };
    let inner = &trimmed[open + 1..trimmed.len() - 1];
    if inner.len() == 4 && inner.bytes().all(|b| b.is_ascii_digit()) {
        trimmed[..open].trim_end()
    } else {
        trimmed
    }
}

/// Swap in the season folder that is already there, when its name only
/// differs from ours in spelling.
///
/// [`destination`] writes `Season 02`; Sonarr writes `Season 2`, and this
/// operator's library is Sonarr's work. Plex and Jellyfin read both as
/// season 2, so the cost is not a broken library — it is two folders side
/// by side for one season, half the episodes in each, which is confusing
/// in exactly the place brarr is trying to stop being confusing.
///
/// Deliberately conservative: it only ever replaces a component with one
/// that **exists** and that names the **same number**, so it cannot
/// invent a destination. Anything it cannot read leaves the path alone —
/// the caller creates the folder as spelled, which is the behaviour this
/// had before.
fn reuse_existing_season_folder(path: &Path) -> PathBuf {
    let (Some(file), Some(season_dir), Some(item_dir)) = (
        path.file_name(),
        path.parent().and_then(Path::file_name),
        path.parent().and_then(Path::parent),
    ) else {
        return path.to_path_buf();
    };
    let Some(wanted) = season_number(&season_dir.to_string_lossy()) else {
        return path.to_path_buf();
    };
    let Ok(entries) = std::fs::read_dir(item_dir) else {
        return path.to_path_buf();
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == season_dir || !entry.path().is_dir() {
            continue;
        }
        if season_number(&name.to_string_lossy()) == Some(wanted) {
            return item_dir.join(name).join(file);
        }
    }
    path.to_path_buf()
}

/// The season a folder name names, in either language brarr writes or
/// reads. `None` for anything that is not a season folder.
fn season_number(name: &str) -> Option<u16> {
    let lowered = name.trim().to_ascii_lowercase();
    let digits = lowered
        .strip_prefix("season")
        .or_else(|| lowered.strip_prefix("temporada"))?
        .trim();
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

/// Make one path component safe on every filesystem brarr might run on.
///
/// Windows is the strict one and the one this dev machine runs, so its
/// rules apply everywhere: no `\/:*?"<>|`, no control characters, no
/// trailing dot or space, and the reserved device names are not
/// available as a whole name.
pub(crate) fn sanitize(raw: &str) -> String {
    const ILLEGAL: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    const RESERVED: &[&str] = &[
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    let mut out = String::with_capacity(raw.len());
    let mut last_was_space = false;
    for c in raw.chars() {
        let c = if ILLEGAL.contains(&c) || c.is_control() {
            ' '
        } else {
            c
        };
        // Collapse the runs a replacement leaves behind.
        if c == ' ' {
            if last_was_space {
                continue;
            }
            last_was_space = true;
        } else {
            last_was_space = false;
        }
        out.push(c);
    }
    let trimmed = out.trim().trim_end_matches(['.', ' ']).to_owned();
    let stem = trimmed.split('.').next().unwrap_or("").to_ascii_lowercase();
    if RESERVED.contains(&stem.as_str()) {
        return format!("_{trimmed}");
    }
    if trimmed.is_empty() {
        return "sem-titulo".to_owned();
    }
    truncate_chars(&trimmed, 200)
}

/// Cap a component's length on a char boundary. 255 bytes is the usual
/// filesystem limit; 200 chars leaves room for a multi-byte title.
pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    s.chars()
        .take(max)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

/// Put `source` at `destination`, returning the mode that actually
/// worked. Never overwrites, never leaves a partial file behind.
fn place(source: &Path, destination: &Path, mode: ImportMode) -> Result<ImportMode, String> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("não consegui criar {}: {e}", parent.display()))?;
    }
    if destination.exists() {
        return Err(format!(
            "{} já existe — o brarr não sobrescreve arquivo nenhum",
            destination.display()
        ));
    }

    match mode {
        ImportMode::Hardlink => match std::fs::hard_link(source, destination) {
            Ok(()) => Ok(ImportMode::Hardlink),
            Err(e) => {
                // Cross-device is the ordinary case (library on another
                // disk), and some filesystems refuse links outright.
                // Both mean "copy instead", not "give up".
                debug!(
                    target: "brarr_orchestrator::import",
                    error = %e,
                    "hardlink not possible; copying"
                );
                copy_new(source, destination).map(|()| ImportMode::Copy)
            }
        },
        ImportMode::Copy => copy_new(source, destination).map(|()| ImportMode::Copy),
        ImportMode::Move => {
            if std::fs::rename(source, destination).is_ok() {
                return Ok(ImportMode::Move);
            }
            // `rename` cannot cross filesystems; copy then remove.
            copy_new(source, destination)?;
            std::fs::remove_file(source).map_err(|e| {
                format!(
                    "copiei para {} mas não consegui remover o original: {e}",
                    destination.display()
                )
            })?;
            Ok(ImportMode::Move)
        }
    }
}

/// Copy into a file that must not already exist.
///
/// `std::fs::copy` truncates an existing destination, which is exactly
/// the behaviour this module must not have — hence `create_new`, where
/// the check and the creation are one syscall. A failure part-way
/// removes what was written.
fn copy_new(source: &Path, destination: &Path) -> Result<(), String> {
    use std::io::Write as _;

    let mut input = std::fs::File::open(source)
        .map_err(|e| format!("não consegui abrir {}: {e}", source.display()))?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|e| format!("não consegui criar {}: {e}", destination.display()))?;

    let copied = std::io::copy(&mut input, &mut output).and_then(|n| output.flush().map(|()| n));
    match copied {
        Ok(_) => Ok(()),
        Err(e) => {
            drop(output);
            // Half a video in the library is worse than none: nothing
            // downstream could tell it apart from a whole one.
            let _ = std::fs::remove_file(destination);
            Err(format!(
                "cópia para {} falhou e foi desfeita: {e}",
                destination.display()
            ))
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on happy paths"
)]
mod tests {
    use super::*;
    use crate::db::seed::Seed;

    /// A directory that lives for one test.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("brarr-import-{name}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn file(&self, rel: &str, bytes: usize) -> PathBuf {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, vec![b'x'; bytes]).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The operator's library is Sonarr's work and spells it `Season 2`.
    /// brarr spells it `Season 02`, and two folders for one season is
    /// the confusion this whole block exists to remove.
    #[test]
    fn an_existing_season_folder_wins_over_our_spelling() {
        let dir = TempDir::new("season-folder");
        dir.file("Dragon Ball Super/Season 2/existing.mkv", 4);

        let wanted = dir
            .path()
            .join("Dragon Ball Super/Season 02/Dragon Ball Super - S02E01.mkv");
        assert_eq!(
            reuse_existing_season_folder(&wanted),
            dir.path()
                .join("Dragon Ball Super/Season 2/Dragon Ball Super - S02E01.mkv")
        );
    }

    /// Only the *same* season is a match. Finding `Season 3` next door
    /// must not drag episode 1 of season 2 into it.
    #[test]
    fn a_different_season_is_not_a_match() {
        let dir = TempDir::new("other-season");
        dir.file("Serie/Season 3/existing.mkv", 4);

        let wanted = dir.path().join("Serie/Season 02/Serie - S02E01.mkv");
        assert_eq!(reuse_existing_season_folder(&wanted), wanted);
    }

    /// Nothing there yet, or nothing readable: the path is written as
    /// spelled, which is what happened before this existed.
    #[test]
    fn an_absent_item_folder_leaves_the_path_alone() {
        let dir = TempDir::new("absent");
        let wanted = dir.path().join("Serie/Season 02/Serie - S02E01.mkv");
        assert_eq!(reuse_existing_season_folder(&wanted), wanted);
    }

    /// A movie has no season component to reconsider.
    #[test]
    fn a_movie_path_is_untouched() {
        let dir = TempDir::new("movie");
        dir.file("filmes/Duna (2024)/Duna (2024).mkv", 4);
        let wanted = dir.path().join("filmes/Duna (2024)/Duna (2024).mkv");
        assert_eq!(reuse_existing_season_folder(&wanted), wanted);
    }

    #[test]
    fn season_folders_are_read_in_either_spelling() {
        assert_eq!(season_number("Season 02"), Some(2));
        assert_eq!(season_number("season 2"), Some(2));
        assert_eq!(season_number("Temporada 11"), Some(11));
        assert_eq!(season_number("Especiais"), None);
        assert_eq!(season_number("Season"), None);
        assert_eq!(season_number("Seasons of Love"), None);
    }

    #[test]
    fn a_movie_lands_in_a_plex_shaped_path() {
        let path = destination(
            Path::new("/data/filmes"),
            "Duna: Parte Dois",
            Some(2024),
            None,
            "mkv",
        );
        assert_eq!(
            path,
            Path::new("/data/filmes/Duna Parte Dois (2024)/Duna Parte Dois (2024).mkv"),
            "the colon is illegal on Windows and would be a silent failure there"
        );
    }

    #[test]
    fn an_episode_lands_under_its_season() {
        let path = destination(
            Path::new("/data/series"),
            "The Boys",
            Some(2019),
            Some((4, 7)),
            "mkv",
        );
        assert_eq!(
            path,
            Path::new("/data/series/The Boys (2019)/Season 04/The Boys - S04E07.mkv")
        );
    }

    #[test]
    fn a_title_with_no_year_still_gets_a_folder() {
        let path = destination(Path::new("/data"), "Sem Ano", None, None, "mp4");
        assert_eq!(path, Path::new("/data/Sem Ano/Sem Ano.mp4"));
    }

    #[test]
    fn sanitising_removes_what_a_filesystem_refuses() {
        assert_eq!(sanitize("A/B\\C:D*E?F\"G<H>I|J"), "A B C D E F G H I J");
        assert_eq!(sanitize("trailing dot."), "trailing dot");
        assert_eq!(sanitize("  espaços   demais  "), "espaços demais");
        assert_eq!(sanitize(""), "sem-titulo");
        assert_eq!(sanitize("///"), "sem-titulo");
    }

    #[test]
    fn a_reserved_device_name_is_not_a_folder_name() {
        // On Windows `NUL.mkv` is not a file, it is the null device, and
        // writing to it succeeds while storing nothing.
        assert_eq!(sanitize("NUL"), "_NUL");
        assert_eq!(sanitize("con.mkv"), "_con.mkv");
        assert_eq!(sanitize("Contact"), "Contact", "only whole names count");
    }

    #[test]
    fn a_very_long_title_is_cut_on_a_char_boundary() {
        let long = "ç".repeat(500);
        let out = sanitize(&long);
        assert_eq!(out.chars().count(), 200);
    }

    #[test]
    fn the_largest_video_wins() {
        let dir = TempDir::new("largest");
        dir.file("Release/small.mkv", 10);
        dir.file("Release/feature.mkv", 1000);
        dir.file("Release/notes.txt", 5000);
        let picked = pick_video(&dir.path().join("Release"), None).unwrap();
        assert_eq!(picked.file_name().unwrap(), "feature.mkv");
    }

    #[test]
    fn samples_are_skipped_even_when_they_are_the_only_big_file() {
        let dir = TempDir::new("sample");
        dir.file("Release/Sample/movie-sample.mkv", 9999);
        dir.file("Release/movie.mkv", 100);
        let picked = pick_video(&dir.path().join("Release"), None).unwrap();
        assert_eq!(picked.file_name().unwrap(), "movie.mkv");
    }

    #[test]
    fn a_download_folder_living_under_a_sample_named_path_still_imports() {
        // Found by a test whose own temp directory happened to contain
        // "sample": scanning the whole path meant an operator with
        // downloads in /mnt/sample-disk/ could never import anything.
        let dir = TempDir::new("sample-disk");
        dir.file("Release/movie.mkv", 100);
        let picked = pick_video(&dir.path().join("Release"), None).unwrap();
        assert_eq!(picked.file_name().unwrap(), "movie.mkv");
    }

    #[test]
    fn a_group_called_samples_is_not_a_sample() {
        // Substring matching would drop this; token matching keeps it.
        let dir = TempDir::new("samples-group");
        dir.file("Release/Movie.2024.1080p-SAMPLES.mkv", 100);
        let picked = pick_video(&dir.path().join("Release"), None).unwrap();
        assert_eq!(
            picked.file_name().unwrap(),
            "Movie.2024.1080p-SAMPLES.mkv",
            "`SAMPLES` is a release group, not a sample file"
        );
    }

    #[test]
    fn a_single_file_download_needs_no_walking() {
        let dir = TempDir::new("single");
        let file = dir.file("Movie.2024.1080p.mkv", 100);
        assert_eq!(pick_video(&file, None).unwrap(), file);
    }

    #[test]
    fn a_download_with_no_video_is_a_permanent_failure() {
        let dir = TempDir::new("novideo");
        dir.file("Release/readme.nfo", 10);
        assert!(pick_video(&dir.path().join("Release"), None).is_err());
    }

    #[test]
    fn an_episode_grab_picks_the_file_that_names_the_episode() {
        // The season-pack guard: the largest file is a different episode.
        let dir = TempDir::new("episode");
        dir.file("Pack/The.Boys.S04E01.1080p.mkv", 9999);
        dir.file("Pack/The.Boys.S04E07.1080p.mkv", 100);
        let picked = pick_video(&dir.path().join("Pack"), Some((4, 7))).unwrap();
        assert_eq!(picked.file_name().unwrap(), "The.Boys.S04E07.1080p.mkv");
    }

    #[test]
    fn a_pack_that_names_no_episode_is_refused_rather_than_guessed() {
        let dir = TempDir::new("ambiguous");
        dir.file("Pack/part1.mkv", 100);
        dir.file("Pack/part2.mkv", 200);
        let err = pick_video(&dir.path().join("Pack"), Some((4, 7))).unwrap_err();
        match err {
            PickError::BadContent(reason) => {
                assert!(reason.contains("S04E07"), "got {reason}");
            }
            other @ PickError::NotVisible(_) => {
                panic!("an ambiguous pack is permanent, got {other:?}")
            }
        }
    }

    #[test]
    fn one_unnamed_video_is_still_accepted_for_an_episode() {
        // A single-file download of one episode often carries a name the
        // marker check cannot read; with nothing to confuse it for, take it.
        let dir = TempDir::new("single-episode");
        dir.file("Pack/episodio.mkv", 100);
        let picked = pick_video(&dir.path().join("Pack"), Some((4, 7))).unwrap();
        assert_eq!(picked.file_name().unwrap(), "episodio.mkv");
    }

    #[test]
    fn placing_hardlinks_by_default_and_leaves_the_source_alone() {
        let dir = TempDir::new("place");
        let source = dir.file("download/movie.mkv", 64);
        let dest = dir.path().join("library/Movie (2024)/Movie (2024).mkv");

        let used = place(&source, &dest, ImportMode::Hardlink).unwrap();
        assert_eq!(used, ImportMode::Hardlink);
        assert!(dest.exists());
        assert!(source.exists(), "the client's copy has to keep seeding");
        assert_eq!(std::fs::read(&dest).unwrap().len(), 64);
    }

    #[test]
    fn an_existing_destination_is_never_overwritten() {
        let dir = TempDir::new("overwrite");
        let source = dir.file("download/movie.mkv", 64);
        let dest = dir.file("library/Movie (2024).mkv", 1);

        let err = place(&source, &dest, ImportMode::Copy).unwrap_err();
        assert!(err.contains("já existe"), "got {err}");
        assert_eq!(
            std::fs::read(&dest).unwrap().len(),
            1,
            "the file that was there is untouched"
        );
    }

    #[test]
    fn copy_produces_an_independent_file() {
        let dir = TempDir::new("copy");
        let source = dir.file("download/movie.mkv", 32);
        let dest = dir.path().join("library/Movie.mkv");

        assert_eq!(
            place(&source, &dest, ImportMode::Copy).unwrap(),
            ImportMode::Copy
        );
        std::fs::write(&source, vec![b'y'; 8]).unwrap();
        assert_eq!(
            std::fs::read(&dest).unwrap().len(),
            32,
            "a copy does not follow the original"
        );
    }

    #[test]
    fn move_takes_the_source_with_it() {
        let dir = TempDir::new("move");
        let source = dir.file("download/movie.mkv", 16);
        let dest = dir.path().join("library/Movie.mkv");

        assert_eq!(
            place(&source, &dest, ImportMode::Move).unwrap(),
            ImportMode::Move
        );
        assert!(dest.exists());
        assert!(!source.exists(), "move is the mode that stops the seed");
    }

    #[test]
    fn the_whole_placement_runs_end_to_end() {
        let dir = TempDir::new("e2e");
        dir.file("download/The.Boys.S04E07.1080p.WEB-DL/sample/s.mkv", 5000);
        dir.file(
            "download/The.Boys.S04E07.1080p.WEB-DL/the.boys.s04e07.mkv",
            128,
        );
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).unwrap();

        let outcome = place_download(
            &dir.path().join("download/The.Boys.S04E07.1080p.WEB-DL"),
            &Placement {
                root: root.clone(),
                title: "The Boys".to_owned(),
                year: Some(2019),
                episode: Some((4, 7)),
                mode: ImportMode::Hardlink,
            },
        );
        match outcome {
            ImportOutcome::Imported { path, .. } => {
                assert_eq!(
                    path,
                    root.join("The Boys (2019)/Season 04/The Boys - S04E07.mkv")
                );
                assert!(path.exists());
            }
            other => panic!("expected an import, got {other:?}"),
        }
    }

    /// A completed grab with an item behind it, ready to be imported.
    async fn pending_grab(pool: &crate::db::Pool) -> Grab {
        use crate::db::grabs::{NewGrab, Protocol};

        let item = library::upsert(pool, &Seed::movie(603, "The Matrix").year(1999).build())
            .await
            .unwrap();
        let provider = crate::db::providers::insert(
            pool,
            crate::db::providers::NewProvider {
                name: "capybara",
                base_url: &url::Url::parse("https://capybarabr.com/").unwrap(),
                api_token: "tok",
                kind: "unit3d",
                plugin_path: None,
            },
        )
        .await
        .unwrap();
        let grab = grabs::reserve(
            pool,
            &NewGrab {
                item_id: item.id,
                episode_id: None,
                season_number: None,
                decision_id: None,
                provider_id: provider.id,
                provider_name: "capybara",
                release_id_remote: "abc",
                release_name: "Matrix.1999.1080p",
                download_url: None,
                protocol: Protocol::Torrent,
            },
        )
        .await
        .unwrap()
        .unwrap();
        grabs::set_status(pool, grab.id, GrabStatus::Completed, None)
            .await
            .unwrap();
        grabs::get_by_id(pool, grab.id).await.unwrap()
    }

    fn test_state(pool: crate::db::Pool) -> AppState {
        AppState::new(pool, brarr_decision_service::Engine::baseline())
    }

    #[tokio::test]
    async fn with_no_root_folder_the_grab_waits_instead_of_failing() {
        let pool = crate::db::open_memory().await.unwrap();
        let grab = pending_grab(&pool).await;
        let state = test_state(pool.clone());

        let summary = import_pending(&state).await.unwrap();
        assert_eq!(summary.considered, 1);
        assert_eq!(summary.waiting, 1);
        assert_eq!(summary.failed, 0);

        // The whole point: an unconfigured brarr must not burn the grab.
        // The moment a root folder exists, the next pass picks it up.
        let after = grabs::get_by_id(&pool, grab.id).await.unwrap();
        assert_eq!(after.status, GrabStatus::Completed);
        assert!(after.imported_path.is_none());
    }

    #[tokio::test]
    async fn a_grab_that_named_no_client_waits_too() {
        let pool = crate::db::open_memory().await.unwrap();
        let grab = pending_grab(&pool).await;
        let dir = TempDir::new("waits");
        root_folders::insert(&pool, dir.path().to_str().unwrap(), None)
            .await
            .unwrap();
        let state = test_state(pool.clone());

        let summary = import_pending(&state).await.unwrap();
        assert_eq!(summary.waiting, 1, "no client to ask where the files are");
        assert_eq!(
            grabs::get_by_id(&pool, grab.id).await.unwrap().status,
            GrabStatus::Completed
        );
    }

    #[test]
    fn a_source_brarr_cannot_see_waits_instead_of_failing() {
        // The production incident, at the unit that decides it. The
        // client reports a path; brarr's container has no such path.
        // Marking this permanent burned a release per sweep.
        let dir = TempDir::new("invisible");
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).unwrap();

        let outcome = place_download(
            &dir.path().join("nao/existe/em/lugar/nenhum"),
            &Placement {
                root,
                title: "The Matrix".to_owned(),
                year: Some(1999),
                episode: None,
                mode: ImportMode::Hardlink,
            },
        );

        assert!(
            matches!(outcome, ImportOutcome::Waiting(_)),
            "a path brarr cannot open is configuration, not a dead release — got {outcome:?}"
        );
    }

    /// The loop this closes, measured in production on 2026-08-15.
    ///
    /// A file sat at exactly the destination brarr computes for this
    /// episode. `place` refused to overwrite it — correctly — and
    /// `place_download` turned that refusal into `Permanent`, which
    /// marks the grab `failed`, which **keeps the barrier key occupied
    /// on purpose** so the next sweep picks a *different* release. That
    /// release downloads, lands on the same file, and fails identically.
    /// Fifty-five grabs, 413.7 GiB, still running when it was found.
    ///
    /// The destination is a coordinate, not a release: a file already
    /// there is this episode, and the answer is to record it rather than
    /// to blame the release that arrived second.
    #[test]
    fn a_destination_that_already_holds_a_file_is_adopted_not_failed() {
        let dir = TempDir::new("already");
        dir.file("download/skeleton.s02e06.mkv", 128);
        let root = dir.path().join("library");
        let occupied = dir.file(
            "library/Skeleton Knight (2022)/Season 02/Skeleton Knight - S02E06.mkv",
            999,
        );

        let outcome = place_download(
            &dir.path().join("download/skeleton.s02e06.mkv"),
            &Placement {
                root,
                title: "Skeleton Knight".to_owned(),
                year: Some(2022),
                episode: Some((2, 6)),
                mode: ImportMode::Hardlink,
            },
        );

        match outcome {
            ImportOutcome::AlreadyPresent { path } => assert_eq!(path, occupied),
            other => panic!("expected the file already there to be adopted, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(&occupied).unwrap().len(),
            999,
            "never overwrite is still the rule — adopting is not writing"
        );
    }

    /// The other half of the same production finding: brarr writes
    /// `Título (ano)/Season 0N` while the library it was pointed at is
    /// Sonarr's, `Título/Season N`. `reuse_existing_season_folder` fixed
    /// the season component and could never fire, because the *series*
    /// folder differed first — so brarr made a second folder per series
    /// and put one episode in it. Eleven series on this operator's disk
    /// ended up split in two.
    #[test]
    fn an_existing_series_folder_is_reused_across_the_year_suffix() {
        let dir = TempDir::new("seriesfolder");
        dir.file("download/skeleton.s02e06.mkv", 128);
        let root = dir.path().join("library");
        // What Sonarr built: no year on the folder, no padding on the
        // season.
        dir.file("library/Skeleton Knight/Season 2/S02E05.mkv", 1);

        let outcome = place_download(
            &dir.path().join("download/skeleton.s02e06.mkv"),
            &Placement {
                root: root.clone(),
                title: "Skeleton Knight".to_owned(),
                year: Some(2022),
                episode: Some((2, 6)),
                mode: ImportMode::Hardlink,
            },
        );

        match outcome {
            ImportOutcome::Imported { path, .. } => assert_eq!(
                path,
                root.join("Skeleton Knight/Season 2/Skeleton Knight - S02E06.mkv"),
                "one folder per series, one folder per season"
            ),
            other => panic!("expected an import, got {other:?}"),
        }
        assert!(
            !root.join("Skeleton Knight (2022)").exists(),
            "the second folder is exactly what must stop being created"
        );
    }

    /// Conservative in the same way the season rule is: it only ever
    /// swaps in a directory that **exists**. With nothing there, brarr
    /// writes its own name, which is what a greenfield library wants.
    #[test]
    fn nothing_on_disk_means_brarr_writes_its_own_folder_name() {
        let dir = TempDir::new("greenfield");
        dir.file("download/movie.mkv", 32);
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).unwrap();

        let outcome = place_download(
            &dir.path().join("download/movie.mkv"),
            &Placement {
                root: root.clone(),
                title: "The Matrix".to_owned(),
                year: Some(1999),
                episode: None,
                mode: ImportMode::Hardlink,
            },
        );

        match outcome {
            ImportOutcome::Imported { path, .. } => {
                assert_eq!(path, root.join("The Matrix (1999)/The Matrix (1999).mkv"));
            }
            other => panic!("expected an import, got {other:?}"),
        }
    }

    /// Found by listing the operator's actual disk while writing this:
    /// `/midias/Filmes` carries `Resident Evil (2002)` beside `Resident
    /// Evil (2023)`, and `Scary Movie (2000)` beside `Scary Movie
    /// (2026)`. Two films, one title. The first version of the rule
    /// stripped the year from both sides, so a third `Resident Evil`
    /// would have been filed inside the 2002 folder — and the movie
    /// naming scheme names the file after the folder, which puts two
    /// different films in one place under one name.
    #[test]
    fn the_same_title_under_another_year_is_another_film() {
        let dir = TempDir::new("remake");
        dir.file("download/movie.mkv", 32);
        let root = dir.path().join("library");
        dir.file("library/Resident Evil (2002)/Resident Evil (2002).mkv", 1);

        let outcome = place_download(
            &dir.path().join("download/movie.mkv"),
            &Placement {
                root: root.clone(),
                title: "Resident Evil".to_owned(),
                year: Some(2023),
                episode: None,
                mode: ImportMode::Hardlink,
            },
        );

        match outcome {
            ImportOutcome::Imported { path, .. } => assert_eq!(
                path,
                root.join("Resident Evil (2023)/Resident Evil (2023).mkv")
            ),
            other => panic!("expected its own folder, got {other:?}"),
        }
    }

    /// And when the disk offers two years for a title brarr holds none
    /// for, readdir order is not a tiebreak. Leaving the path alone is
    /// the same refusal `pick_video` makes for an ambiguous folder.
    #[test]
    fn two_year_variants_are_ambiguous_and_nothing_is_reused() {
        let dir = TempDir::new("ambiguous");
        dir.file("download/movie.mkv", 32);
        let root = dir.path().join("library");
        dir.file("library/Battlestar Galactica (1978)/x.mkv", 1);
        dir.file("library/Battlestar Galactica (2003)/x.mkv", 1);

        let outcome = place_download(
            &dir.path().join("download/movie.mkv"),
            &Placement {
                root: root.clone(),
                title: "Battlestar Galactica".to_owned(),
                year: None,
                episode: None,
                mode: ImportMode::Hardlink,
            },
        );

        match outcome {
            ImportOutcome::Imported { path, .. } => assert_eq!(
                path,
                root.join("Battlestar Galactica/Battlestar Galactica.mkv"),
                "an unresolvable choice is not a choice"
            ),
            other => panic!("expected its own folder, got {other:?}"),
        }
    }

    /// A different film that happens to share a prefix is not the same
    /// film. The rule compares whole names modulo the year suffix, not
    /// prefixes — `Dune` must not adopt `Dune Part Two`'s folder.
    #[test]
    fn a_different_title_is_not_a_year_variant() {
        let dir = TempDir::new("notvariant");
        dir.file("download/movie.mkv", 32);
        let root = dir.path().join("library");
        dir.file("library/Dune Part Two (2024)/x.mkv", 1);

        let outcome = place_download(
            &dir.path().join("download/movie.mkv"),
            &Placement {
                root: root.clone(),
                title: "Dune".to_owned(),
                year: Some(2021),
                episode: None,
                mode: ImportMode::Hardlink,
            },
        );

        match outcome {
            ImportOutcome::Imported { path, .. } => {
                assert_eq!(path, root.join("Dune (2021)/Dune (2021).mkv"));
            }
            other => panic!("expected an import, got {other:?}"),
        }
    }

    #[test]
    fn a_folder_with_no_video_is_still_permanent() {
        // The other side of the split: this one really will never work,
        // and waiting on it forever would be its own bug.
        let dir = TempDir::new("novideo");
        dir.file("Release/leiame.txt", 10);
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).unwrap();

        let outcome = place_download(
            &dir.path().join("Release"),
            &Placement {
                root,
                title: "The Matrix".to_owned(),
                year: Some(1999),
                episode: None,
                mode: ImportMode::Hardlink,
            },
        );

        assert!(
            matches!(outcome, ImportOutcome::Permanent(_)),
            "got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn waiting_records_a_reason_the_operator_can_act_on() {
        let pool = crate::db::open_memory().await.unwrap();
        let grab = pending_grab(&pool).await;
        let state = test_state(pool.clone());

        import_pending(&state).await.unwrap();

        let after = grabs::get_by_id(&pool, grab.id).await.unwrap();
        assert_eq!(after.status, GrabStatus::Completed);
        assert!(
            after.import_wait_reason.is_some(),
            "waiting used to be invisible — it lived in a debug! log while five \
             finished downloads sat on disk with nothing in the UI able to say why"
        );
        assert!(
            after.error.is_none(),
            "waiting is not failing, so the failure column stays empty"
        );
    }

    #[tokio::test]
    async fn a_stuck_grab_does_not_starve_the_import_queue() {
        // `awaiting_import` used to order by `updated_at` alone. Waiting
        // does not move `updated_at`, so with MAX_IMPORTS_PER_PASS = 3
        // the same three stuck rows were retried forever and everything
        // behind them was never attempted.
        let pool = crate::db::open_memory().await.unwrap();
        let state = test_state(pool.clone());

        let first = pending_grab(&pool).await;
        import_pending(&state).await.unwrap();

        let after = grabs::get_by_id(&pool, first.id).await.unwrap();
        assert!(
            after.import_attempted_at.is_some(),
            "the importer needs its own clock, distinct from updated_at"
        );
        assert_eq!(
            after.updated_at, first.updated_at,
            "an attempt is not a state change: moving updated_at would keep \
             resetting the queue's MISSING_GRACE window"
        );

        // A never-attempted grab sorts ahead of the stuck one.
        let queue = grabs::awaiting_import(&pool).await.unwrap();
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn the_mode_label_round_trips_and_bad_input_is_the_safe_default() {
        for mode in [ImportMode::Hardlink, ImportMode::Copy, ImportMode::Move] {
            assert_eq!(ImportMode::from_label(mode.label()), mode);
        }
        assert_eq!(
            ImportMode::from_label("qualquer coisa"),
            ImportMode::Hardlink,
            "a bad setting must not become `move` and kill a seed"
        );
    }
}
