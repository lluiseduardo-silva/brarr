//! Taking ownership of files that were already on disk when brarr
//! arrived.
//!
//! [`crate::import`] opens by saying brarr never has to interpret a
//! name: it only imports what it downloaded itself, and the grab already
//! carries `item_id` and `episode_id`. Adoption breaks that, and there
//! is no version of the feature that does not. Three things bound the
//! opening:
//!
//! 1. It lives entirely in this module. [`parse_marker`] has no caller
//!    outside it, and `import::plan_and_place` still reads the grab and
//!    never a name.
//! 2. The question is smaller. Radarr asks "what is this file?" in an
//!    open world. Here the operator picks the title; the only unknowns
//!    are two small integers.
//! 3. Nothing automatic reaches this module. Adoption runs only with the
//!    operator pressing a button, twice, after reading a list of full
//!    paths.
//!
//! ## The rule, in the operator's words
//!
//! > "Se o diretório do arquivo é diferente do diretório de destino ele
//! > cria o hardlink. Se o arquivo já está na pasta de saída correta
//! > vai apenas adicionar sem mover/criar hardlink."
//!
//! "Already in the destination folder" means **under a registered root
//! folder that serves the item** — not "at the path `destination()`
//! would build". The strict reading is the one that destroys a real
//! library: measured over this operator's 7 215 files, not one is at the
//! path brarr would generate, so every one of them would be classified
//! as outside and hardlinked into a parallel tree.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::db::grabs::{self, Grab, GrabTarget};
use crate::db::library::{self, Episode, LibraryItem, MediaType};
use crate::db::{episode_numbering, root_folders};
use crate::episode_match::EpisodeMatcher;
use crate::scan;
use crate::{AppError, AppState};

/// What the import would do with one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AdoptAction {
    /// The file is already in the library. Record the path it has and
    /// touch nothing — no move, no rename, no link.
    ///
    /// The variant deliberately carries **no destination**: there is no
    /// value to hand [`link_in`], so this branch cannot write even by
    /// mistake. The guarantee is the type's, not a comment's.
    InPlace,
    /// Outside the library. Build the Plex/Jellyfin path and hardlink.
    Link {
        /// Exactly what `import::destination` builds.
        destination: PathBuf,
    },
    /// There is a target in the catalogue, but brarr will not write: no
    /// root folder serves this item. It never guesses a destination.
    Blocked(String),
}

/// Everything the rule needs that does not come from disk.
///
/// A struct rather than five parameters because `clippy.toml` pins
/// `too-many-arguments-threshold = 6`.
#[derive(Debug, Clone)]
pub(crate) struct AdoptContext {
    /// Where a linked file would land. `None` when no root folder serves
    /// this item.
    pub root: Option<PathBuf>,
    /// **Every** registered root folder serving this item's media type,
    /// already canonicalised. A file under any of them *is* the library.
    pub library_roots: Vec<PathBuf>,
    /// Catalogue title, for the destination path.
    pub title: String,
    /// Release year, for the folder name.
    pub year: Option<i32>,
}

/// Adopt where it stands, or link it in?
///
/// The predicate is containment in a registered root folder. That is
/// what "library" means to brarr — `db::root_folders` validates those
/// paths at registration and `import` writes inside them — and it says
/// nothing about layout, so any naming convention survives.
///
/// **Every** root serving the type is considered, not just the one
/// `root_folders::pick` would answer with. Nothing in the schema stops
/// two series roots, and 19 TB is exactly the collection that lives on
/// two disks. Comparing against one would make the series on the second
/// disk "outside", and hardlinking across disks is a copy.
pub(crate) fn action_for(
    source: &Path,
    episode: Option<(u16, u16)>,
    ctx: &AdoptContext,
) -> AdoptAction {
    let here = real(source);
    if ctx.library_roots.iter().any(|root| here.starts_with(root)) {
        return AdoptAction::InPlace;
    }
    let Some(root) = ctx.root.as_deref() else {
        return AdoptAction::Blocked("nenhuma pasta raiz serve este tipo".to_owned());
    };
    let extension = source
        .extension()
        .map_or_else(|| "mkv".to_owned(), |e| e.to_string_lossy().to_lowercase());
    AdoptAction::Link {
        destination: crate::import::destination(root, &ctx.title, ctx.year, episode, &extension),
    }
}

/// `canonicalize` where it works, the literal path where it does not.
///
/// Both sides of the comparison go through here — the file in
/// [`action_for`], the roots when the [`AdoptContext`] is built. Without
/// it, a root registered as `/data/series` that is a symlink to
/// `/mnt/disk1/series` makes the whole library read as "outside", and
/// every episode gets a parallel hardlink. Bind mounts and symlink farms
/// are normal on the Docker target.
///
/// `canonicalize` resolves symlinks and `..`; it does **not** resolve
/// hardlinks, which is what is wanted — a hardlink is a real directory
/// entry with a path of its own.
///
/// A path that will not canonicalise (unmounted, no permission on the
/// parent) falls back to the literal rather than silently containing
/// nothing.
pub(crate) fn real(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Link the file into the library — and stop if it cannot.
///
/// **This is not `import::place`, and that is the most important
/// difference in the module.** `place` degrades *any* `hard_link` error
/// to a copy and reports `ImportMode::Copy`. That is right for a
/// download the client will delete anyway, and wrong here.
///
/// The incident that produced this feature was five episodes copied in
/// silence, ~7 GB duplicated, because `fs.protected_hardlinks=1` refuses
/// a link to a file the container's uid neither owns nor can write.
/// Pressing a button does not fix the container's configuration: if
/// adoption fell back to copying, the repair would reproduce the bug. A
/// refusal the operator sees and fixes beats a copy they discover on a
/// disk usage graph.
///
/// `ImportMode::Move` is unreachable from here — the module does not
/// import the type. Adoption operates on the operator's own file, and
/// `Move` would kill a private tracker seed, irreversibly, behind a
/// button labelled "import".
///
/// The errno is called out separately because the two causes ask for
/// opposite actions: `EXDEV` is "move the file first", `EPERM` is "fix
/// the file's owner or `fs.protected_hardlinks`" — the incident's case.
pub(crate) fn link_in(source: &Path, destination: &Path) -> Result<(), String> {
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
    std::fs::hard_link(source, destination).map_err(|e| {
        let hint = match e.raw_os_error() {
            Some(18) => {
                " — origem e destino estão em sistemas de arquivos diferentes; mova o arquivo \
                 para dentro da pasta raiz e importe de novo"
            }
            Some(1) => {
                " — o dono do arquivo não é o brarr e `fs.protected_hardlinks` está ligado; \
                 ajuste o uid/permissão da origem"
            }
            _ => "",
        };
        format!(
            "não consegui criar o hardlink em {}: {e}{hint}",
            destination.display()
        )
    })
}

/// Refuse a folder the scan cannot make sense of.
///
/// The plan this module grew from refused a registered root folder here,
/// because the per-item flow it described picked "the largest video that
/// is not a sample" and pointing that at 19 TB would offer the biggest
/// remux in the collection as the movie. The importer assigns a title
/// per file instead, so there is no heuristic left to fool and adopting
/// an existing library means pointing exactly at a root. What survives
/// is the check that the path is a readable directory.
///
/// # Errors
///
/// [`AppError::InvalidInput`] when the path is not a directory brarr can
/// read.
pub(crate) fn validate_folder(folder: &Path) -> Result<(), AppError> {
    if !folder.is_absolute() {
        return Err(AppError::InvalidInput(format!(
            "{} não é um caminho absoluto",
            folder.display()
        )));
    }
    if !folder.is_dir() {
        return Err(AppError::InvalidInput(format!(
            "{} não é um diretório acessível — em Docker, use o caminho de dentro do contêiner",
            folder.display()
        )));
    }
    if std::fs::read_dir(folder).is_err() {
        return Err(AppError::InvalidInput(format!(
            "{} existe mas o brarr não consegue ler — confira o uid do contêiner",
            folder.display()
        )));
    }
    Ok(())
}

/// Why a name yielded no usable episode marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerError {
    /// No `SxxEyy` / `1x02` anywhere. Absolute anime numbering, single
    /// file season packs and date-based episodes all land here.
    Absent,
    /// More than one distinct marker, or a chained `S01E01E02`. One file
    /// cannot hold two barrier keys, and recording it against one of the
    /// episodes would leave the other looking unacquired.
    Ambiguous,
}

impl MarkerError {
    /// What the importer shows in the row's reason column.
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Absent => "não consegui ler temporada/episódio do nome",
            Self::Ambiguous => "o nome cita mais de um episódio",
        }
    }
}

/// The single episode a name points at.
///
/// Deliberately strict: this pre-fills a form field the operator can
/// override, so a wrong guess costs more than an empty one. Everything
/// it refuses shows up in the importer with the reason and an empty
/// picker, which is a decision the operator can make in one click.
pub(crate) fn parse_marker(file_name: &str) -> Result<(u16, u16), MarkerError> {
    let markers = scan::episode_markers(file_name);
    let Some(first) = markers.first() else {
        return Err(MarkerError::Absent);
    };
    if first.chained {
        return Err(MarkerError::Ambiguous);
    }
    let distinct: HashSet<(u16, u16)> = markers.iter().map(|m| (m.season, m.episode)).collect();
    if distinct.len() > 1 {
        return Err(MarkerError::Ambiguous);
    }
    Ok((first.season, first.episode))
}

/// One video the scan found.
#[derive(Debug, Clone)]
pub(crate) struct FoundFile {
    /// Absolute path on the machine running brarr.
    pub path: PathBuf,
    /// Size in bytes, as the walk saw it.
    pub size: u64,
    /// The episode the file name names, when it names exactly one.
    pub marker: Result<(u16, u16), MarkerError>,
}

/// Ceiling on how many files one preview will carry.
///
/// A root folder holds thousands of videos and the dialog is a form: at
/// some point the page stops being usable and the browser stops being
/// happy. What does not fit is **reported**, never silently dropped —
/// the operator narrows the folder and goes again.
pub(crate) const MAX_PREVIEW_FILES: usize = 400;

/// What one folder holds, ready to render.
#[derive(Debug, Clone)]
pub(crate) struct FolderScan {
    /// Files to offer, sorted by path so the order is stable between the
    /// preview and the commit.
    pub files: Vec<FoundFile>,
    /// Videos found beyond [`MAX_PREVIEW_FILES`].
    pub over_cap: usize,
    /// Videos skipped because the operator set them aside.
    pub ignored: usize,
}

/// Walk a folder and describe every video in it.
///
/// Synchronous on purpose: the caller runs it inside `spawn_blocking`,
/// because a `stat` on a network mount is fast right up until it is not,
/// and a stalled NFS call on a runtime worker takes the process with it.
pub(crate) fn scan_folder(folder: &Path, ignored: &HashSet<String>) -> FolderScan {
    let mut found = Vec::new();
    crate::import::collect_videos(folder, &mut found, 0);
    found.sort_by(|a, b| a.0.cmp(&b.0));

    let mut skipped = 0;
    let mut files = Vec::new();
    let mut over_cap = 0;
    for (path, size) in found {
        if crate::import::looks_like_sample(&path, folder) {
            continue;
        }
        if ignored.contains(&path.to_string_lossy().to_string()) {
            skipped += 1;
            continue;
        }
        if files.len() >= MAX_PREVIEW_FILES {
            over_cap += 1;
            continue;
        }
        let marker = path.file_name().map_or(Err(MarkerError::Absent), |n| {
            parse_marker(&n.to_string_lossy())
        });
        files.push(FoundFile { path, size, marker });
    }
    FolderScan {
        files,
        over_cap,
        ignored: skipped,
    }
}

/// Everything a preview row renders.
#[derive(Debug, Clone)]
pub struct PlannedFile {
    /// Absolute path on disk.
    pub path: PathBuf,
    /// File name, which is what the row shows.
    pub name: String,
    /// Size in bytes.
    pub size: u64,
    /// Round-trip token carrying the target and the fingerprint.
    /// `None` when the row cannot be written yet — no title, no
    /// episode, already covered, or nowhere to put it.
    pub token: Option<String>,
    /// Catalogue entry this file was matched to, when one was.
    pub item_id: Option<Uuid>,
    /// Its title, for the row.
    pub title: Option<String>,
    /// `true` when the matched item is a series, which is what decides
    /// whether the season and episode cells mean anything.
    pub is_series: bool,
    /// Season the name claims.
    pub season: Option<u16>,
    /// Episode the name claims.
    pub episode: Option<u16>,
    /// Why the name yielded no episode, when it yielded none.
    pub marker_error: Option<MarkerError>,
    /// The catalogue episode those two resolve to.
    pub episode_id: Option<Uuid>,
    /// `4 — The Insider`, for the row.
    pub episode_label: Option<String>,
    /// Why this row still needs a human.
    pub reason: Option<String>,
    /// What would happen on confirm, already worded for the row.
    pub effect: Option<String>,
    /// Where a link would land. `None` for an in-place adoption, which
    /// is what carries [`AdoptAction::InPlace`]'s guarantee into the
    /// commit: with no destination there is nothing to write.
    pub link_destination: Option<PathBuf>,
    /// A live grab already covers this target.
    pub covered: bool,
}

/// What one folder would produce.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Folder that was scanned.
    pub folder: PathBuf,
    /// One row per offered file.
    pub files: Vec<PlannedFile>,
    /// Videos beyond [`MAX_PREVIEW_FILES`] — reported, never dropped.
    pub over_cap: usize,
    /// Videos the operator had set aside.
    pub ignored: usize,
}

impl Plan {
    /// Rows that would be written if the operator confirmed now.
    #[must_use]
    pub fn ready(&self) -> usize {
        self.files.iter().filter(|f| f.token.is_some()).count()
    }

    /// Rows waiting on a decision.
    #[must_use]
    pub fn undecided(&self) -> usize {
        self.files
            .iter()
            .filter(|f| f.token.is_none() && !f.covered)
            .count()
    }

    /// Rows a live grab already covers.
    #[must_use]
    pub fn covered(&self) -> usize {
        self.files.iter().filter(|f| f.covered).count()
    }
}

/// A confirmed row, decoded from the form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pick {
    /// Catalogue entry the operator assigned.
    pub item_id: Uuid,
    /// Episode, for a series.
    pub episode_id: Option<Uuid>,
    /// Size at preview time.
    pub size: u64,
    /// Absolute path.
    pub path: String,
}

impl Pick {
    /// `{item}|{episode}|{size}|{path}`, with `splitn(4, '|')` so the
    /// path — the only field that can contain a `|` — absorbs the rest.
    ///
    /// The fingerprint travels **per row**, not as a digest of the whole
    /// plan. A single digest would let one partial file that qBittorrent
    /// is still writing into the same folder veto the entire
    /// confirmation, which is exactly the repair scenario.
    fn encode(item_id: Uuid, episode_id: Option<Uuid>, size: u64, path: &str) -> String {
        let episode = episode_id.map_or_else(|| "-".to_owned(), |e| e.to_string());
        format!("{item_id}|{episode}|{size}|{path}")
    }

    /// Read one back. Anything malformed is dropped rather than guessed.
    #[must_use]
    pub fn decode(raw: &str) -> Option<Self> {
        let mut parts = raw.splitn(4, '|');
        let item_id = Uuid::parse_str(parts.next()?).ok()?;
        let episode = parts.next()?;
        let episode_id = if episode == "-" {
            None
        } else {
            Some(Uuid::parse_str(episode).ok()?)
        };
        let size = parts.next()?.parse().ok()?;
        let path = parts.next()?.to_owned();
        Some(Self {
            item_id,
            episode_id,
            size,
            path,
        })
    }
}

/// Normalised form used to match a file against a catalogue title.
///
/// Lowercase, accents folded to ASCII where it is cheap, and every run
/// of non-alphanumerics collapsed to a single space. `Duna: Parte Dois`
/// and `Duna.Parte.Dois.2024.2160p` both reduce to something the
/// containment test can see.
fn normalise(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_space = false;
    for ch in raw.chars() {
        let folded = match ch {
            'á' | 'à' | 'â' | 'ã' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            other => other,
        };
        if folded.is_ascii_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(folded.to_ascii_lowercase());
        } else {
            pending_space = true;
        }
    }
    out
}

/// Which catalogue entry a file belongs to, if any is obvious.
///
/// The haystack is the file name plus the folders between it and the
/// scanned root, because a series folder often carries the title the
/// file name abbreviates. **Longest title wins**: `Duna` and
/// `Duna: Parte Dois` both match a file called
/// `Duna.Parte.Dois.2024.mkv`, and the shorter one is the wrong answer.
///
/// This only pre-fills a picker. Anything it declines shows up with an
/// empty title cell, which is one click for the operator and never a
/// wrong adoption.
fn match_item<'a>(path: &Path, base: &Path, library: &'a [LibraryItem]) -> Option<&'a LibraryItem> {
    let relative = path.strip_prefix(base).unwrap_or(path);
    let haystack = normalise(&relative.to_string_lossy());
    library
        .iter()
        .filter(|item| {
            let title = normalise(&item.title);
            !title.is_empty() && haystack.contains(&title)
        })
        .max_by_key(|item| normalise(&item.title).len())
}

/// What one folder would produce, without writing anything.
///
/// **Writes nothing.** No probe file, no test link, no row in `grabs` —
/// except the stale-reservation sweep, which only removes debris from an
/// interrupted run.
///
/// All the disk work runs inside a single `spawn_blocking`: a `stat` is
/// fast, but on a network mount it is fast right up until it is not, and
/// a stalled NFS call on a runtime worker takes the process with it.
///
/// # Errors
///
/// [`AppError::InvalidInput`] when the folder is not readable,
/// [`AppError::Database`] on SQL failure.
pub async fn plan(
    state: &AppState,
    folder: &Path,
    forced_item: Option<Uuid>,
) -> Result<Plan, AppError> {
    validate_folder(folder)?;
    let pool = state.pool();
    let ignored = crate::db::ignored_paths::paths(pool).await?;
    let library = library::list(pool).await?;
    let roots = root_folders::list_all(pool).await?;

    let owned = folder.to_path_buf();
    let scan = tokio::task::spawn_blocking(move || scan_folder(&owned, &ignored))
        .await
        .map_err(|e| AppError::InvalidInput(format!("varredura do disco falhou: {e}")))?;

    // Assign titles first, then load the catalogue once per distinct
    // item: a folder of 300 episodes of one series must not ask for the
    // same episode list 300 times.
    let assigned: Vec<Option<&LibraryItem>> = scan
        .files
        .iter()
        .map(|found| match forced_item {
            Some(id) => library.iter().find(|i| i.id == id),
            None => match_item(&found.path, folder, &library),
        })
        .collect();

    let mut episodes: HashMap<Uuid, Vec<Episode>> = HashMap::new();
    let mut matchers: HashMap<Uuid, EpisodeMatcher> = HashMap::new();
    let mut live: HashMap<Uuid, Vec<Grab>> = HashMap::new();
    let distinct: HashSet<Uuid> = assigned.iter().flatten().map(|i| i.id).collect();
    for id in distinct {
        let eps = library::episodes(pool, id).await?;
        let reverse = episode_numbering::reverse_for_item(pool, id).await?;
        matchers.insert(id, EpisodeMatcher::new(&eps, reverse));
        episodes.insert(id, eps);
        live.insert(id, grabs::live_for_item(pool, id).await?);
    }

    let mut files = Vec::with_capacity(scan.files.len());
    for (found, item) in scan.files.iter().zip(assigned) {
        let row = build_row(state, found, item, folder, &roots).await?;
        files.push(resolve_target(row, item, &episodes, &matchers, &live));
    }

    Ok(Plan {
        folder: folder.to_path_buf(),
        files,
        over_cap: scan.over_cap,
        ignored: scan.ignored,
    })
}

/// The immediate subdirectories of `folder`, for navigating to the
/// folder *before* committing to a scan.
///
/// One `read_dir`, no recursion, no per-file `stat`. Opening the dialog
/// used to walk the whole tree before the operator had even said which
/// folder they meant — pointed at a root holding 3 982 files, that is
/// thousands of syscalls to answer a question nobody asked yet.
///
/// # Errors
///
/// [`AppError::InvalidInput`] when the folder is not a readable
/// directory.
pub async fn list_dirs(folder: &Path) -> Result<Vec<(String, PathBuf)>, AppError> {
    validate_folder(folder)?;
    let owned = folder.to_path_buf();
    let dirs = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&owned) else {
            return out;
        };
        for entry in entries.flatten() {
            // `file_type` comes from the dirent on every platform brarr
            // targets, so this does not stat.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if !kind.is_dir() {
                continue;
            }
            out.push((
                entry.file_name().to_string_lossy().to_string(),
                entry.path(),
            ));
        }
        out.sort_by_key(|(name, _)| name.to_lowercase());
        out
    })
    .await
    .map_err(|e| AppError::InvalidInput(format!("leitura da pasta falhou: {e}")))?;
    Ok(dirs)
}

/// Rebuild one row after the operator assigned a target by hand.
///
/// Cheaper than re-planning the folder — it stats one file instead of
/// walking hundreds — and it is also the authorisation boundary for the
/// pickers: the path **must** live under the folder being imported, so
/// the endpoint cannot be turned into "adopt any path on this machine".
///
/// # Errors
///
/// [`AppError::InvalidInput`] when the path is outside the folder or
/// gone, [`AppError::NotFound`] when the target does not belong to the
/// item, [`AppError::Database`] on SQL failure.
pub async fn plan_one(
    state: &AppState,
    folder: &Path,
    path: &Path,
    item_id: Option<Uuid>,
    episode_id: Option<Uuid>,
) -> Result<PlannedFile, AppError> {
    validate_folder(folder)?;
    if !real(path).starts_with(real(folder)) {
        return Err(AppError::InvalidInput(format!(
            "{} está fora da pasta que está sendo importada",
            path.display()
        )));
    }
    let size = std::fs::metadata(path)
        .map_err(|e| AppError::InvalidInput(format!("não consegui ler {}: {e}", path.display())))?
        .len();

    let pool = state.pool();
    let item = match item_id {
        Some(id) => Some(library::get_by_id(pool, id).await?),
        None => None,
    };
    let found = FoundFile {
        marker: path.file_name().map_or(Err(MarkerError::Absent), |n| {
            parse_marker(&n.to_string_lossy())
        }),
        path: path.to_path_buf(),
        size,
    };
    let roots = root_folders::list_all(pool).await?;
    let mut row = build_row(state, &found, item.as_ref(), folder, &roots).await?;

    let Some(item) = item else { return Ok(row) };
    let episodes = library::episodes(pool, item.id).await?;
    // A hand-picked episode wins over whatever the file name claimed —
    // that is the whole point of the picker — but it still has to belong
    // to this item, or the barrier key would name a target the catalogue
    // does not have.
    if let Some(chosen) = episode_id {
        let Some(ep) = episodes.iter().find(|e| e.id == chosen) else {
            return Err(AppError::NotFound(format!(
                "episódio {chosen} não pertence a {}",
                item.title
            )));
        };
        row.season = u16::try_from(ep.season_number).ok();
        row.episode = u16::try_from(ep.episode_number).ok();
        row.marker_error = None;
    }
    let reverse = episode_numbering::reverse_for_item(pool, item.id).await?;
    let mut matchers = HashMap::new();
    matchers.insert(item.id, EpisodeMatcher::new(&episodes, reverse));
    let mut by_item = HashMap::new();
    by_item.insert(item.id, episodes);
    let mut live = HashMap::new();
    live.insert(item.id, grabs::live_for_item(pool, item.id).await?);
    Ok(resolve_target(row, Some(&item), &by_item, &matchers, &live))
}

/// One episode the picker offers.
#[derive(Debug, Clone)]
pub struct EpisodeSlot {
    /// Catalogue id, which the picker posts back.
    pub id: Uuid,
    /// Season it belongs to.
    pub season: i32,
    /// Number within the season.
    pub number: i32,
    /// Episode title, when TMDB has one.
    pub title: Option<String>,
    /// A live grab already covers it.
    ///
    /// Shown rather than hidden: the plan's `<select>` listed only free
    /// slots, which answers "where can this go" but not "why is E01
    /// missing from the list". Seeing both halves is also what stops the
    /// operator pointing two files at one episode.
    pub taken: bool,
}

/// Every episode of one season, with what already covers it.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn episode_slots(
    state: &AppState,
    item_id: Uuid,
    season: Option<i32>,
) -> Result<Vec<EpisodeSlot>, AppError> {
    let pool = state.pool();
    let live = grabs::live_for_item(pool, item_id).await?;
    Ok(library::episodes(pool, item_id)
        .await?
        .into_iter()
        // Season 0 is TMDB's specials bucket, excluded here exactly as
        // the sweep excludes it.
        .filter(|e| e.season_number > 0)
        .filter(|e| season.is_none_or(|s| e.season_number == s))
        .map(|e| EpisodeSlot {
            taken: live
                .iter()
                .any(|g| grabs::covers(g, GrabTarget::episode(e.id, e.season_number))),
            id: e.id,
            season: e.season_number,
            number: e.episode_number,
            title: e.title,
        })
        .collect())
}

/// The half of a row that does not depend on the catalogue lookups.
async fn build_row(
    state: &AppState,
    found: &FoundFile,
    item: Option<&LibraryItem>,
    base: &Path,
    roots: &[root_folders::RootFolder],
) -> Result<PlannedFile, AppError> {
    let name = found.path.file_name().map_or_else(
        || found.path.to_string_lossy().to_string(),
        |n| n.to_string_lossy().to_string(),
    );
    let (season, episode, marker_error) = match found.marker {
        Ok((s, e)) => (Some(s), Some(e), None),
        Err(why) => (None, None, Some(why)),
    };
    let mut row = PlannedFile {
        path: found.path.clone(),
        name,
        size: found.size,
        token: None,
        item_id: item.map(|i| i.id),
        title: item.map(|i| i.title.clone()),
        is_series: item.is_some_and(|i| i.media_type == MediaType::Tv),
        season,
        episode,
        marker_error,
        episode_id: None,
        episode_label: None,
        reason: None,
        effect: None,
        link_destination: None,
        covered: false,
    };
    let Some(item) = item else {
        row.reason = Some("escolher título".to_owned());
        return Ok(row);
    };
    // The effect the operator sees is a function of the title they
    // picked, so it can only be computed once there is one.
    let root = crate::import::resolve_root(state, item).await?;
    let ctx = AdoptContext {
        root: root.map(|r| r.path),
        library_roots: roots
            .iter()
            .filter(|r| r.media_type.is_none() || r.media_type == Some(item.media_type))
            .map(|r| real(&r.path))
            .collect(),
        title: item.title.clone(),
        year: item.year,
    };
    let marker = found.marker.ok();
    match action_for(&found.path, marker, &ctx) {
        AdoptAction::InPlace => row.effect = Some("manter no lugar".to_owned()),
        AdoptAction::Link { destination } => {
            let shown = destination
                .strip_prefix(base)
                .unwrap_or(&destination)
                .to_string_lossy()
                .to_string();
            row.effect = Some(format!("vincular em {shown}"));
            row.link_destination = Some(destination);
        }
        AdoptAction::Blocked(why) => row.reason = Some(why),
    }
    Ok(row)
}

/// Tie the row to an episode and to the coverage already in `grabs`.
///
/// The coordinates in `row` are what the **file name** says, and a name
/// follows the numbering the release used — which is not always the
/// catalogue's. `matchers` is what reconciles the two, so a folder of
/// `S02E01`-style names lands on the right rows of a catalogue that
/// numbers the same episodes 15 upwards.
fn resolve_target(
    mut row: PlannedFile,
    item: Option<&LibraryItem>,
    episodes: &HashMap<Uuid, Vec<Episode>>,
    matchers: &HashMap<Uuid, EpisodeMatcher>,
    live: &HashMap<Uuid, Vec<Grab>>,
) -> PlannedFile {
    let Some(item) = item else { return row };
    let target = if row.is_series {
        let Some((season, number)) = row.season.zip(row.episode) else {
            // Say *why* the episode is missing. "Escolher episódio" alone
            // makes absolute anime numbering and a two-episode file look
            // like the same problem, and they are not.
            row.reason = Some(
                row.marker_error
                    .map_or_else(|| "escolher episódio".to_owned(), |e| e.reason().to_owned()),
            );
            return row;
        };
        let found = matchers
            .get(&item.id)
            .and_then(|m| m.resolve(i32::from(season), i32::from(number), None))
            .and_then(|id| {
                episodes
                    .get(&item.id)
                    .and_then(|eps| eps.iter().find(|e| e.id == id))
            });
        let Some(ep) = found else {
            row.reason = Some(format!("S{season:02}E{number:02} não existe no catálogo"));
            return row;
        };
        row.episode_id = Some(ep.id);
        // The catalogue's number, not the name's: under an alternate
        // ordering they differ, and the picker beside this label lists
        // catalogue numbers.
        row.episode_label = Some(match &ep.title {
            Some(t) => format!("{} — {t}", ep.episode_number),
            None => ep.episode_number.to_string(),
        });
        GrabTarget::episode(ep.id, ep.season_number)
    } else {
        GrabTarget::item()
    };

    if let Some(rows) = live.get(&item.id) {
        if rows.iter().any(|g| grabs::covers(g, target)) {
            row.covered = true;
            row.reason = Some("já na biblioteca".to_owned());
            row.effect = None;
            return row;
        }
    }
    if row.reason.is_none() {
        row.token = Some(Pick::encode(
            item.id,
            row.episode_id,
            row.size,
            &row.path.to_string_lossy(),
        ));
    }
    row
}

/// Undo one adoption.
///
/// In place: delete the row. **Nothing on disk is touched** — brarr
/// wrote nothing, so deleting the row is the complete undo. That is the
/// strongest practical argument for the adopt-where-it-stands rule.
///
/// Linked: the link brarr created is at `imported_path`. Before removing
/// it, the destination's inode and device are compared against the
/// source's. Different means someone replaced that path between the
/// adoption and the undo, and deleting it would destroy a file that is
/// not brarr's. On platforms without inodes the removal is refused and
/// the path is named, so the operator can do it by hand.
///
/// Leaving the link instead of removing it would be worse: the next
/// import would find it *inside* the root and adopt it in place,
/// resurrecting the match the operator just rejected.
///
/// # Errors
///
/// [`AppError::NotFound`] when the grab is not an adoption,
/// [`AppError::InvalidInput`] when the link cannot be safely removed,
/// [`AppError::Database`] on SQL failure.
pub async fn undo(state: &AppState, grab_id: Uuid) -> Result<String, AppError> {
    let grab = grabs::get_by_id(state.pool(), grab_id).await?;
    if grab.protocol != crate::db::grabs::Protocol::Local {
        return Err(AppError::NotFound(format!("adoção {grab_id}")));
    }
    if grabs::is_in_place(&grab) {
        grabs::delete_adopted(state.pool(), grab_id).await?;
        return Ok("esquecido — o arquivo continua no disco, intacto".to_owned());
    }
    let Some(link) = grab.imported_path.clone() else {
        grabs::delete_adopted(state.pool(), grab_id).await?;
        return Ok("esquecido — nada tinha sido gravado".to_owned());
    };
    let source = grab.release_id_remote.clone();
    let removed = tokio::task::spawn_blocking(move || remove_link(&source, &link))
        .await
        .map_err(|e| AppError::InvalidInput(format!("remoção falhou: {e}")))?;
    match removed {
        Ok(message) => {
            grabs::delete_adopted(state.pool(), grab_id).await?;
            Ok(message)
        }
        Err(why) => Err(AppError::InvalidInput(why)),
    }
}

/// Remove the hardlink brarr created, and only that.
fn remove_link(source: &str, link: &str) -> Result<String, String> {
    let link_path = Path::new(link);
    if !link_path.exists() {
        return Ok("o vínculo já não existia".to_owned());
    }
    if !same_file(Path::new(source), link_path)? {
        return Err(format!(
            "{link} não é mais o vínculo que o brarr criou — alguém substituiu esse caminho. \
             Remova à mão se for o caso."
        ));
    }
    std::fs::remove_file(link_path)
        .map(|()| format!("vínculo removido de {link}"))
        .map_err(|e| format!("não consegui remover {link}: {e}"))
}

/// Are these two paths the same bytes — the same inode on the same
/// device?
#[cfg(unix)]
fn same_file(source: &Path, link: &Path) -> Result<bool, String> {
    use std::os::unix::fs::MetadataExt as _;
    let a = std::fs::metadata(source)
        .map_err(|e| format!("não consegui ler {}: {e}", source.display()))?;
    let b =
        std::fs::metadata(link).map_err(|e| format!("não consegui ler {}: {e}", link.display()))?;
    Ok(a.ino() == b.ino() && a.dev() == b.dev())
}

/// Windows has no cheap inode. Refusing is the honest answer: the whole
/// point of the check is to avoid deleting a file that is not the link
/// brarr made, and "probably" is not good enough for `remove_file`.
#[cfg(not(unix))]
fn same_file(_source: &Path, link: &Path) -> Result<bool, String> {
    Err(format!(
        "nesta plataforma o brarr não consegue confirmar que {} ainda é o vínculo que criou; \
         remova à mão e depois desfaça",
        link.display()
    ))
}

/// How one confirmed file ended up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitStatus {
    /// Recorded where it already was. No filesystem call at all.
    InPlace,
    /// Hardlinked into the library.
    Linked,
    /// Nothing was written, and the detail says why.
    Skipped,
}

/// One line of the report.
#[derive(Debug, Clone)]
pub struct CommitOutcome {
    /// File name, for the row.
    pub name: String,
    /// What happened.
    pub status: CommitStatus,
    /// Where it landed, or why it did not.
    pub detail: String,
}

/// What a confirmation did.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// One line per submitted file.
    pub outcomes: Vec<CommitOutcome>,
    /// Files that appeared in the folder between the preview and the
    /// confirmation. Reported, because the operator did not see them and
    /// cannot have meant to include them.
    pub appeared: usize,
}

impl Report {
    /// How many rows ended in a given status.
    #[must_use]
    pub fn count(&self, status: CommitStatus) -> usize {
        self.outcomes.iter().filter(|o| o.status == status).count()
    }
}

/// Write what the operator confirmed.
///
/// The plan is rebuilt from scratch first. The preview is a report,
/// never a promise — and re-planning is also the authorisation check:
/// only paths brarr's own scan produced can be written, so this is not
/// an oracle for "record any path as my library".
///
/// Guards, in order, before anything is written: the token must appear
/// **identically** in the fresh plan (path, target and fingerprint), the
/// target must be legal for the item's media type, and the coverage is
/// re-read — a sweep may have landed two minutes ago.
///
/// # Errors
///
/// [`AppError::InvalidInput`] when the folder stopped being readable,
/// [`AppError::Database`] on SQL failure.
pub async fn commit(
    state: &AppState,
    folder: &Path,
    forced_item: Option<Uuid>,
    picks: &[Pick],
) -> Result<Report, AppError> {
    // The one write path the pause never reached, while the banner
    // promised "nada é … vinculado". Not a worker with a loop to guard —
    // a button, reached straight from the route. `plan` above stays open
    // deliberately: it writes nothing, and a paused brarr the operator
    // cannot even look at is a worse tool.
    crate::db::settings::refuse_if_paused(state.pool(), "adotar arquivos").await?;
    let fresh = plan(state, folder, forced_item).await?;
    let mut report = Report::default();

    let submitted: HashSet<&str> = picks.iter().map(|p| p.path.as_str()).collect();
    report.appeared = fresh
        .files
        .iter()
        .filter(|f| f.token.is_some() && !submitted.contains(f.path.to_string_lossy().as_ref()))
        .count();

    for pick in picks {
        let row = fresh.files.iter().find(|f| {
            f.token.as_deref()
                == Some(Pick::encode(pick.item_id, pick.episode_id, pick.size, &pick.path).as_str())
        });
        let Some(row) = row else {
            report.outcomes.push(CommitOutcome {
                name: file_name_of(&pick.path),
                status: CommitStatus::Skipped,
                detail: "mudou desde a prévia — importe de novo".to_owned(),
            });
            continue;
        };
        report.outcomes.push(apply(state, row, pick).await?);
    }
    Ok(report)
}

/// Reserve, then place. Never the other way round: a hardlink written
/// before the reservation leaves an orphan in the library when the race
/// is lost.
async fn apply(
    state: &AppState,
    row: &PlannedFile,
    pick: &Pick,
) -> Result<CommitOutcome, AppError> {
    let name = row.name.clone();
    let reserved = grabs::reserve_local(
        state.pool(),
        &grabs::LocalGrab {
            item_id: pick.item_id,
            episode_id: pick.episode_id,
            source_path: &pick.path,
            release_name: &name,
        },
    )
    .await?;
    let Some(grab) = reserved else {
        return Ok(CommitOutcome {
            name,
            status: CommitStatus::Skipped,
            detail: "já adotado".to_owned(),
        });
    };

    // The in-place branch has no destination to hand `link_in`, so this
    // is where the type guarantee pays off: there is nothing to write.
    let Some(destination) = row.link_destination.clone() else {
        grabs::mark_imported(state.pool(), grab.id, &pick.path).await?;
        return Ok(CommitOutcome {
            name,
            status: CommitStatus::InPlace,
            detail: pick.path.clone(),
        });
    };

    let source = PathBuf::from(&pick.path);
    let target = destination.clone();
    let linked = tokio::task::spawn_blocking(move || link_in(&source, &target))
        .await
        .map_err(|e| AppError::InvalidInput(format!("vinculação falhou: {e}")))?;
    match linked {
        Ok(()) => {
            let shown = destination.to_string_lossy().to_string();
            grabs::mark_imported(state.pool(), grab.id, &shown).await?;
            Ok(CommitOutcome {
                name,
                status: CommitStatus::Linked,
                detail: shown,
            })
        }
        Err(why) => {
            // Releasing rather than failing: nothing was consumed, the
            // operator's file is exactly where it was, and the right
            // state is "not adopted yet, try again after fixing the
            // permission".
            grabs::release_reservation(state.pool(), grab.id).await?;
            Ok(CommitOutcome {
                name,
                status: CommitStatus::Skipped,
                detail: why,
            })
        }
    }
}

fn file_name_of(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(|| path.to_owned(), |n| n.to_string_lossy().to_string())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert on happy paths"
)]
mod tests {
    use super::*;

    fn ctx(root: Option<&Path>, library: &[&Path]) -> AdoptContext {
        AdoptContext {
            root: root.map(Path::to_path_buf),
            library_roots: library.iter().map(|p| real(p)).collect(),
            title: "The Boys".to_owned(),
            year: Some(2019),
        }
    }

    /// A real Sonarr-organised library diverges from `destination()` in
    /// the folder name, the season padding and the file name — and none
    /// of that matters, because the rule is containment.
    #[test]
    fn a_file_under_a_root_is_adopted_where_it_stands() {
        let root = Path::new("/data/series");
        let file = Path::new(
            "/data/series/The Boys (2019) [tvdbid-355567]/Season 4/\
             The Boys - S04E07 - Assassination Run [WEBDL-1080p]-NTb.mkv",
        );
        assert_eq!(
            action_for(file, Some((4, 7)), &ctx(Some(root), &[root])),
            AdoptAction::InPlace
        );
    }

    #[test]
    fn a_file_outside_every_root_is_linked_in() {
        let root = Path::new("/data/series");
        let file = Path::new("/downloads/The.Boys.S04E07.1080p.WEB-DL-NTb/x.mkv");
        let action = action_for(file, Some((4, 7)), &ctx(Some(root), &[root]));
        let AdoptAction::Link { destination } = &action else {
            unreachable!("expected a link, got {action:?}")
        };
        assert!(destination.ends_with("The Boys - S04E07.mkv"));
        assert!(destination.starts_with(root));
    }

    /// 19 TB is exactly the collection that lives on two disks. Judging
    /// against one root would hardlink the second disk into the first.
    #[test]
    fn every_root_serving_the_type_counts_as_the_library() {
        let first = Path::new("/data/series");
        let second = Path::new("/mnt/hdd2/series");
        let file = Path::new("/mnt/hdd2/series/The Boys (2019)/Season 04/x.mkv");
        assert_eq!(
            action_for(file, Some((4, 7)), &ctx(Some(first), &[first, second])),
            AdoptAction::InPlace
        );
    }

    /// `Path::starts_with` compares components, not bytes. A `str`
    /// version of this test passes and the bug ships.
    #[test]
    fn a_sibling_with_a_shared_prefix_is_not_the_library() {
        let root = Path::new("/data/series");
        let file = Path::new("/data/series-antigas/The Boys/x.mkv");
        assert!(matches!(
            action_for(file, Some((4, 7)), &ctx(Some(root), &[root])),
            AdoptAction::Link { .. }
        ));
    }

    #[test]
    fn no_root_for_the_type_blocks_instead_of_guessing() {
        let file = Path::new("/downloads/x.mkv");
        assert!(matches!(
            action_for(file, None, &ctx(None, &[])),
            AdoptAction::Blocked(_)
        ));
    }

    /// The whole point of the in-place branch: it has no destination to
    /// hand [`link_in`], so it cannot write.
    #[test]
    fn adopting_in_place_carries_nothing_to_write_with() {
        let root = Path::new("/data/filmes");
        let file = Path::new("/data/filmes/Duna Parte Dois 2024 1080p WEB-DL.mkv");
        // Equality against the unit variant is the assertion: giving
        // `InPlace` a destination field later stops this compiling,
        // which is the point — the branch must have nothing to write.
        assert_eq!(
            action_for(file, None, &ctx(Some(root), &[root])),
            AdoptAction::InPlace
        );
    }

    #[test]
    fn markers_are_read_or_refused_with_a_reason() {
        assert_eq!(parse_marker("The.Boys.S04E07.1080p.mkv"), Ok((4, 7)));
        assert_eq!(
            parse_marker("The Big Bang Theory - S011E09.mkv"),
            Ok((11, 9))
        );
        assert_eq!(parse_marker("Show 1x02.mkv"), Ok((1, 2)));
        assert_eq!(
            parse_marker("[Anitsu] Bofuri 2 - 11 [BD 1080p x265].mkv"),
            Err(MarkerError::Absent)
        );
        assert_eq!(
            parse_marker("Show.S01E01E02.1080p.mkv"),
            Err(MarkerError::Ambiguous)
        );
        assert_eq!(
            parse_marker("Show.S01E01.and.S01E05.mkv"),
            Err(MarkerError::Ambiguous)
        );
        // The same marker twice is one decision, not an ambiguity.
        assert_eq!(parse_marker("S04E07/The.Boys.S04E07.mkv"), Ok((4, 7)));
    }

    #[test]
    fn a_relative_or_unreadable_folder_is_a_form_error() {
        assert!(validate_folder(Path::new("relativo/aqui")).is_err());
        assert!(validate_folder(Path::new("/nao/existe/em/lugar/nenhum")).is_err());
    }

    /// The guard against the 7 GB incident coming back through the
    /// repair button: `link_in` links or fails, and never copies.
    #[test]
    fn link_in_refuses_instead_of_falling_back_to_a_copy() {
        let dir = std::env::temp_dir().join(format!("brarr-link-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("origem.mkv");
        std::fs::write(&source, b"conteudo").unwrap();

        let destination = dir
            .join("biblioteca")
            .join("Show")
            .join("Show - S01E01.mkv");
        link_in(&source, &destination).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"conteudo");

        // Same bytes, two names: writing through one is visible in the
        // other. A copy would not do that.
        std::fs::write(&source, b"mudou").unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"mudou");

        // And an occupied destination is refused rather than replaced.
        let second = dir.join("outra.mkv");
        std::fs::write(&second, b"outra").unwrap();
        let err = link_in(&second, &destination).expect_err("must not overwrite");
        assert!(err.contains("já existe"), "{err}");
        assert_eq!(std::fs::read(&destination).unwrap(), b"mudou");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_refusal_carries_a_reason_the_operator_can_act_on() {
        assert!(MarkerError::Absent.reason().contains("temporada/episódio"));
        assert!(MarkerError::Ambiguous.reason().contains("mais de um"));
    }

    /// Every file under `dir`, relative and sorted — a snapshot to prove
    /// "nothing was written", not just "the file is still there".
    fn tree(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(next) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&next) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(rel) = path.strip_prefix(dir) {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        out.sort();
        out
    }

    async fn state_with_root(root: &Path, media: MediaType) -> AppState {
        let pool = crate::db::open_memory().await.unwrap();
        crate::db::root_folders::insert(&pool, &root.to_string_lossy(), Some(media))
            .await
            .unwrap();
        AppState::new(pool, brarr_decision_service::Engine::baseline())
    }

    /// The whole feature's central promise: a file already under a root
    /// folder is recorded where it stands, and the disk is untouched.
    #[tokio::test]
    async fn adopting_in_place_writes_nothing_to_disk() {
        let base = std::env::temp_dir().join(format!("brarr-inplace-{}", uuid::Uuid::new_v4()));
        let root = base.join("midias");
        // A layout Sonarr would produce: no year, unpadded season, the
        // release name kept. None of it matches what brarr generates.
        let folder = root.join("The Boys").join("Season 4");
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join("The.Boys.S04E07.1080p.WEB-DL-NTb.mkv");
        std::fs::write(&file, b"video").unwrap();

        let state = state_with_root(&root, MediaType::Tv).await;
        let item = library::upsert(
            state.pool(),
            &library::NewLibraryItem {
                media_type: Some(MediaType::Tv),
                tmdb_id: 76479,
                title: "The Boys".to_owned(),
                year: Some(2019),
                ..library::NewLibraryItem::default()
            },
        )
        .await
        .unwrap();
        library::sync_seasons(
            state.pool(),
            item.id,
            &[library::NewSeason {
                season_number: 4,
                episode_count: 7,
                air_date: None,
                episodes: (1..=7)
                    .map(|n| library::NewEpisode {
                        tmdb_episode_id: None,
                        episode_number: n,
                        title: None,
                        air_date: None,
                    })
                    .collect(),
            }],
        )
        .await
        .unwrap();

        let before = tree(&base);
        let plan = plan(&state, &root, None).await.unwrap();
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].title.as_deref(), Some("The Boys"));
        assert_eq!(plan.files[0].episode, Some(7));
        assert_eq!(plan.files[0].effect.as_deref(), Some("manter no lugar"));
        assert!(
            plan.files[0].link_destination.is_none(),
            "in place carries nothing to write with"
        );
        assert_eq!(plan.ready(), 1);
        assert_eq!(
            tree(&base),
            before,
            "the preview must not touch the filesystem"
        );

        let token = plan.files[0].token.clone().unwrap();
        let pick = Pick::decode(&token).unwrap();
        let report = commit(&state, &root, None, std::slice::from_ref(&pick))
            .await
            .unwrap();

        assert_eq!(report.count(CommitStatus::InPlace), 1);
        assert_eq!(report.count(CommitStatus::Linked), 0);
        assert_eq!(
            tree(&base),
            before,
            "adopting in place must not create, move or copy anything"
        );

        let stored = grabs::for_item(state.pool(), item.id).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert!(grabs::is_in_place(&stored[0]));
        assert_eq!(
            stored[0].imported_path.as_deref(),
            Some(file.to_string_lossy().as_ref())
        );

        // And a second confirmation is refused by the barrier.
        let again = commit(&state, &root, None, std::slice::from_ref(&pick))
            .await
            .unwrap();
        assert_eq!(again.count(CommitStatus::Skipped), 1);

        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A file outside every root is hardlinked in — and the link is a
    /// link, not a copy.
    #[tokio::test]
    async fn adopting_from_a_downloads_folder_links_it_in() {
        let base = std::env::temp_dir().join(format!("brarr-link2-{}", uuid::Uuid::new_v4()));
        let root = base.join("midias");
        let downloads = base.join("torrents");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&downloads).unwrap();
        let file = downloads.join("Matrix.1999.1080p.BluRay.mkv");
        std::fs::write(&file, b"video").unwrap();

        let state = state_with_root(&root, MediaType::Movie).await;
        let item = library::upsert(
            state.pool(),
            &library::NewLibraryItem {
                media_type: Some(MediaType::Movie),
                tmdb_id: 603,
                title: "Matrix".to_owned(),
                year: Some(1999),
                ..library::NewLibraryItem::default()
            },
        )
        .await
        .unwrap();

        let plan = plan(&state, &downloads, None).await.unwrap();
        assert_eq!(plan.files.len(), 1);
        let row = &plan.files[0];
        assert!(
            row.effect
                .as_deref()
                .is_some_and(|e| e.starts_with("vincular")),
            "{:?}",
            row.effect
        );
        let pick = Pick::decode(row.token.as_ref().unwrap()).unwrap();
        let report = commit(&state, &downloads, None, &[pick]).await.unwrap();
        assert_eq!(report.count(CommitStatus::Linked), 1);

        let landed = root.join("Matrix (1999)").join("Matrix (1999).mkv");
        assert!(landed.is_file(), "the link is where destination() says");
        // Two names for the same bytes: writing through one shows in the
        // other. A copy would not.
        std::fs::write(&file, b"outro").unwrap();
        assert_eq!(std::fs::read(&landed).unwrap(), b"outro");

        let stored = grabs::for_item(state.pool(), item.id).await.unwrap();
        assert!(!grabs::is_in_place(&stored[0]));

        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Undoing an in-place adoption is a row deletion and nothing else.
    /// That is the strongest practical argument for the
    /// adopt-where-it-stands rule: there is no file to unmake.
    #[tokio::test]
    async fn undoing_an_in_place_adoption_leaves_the_disk_alone() {
        let base = std::env::temp_dir().join(format!("brarr-undo-{}", uuid::Uuid::new_v4()));
        let root = base.join("midias");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("Matrix 1999 1080p.mkv");
        std::fs::write(&file, b"video").unwrap();

        let state = state_with_root(&root, MediaType::Movie).await;
        let item = library::upsert(
            state.pool(),
            &library::NewLibraryItem {
                media_type: Some(MediaType::Movie),
                tmdb_id: 603,
                title: "Matrix".to_owned(),
                year: Some(1999),
                ..library::NewLibraryItem::default()
            },
        )
        .await
        .unwrap();

        let preview = plan(&state, &root, None).await.unwrap();
        let pick = Pick::decode(preview.files[0].token.as_ref().unwrap()).unwrap();
        commit(&state, &root, None, &[pick]).await.unwrap();

        let before = tree(&base);
        let grab = grabs::for_item(state.pool(), item.id).await.unwrap()[0].id;
        let message = undo(&state, grab).await.unwrap();

        assert!(message.contains("continua no disco"), "{message}");
        assert_eq!(tree(&base), before, "undo touched the filesystem");
        assert!(
            grabs::for_item(state.pool(), item.id)
                .await
                .unwrap()
                .is_empty(),
            "the row is gone, which frees the key to adopt again"
        );
        // And the same file can be adopted again afterwards.
        let again = plan(&state, &root, None).await.unwrap();
        assert_eq!(again.ready(), 1);

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn the_scan_reports_what_it_set_aside_instead_of_dropping_it() {
        let dir = std::env::temp_dir().join(format!("brarr-adopt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "Show.S01E01.mkv",
            "Show.S01E02.mkv",
            "Show-sample.mkv",
            "leia-me.txt",
        ] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let ignored: HashSet<String> =
            std::iter::once(dir.join("Show.S01E02.mkv").to_string_lossy().to_string()).collect();

        let scan = scan_folder(&dir, &ignored);
        assert_eq!(scan.files.len(), 1, "sample and non-video are not offered");
        assert_eq!(scan.files[0].marker, Ok((1, 1)));
        assert_eq!(scan.files[0].path, dir.join("Show.S01E01.mkv"));
        assert_eq!(scan.files[0].size, 1);
        assert_eq!(scan.ignored, 1, "an ignored file is counted, not hidden");
        assert_eq!(scan.over_cap, 0);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// **Adoption is the one write path the pause never reached.**
    ///
    /// The banner promises "Nada é buscado, baixado, importado ou
    /// **vinculado**", and this is the function that vincula: it reserves
    /// a grab and writes a hardlink. Every background worker asks
    /// `is_paused` before its loop, and this one is not a worker — it is a
    /// button, reached straight from the route, so it never asked.
    ///
    /// The cost is not hypothetical during the numbering refactor: a
    /// hardlink and a grab written under the coordinates brarr is about to
    /// change is precisely the state the pause exists to freeze.
    ///
    /// `plan` is deliberately still allowed — it writes nothing, and a
    /// paused brarr the operator cannot even look at is a worse tool.
    #[tokio::test]
    async fn adopting_while_paused_is_refused() {
        let base = std::env::temp_dir().join(format!("brarr-paused-{}", uuid::Uuid::new_v4()));
        let root = base.join("midias");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("Matrix.1999.1080p.BluRay.mkv");
        std::fs::write(&file, b"video").unwrap();

        let state = state_with_root(&root, MediaType::Movie).await;
        let item = library::upsert(
            state.pool(),
            &library::NewLibraryItem {
                media_type: Some(MediaType::Movie),
                tmdb_id: 603,
                title: "Matrix".to_owned(),
                year: Some(1999),
                ..library::NewLibraryItem::default()
            },
        )
        .await
        .unwrap();

        let preview = plan(&state, &root, None).await.unwrap();
        let pick = Pick::decode(preview.files[0].token.as_ref().unwrap()).unwrap();

        crate::db::settings::set(state.pool(), crate::db::settings::KEY_PAUSED, "1")
            .await
            .unwrap();

        // The preview still works: it writes nothing.
        assert_eq!(plan(&state, &root, None).await.unwrap().ready(), 1);

        let refused = commit(&state, &root, None, &[pick]).await;
        assert!(
            matches!(refused, Err(AppError::Paused { .. })),
            "commit adopted while paused: {refused:?}"
        );
        assert!(
            grabs::for_item(state.pool(), item.id)
                .await
                .unwrap()
                .is_empty(),
            "a paused brarr must not reserve a grab"
        );

        std::fs::remove_dir_all(&base).unwrap();
    }
}
