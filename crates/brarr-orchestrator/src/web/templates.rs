//! Askama template structs. Each struct corresponds to one `.html`
//! file in `templates/`. Field names match `{{ field }}` references in
//! the templates.

use askama::Template;

/// Dashboard view at `/`.
#[derive(Debug, Template)]
#[template(path = "dashboard.html")]
pub struct DashboardTemplate {
    /// Aggregated stat for the header cards.
    pub provider_count: usize,
    /// Aggregated stat for the header cards.
    pub search_count: usize,
    /// Total push attempts ever recorded — denominator of the
    /// push-success-rate stat card.
    pub push_total: u64,
    /// Push attempts that returned `status='ok'`. Stat card renders
    /// `100 * push_ok / push_total` as a percentage when total > 0.
    pub push_ok: u64,
    /// Most recent searches.
    pub recent_searches: Vec<RecentSearchView>,
    /// Most recent kept decisions (any search, non-rejected).
    pub recent_decisions: Vec<DecisionView>,
}

/// Compact search summary for the dashboard list AND the
/// `/searches` filtered history page. Both surfaces include the
/// shared `partials/search_row_list.html` partial — extending this
/// struct with a new field reflects automatically in both.
#[derive(Debug)]
pub struct RecentSearchView {
    /// Stringified UUID.
    pub id: String,
    /// TMDb id used in the request (formatted as `"-"` if absent).
    pub tmdb_id: String,
    /// IMDb id used in the request (formatted as `"-"` if absent).
    pub imdb_id: String,
    /// TVDB id used in the request (formatted as `"-"` if absent).
    pub tvdb_id: String,
    /// Season filter (formatted; empty when not set).
    pub season: String,
    /// Episode filter (formatted; empty when not set).
    pub episode: String,
    /// ISO-8601 timestamp.
    pub submitted_at: String,
    /// Number of kept results.
    pub result_count: u32,
}

/// Single decision row for templates.
#[derive(Debug)]
pub struct DecisionView {
    /// Stringified UUID.
    pub id: String,
    /// Snapshot provider name.
    pub provider_name: String,
    /// Release title.
    pub release_name: String,
    /// Score shown on the card — the max across the baseline engine
    /// score and every per-profile score persisted for this row. Lets
    /// a release that's modest under baseline but high under a custom
    /// anime / dub profile read correctly without operator action.
    pub score: u32,
    /// Baseline engine score — kept around so the search-detail page
    /// can show "baseline 60 → anime profile 330" instead of hiding
    /// the delta.
    pub baseline_score: u32,
    /// Name of the profile whose score drove the displayed [`Self::score`].
    /// `None` when the baseline already wins (no profile re-evaluation
    /// produced a higher number).
    pub winning_profile: Option<String>,
    /// `true` when the operator explicitly chose a profile via the
    /// `?profile=<uuid>` query param on the search detail URL. In that
    /// case the score is the profile's exact output (no max-with-baseline
    /// clamp) so the operator can read the literal A/B value.
    pub profile_locked: bool,
    /// Rejected flag.
    pub rejected: bool,
    /// Comma-joined tags.
    pub tags: String,
    /// Comma-joined names of rules that fired for this decision. Lets
    /// the search-detail / releases pages explain "this release got
    /// 145 because: PT ambíguo + 2160p + HDR" without forcing the
    /// operator to read the rule engine source.
    pub matched_rules: String,
    /// Same data as `matched_rules` but pre-split + classified into
    /// (label, kind) pairs the templates render as coloured chips.
    /// `kind` is one of `"pt"` | `"accent"` | `"warning"` | `"neutral"`
    /// — purely a UI hint, not a domain enum.
    pub rule_chips: Vec<(String, String)>,
    /// Explicit language chips derived from the persisted
    /// `audio_languages` snapshot — `("PT-BR áudio", "pt")`,
    /// `("Dublado", "accent")`, etc. Independent of `rule_chips`: rule
    /// chips show *why* the score is what it is; these show *what the
    /// release actually has* regardless of which rules ran. `kind`
    /// uses the same vocabulary as `rule_chips`.
    pub audio_chips: Vec<(String, String)>,
    /// Subtitle counterpart to [`Self::audio_chips`] —
    /// `("PT-BR legenda", "pt")`, `("Legendado", "accent")`, etc.
    pub subtitle_chips: Vec<(String, String)>,
    /// Resolution label.
    pub resolution: String,
    /// Kind label.
    pub kind: String,
    /// Seeders count.
    pub seeders: u32,
    /// Human-friendly size (e.g. `1.23 GiB`).
    pub size_human: String,
    /// Single uppercase letter for the header provider badge —
    /// `provider_name`'s first ASCII alphanumeric, uppercased, or `?`
    /// when the name is blank / starts with punctuation.
    pub provider_initial: String,
    /// Approximate age of the decision relative to now, in pt-BR
    /// (`"há 23 dias"`, `"há 2 horas"`, `"agora"`). Empty string when
    /// the decision timestamp is in the future (clock skew) or
    /// otherwise unprintable.
    pub age: String,
}

/// Providers index view at `/providers`.
#[derive(Debug, Template)]
#[template(path = "providers.html")]
pub struct ProvidersTemplate {
    /// All configured providers.
    pub providers: Vec<ProviderView>,
}

/// Single provider row.
#[derive(Debug)]
pub struct ProviderView {
    /// Stringified UUID.
    pub id: String,
    /// Provider name.
    pub name: String,
    /// Provider base URL.
    pub base_url: String,
    /// Provider family (`unit3d`, `newznab`, `torznab`, `plugin`).
    pub kind: String,
    /// `true` when this provider participates in the search fan-out.
    /// Soft-disabled rows show a muted state in the UI and are
    /// skipped by `search::run_search`.
    pub enabled: bool,
    /// Creation timestamp (ISO-8601).
    pub created_at: String,
}

/// Partial template used by HTMX after `POST /providers`.
#[derive(Debug, Template)]
#[template(path = "partials/providers_list.html")]
pub struct ProvidersListPartial {
    /// All configured providers.
    pub providers: Vec<ProviderView>,
}

/// `/arr-instances` view — admin CRUD for Sonarr/Radarr endpoints
/// brarr can push releases to.
#[derive(Debug, Template)]
#[template(path = "arr_instances.html")]
pub struct ArrInstancesTemplate {
    /// All configured *arr endpoints.
    pub instances: Vec<ArrInstanceView>,
    /// All quality profiles — populates the "Quality Profile" select
    /// in the add-instance form. Empty when no profiles exist; the
    /// template hides the select and falls back to the threshold
    /// input.
    pub profiles: Vec<ProfileView>,
}

/// Single row in the *arr admin table.
#[derive(Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "these are independent axes, not a state machine — an instance can be a sync source while disabled for the deprecated push path, which is exactly the configuration the operator's three instances are in"
)]
pub struct ArrInstanceView {
    /// Stringified UUID.
    pub id: String,
    /// Operator-chosen display name.
    pub name: String,
    /// `"sonarr"` / `"radarr"`.
    pub kind: String,
    /// Base URL of the *arr instance.
    pub base_url: String,
    /// Minimum decision score required to trigger an auto-push.
    /// Profile's threshold (when attached) wins over this value at
    /// push time; the list view still shows it as a fallback so the
    /// operator can see what would apply if the profile is detached.
    pub push_threshold: u32,
    /// Display name of the attached quality profile (resolved by the
    /// list handler so the template doesn't need a second query).
    /// `None` when no profile is attached — the row falls back to
    /// `push_threshold`.
    pub profile_name: Option<String>,
    /// Threshold inherited from the attached profile (only populated
    /// when `profile_name` is `Some`). Lets the row chip render the
    /// effective threshold without another query.
    pub profile_threshold: Option<u32>,
    /// `true` if this row is currently eligible for push.
    pub enabled: bool,
    /// `true` ⇒ scheduled poller skips this instance (webhook-driven).
    pub webhook_driven: bool,
    /// `true` ⇒ brarr reads this catalogue into its own library. A
    /// different axis from `enabled`: the operator's instances are
    /// disabled for the deprecated push path while being exactly the
    /// catalogues brarr syncs from.
    pub sync_source: bool,
    /// When the passive sweep last read it (ISO-8601), `None` until it
    /// runs once.
    pub synced_at: Option<String>,
    /// Ready-to-paste inbound webhook URL for this instance's Connect →
    /// Webhook setting. Empty for views that don't render the *arr table
    /// (e.g. dashboard dropdowns). Includes `?apikey=` when auth is on.
    pub webhook_url: String,
    /// `false` ⇒ auth is disabled, so the URL carries no apikey — the
    /// template shows a dev-mode note instead.
    pub webhook_has_token: bool,
    /// Creation timestamp (ISO-8601).
    pub created_at: String,
}

/// HTMX partial returned after `POST /arr-instances` so the list cell
/// can refresh without a full page reload.
#[derive(Debug, Template)]
#[template(path = "partials/arr_instances_list.html")]
pub struct ArrInstancesListPartial {
    /// All configured *arr endpoints.
    pub instances: Vec<ArrInstanceView>,
}

/// `/arr-instances/{id}/import` — the preview of what reading one \*arr
/// catalogue would bring into brarr's library.
///
/// Fields are duplicated onto [`ArrImportBodyPartial`] rather than
/// nested, because Askama renders an `{% include %}` against the
/// *parent* context — the same arrangement as
/// [`DownloadClientsTemplate`]. `From` is implemented so the two are
/// built in one place and cannot drift.
#[derive(Debug, Template)]
#[template(path = "arr_import.html")]
pub struct ArrImportTemplate {
    /// Instance being imported.
    pub instance_id: String,
    /// Its display name, for the page header.
    pub instance_name: String,
    /// `Sonarr` / `Radarr`, used in the prose.
    pub kind: String,
    /// Roots the *arr reports, with what brarr makes of each.
    pub roots: Vec<ArrImportRootView>,
    /// brarr's own root folders, for the mapping select.
    pub root_folders: Vec<ArrRootOption>,
    /// One row per catalogued title.
    pub titles: Vec<ArrImportTitleView>,
    /// Quality profiles, for the run form.
    pub profiles: Vec<ProfileView>,
    /// Roots no mapping covers — the one thing to fix first.
    pub unmapped_roots: usize,
    /// Titles the run would add.
    pub new_titles: usize,
    /// Titles already catalogued.
    pub known_titles: usize,
    /// Titles nothing can be done with.
    pub blocked_titles: usize,
    /// Titles whose folder brarr can actually open. Zero here is what a
    /// wrong root mapping looks like *before* it costs anything.
    pub seen_folders: usize,
}

/// HTMX partial re-rendered after every write on the import screen.
///
/// The whole body is one target because its three parts move together:
/// adding a mapping is exactly what turns "0 pastas encontradas" into a
/// real number, and a count outside the swap would keep reading zero.
#[derive(Debug, Template)]
#[template(path = "partials/arr_import_body.html")]
pub struct ArrImportBodyPartial {
    /// Instance being imported.
    pub instance_id: String,
    /// `Sonarr` / `Radarr`.
    pub kind: String,
    /// Roots the *arr reports.
    pub roots: Vec<ArrImportRootView>,
    /// brarr's own root folders.
    pub root_folders: Vec<ArrRootOption>,
    /// One row per catalogued title.
    pub titles: Vec<ArrImportTitleView>,
    /// Quality profiles.
    pub profiles: Vec<ProfileView>,
    /// Roots no mapping covers.
    pub unmapped_roots: usize,
    /// Titles the run would add.
    pub new_titles: usize,
    /// Titles already catalogued.
    pub known_titles: usize,
    /// Titles nothing can be done with.
    pub blocked_titles: usize,
    /// Title folders brarr can open.
    pub seen_folders: usize,
}

impl ArrImportTemplate {
    /// Wrap a rendered body in the full page.
    #[must_use]
    pub fn from_body(body: ArrImportBodyPartial, instance_name: String) -> Self {
        Self {
            instance_id: body.instance_id,
            instance_name,
            kind: body.kind,
            roots: body.roots,
            root_folders: body.root_folders,
            titles: body.titles,
            profiles: body.profiles,
            unmapped_roots: body.unmapped_roots,
            new_titles: body.new_titles,
            known_titles: body.known_titles,
            blocked_titles: body.blocked_titles,
            seen_folders: body.seen_folders,
        }
    }
}

/// One *arr root folder in the mapping table.
#[derive(Debug)]
pub struct ArrImportRootView {
    /// Path as the *arr reports it.
    pub arr_path: String,
    /// Where the mapping sends it, when one covers it.
    pub mapped_to: Option<String>,
    /// The rule that fired, so the row can offer to remove it.
    pub mapping_id: Option<String>,
    /// `false` ⇒ brarr cannot open the translated directory.
    pub reachable: bool,
    /// Titles the *arr keeps under it.
    pub titles: usize,
}

/// One brarr root folder in the mapping select.
#[derive(Debug)]
pub struct ArrRootOption {
    /// Stringified UUID, which is the posted value.
    pub id: String,
    /// Absolute path, rendered mono.
    pub path: String,
}

/// One title in the import preview.
#[derive(Debug)]
pub struct ArrImportTitleView {
    /// Title as the *arr shows it.
    pub title: String,
    /// Year, when the *arr has one.
    pub year: Option<i32>,
    /// TMDB id; `0` renders as a dash and blocks the row.
    pub tmdb_id: i64,
    /// Whether the *arr is chasing it.
    pub monitored: bool,
    /// Whether brarr can open the title's folder after translation.
    pub folder_seen: bool,
    /// `novo` / `já na biblioteca` / the reason it is blocked.
    pub status: String,
    /// Drives the muted row style.
    pub blocked: bool,
}

/// What one import run did, or that it outlived the request.
#[derive(Debug, Default, Template)]
#[template(path = "partials/arr_import_report.html")]
pub struct ArrImportReportPartial {
    /// `true` ⇒ the run is still going and the numbers are not in yet.
    pub running: bool,
    /// Titles added.
    pub added: usize,
    /// Titles already present, whose metadata was refreshed.
    pub refreshed: usize,
    /// Titles nothing could be done with.
    pub blocked: usize,
    /// Files recorded.
    pub adopted: usize,
    /// Files a grab already covered.
    pub already: usize,
    /// Files whose grab had lost its episode and was repaired from the
    /// \*arr's pairing.
    pub relinked: usize,
    /// Files translated but not on disk.
    pub missing: usize,
    /// Files no mapping covered.
    pub unmapped: usize,
    /// Per-title failures kept verbatim.
    pub failures: Vec<String>,
    /// Total failures, cap included.
    pub failed: usize,
}

/// `/download-clients` view — admin CRUD for the qBittorrent / SABnzbd
/// instances brarr hands releases to.
#[derive(Debug, Template)]
#[template(path = "download_clients.html")]
pub struct DownloadClientsTemplate {
    /// Every configured client, enabled first.
    pub clients: Vec<DownloadClientView>,
    /// Configured destinations — the bottom half of the same screen.
    pub root_folders: Vec<RootFolderView>,
    /// Path-rewrite rules. Askama renders an `{% include %}` against the
    /// *parent* context, so the partial's fields have to live here too —
    /// same arrangement as [`DownloadClientsListPartial`].
    pub mappings: Vec<PathMappingView>,
    /// Clients available in the "add mapping" select.
    pub mapping_clients: Vec<PathMappingClientOption>,
    /// Failed imports the operator can send back to the queue.
    pub stuck: Vec<StuckImportView>,
    /// `false` ⇒ no enabled qBittorrent, so a torrent grab has nowhere
    /// to go. Surfaced as a warning strip rather than discovered when a
    /// release is finally found.
    pub has_torrent: bool,
    /// `false` ⇒ no enabled SABnzbd; same story for usenet.
    pub has_usenet: bool,
}

/// HTMX partial returned after every write on `/download-clients`, so
/// the table refreshes without a full page load.
///
/// Carries the same summary flags as the full page: they live *inside*
/// the swapped region on purpose. A count rendered in the page header
/// would still read "0 configurados" after the first client was added,
/// because HTMX only replaces the list.
#[derive(Debug, Template)]
#[template(path = "partials/download_clients_list.html")]
pub struct DownloadClientsListPartial {
    /// Every configured client, enabled first.
    pub clients: Vec<DownloadClientView>,
    /// See [`DownloadClientsTemplate::has_torrent`].
    pub has_torrent: bool,
    /// See [`DownloadClientsTemplate::has_usenet`].
    pub has_usenet: bool,
}

/// One row in the download-clients table.
#[derive(Debug)]
pub struct DownloadClientView {
    /// Stringified UUID.
    pub id: String,
    /// Operator-chosen display name.
    pub name: String,
    /// Persisted kind label (`"qbittorrent"` / `"sabnzbd"`) — the
    /// template branches on this.
    pub kind: String,
    /// Kind spelled the way the vendor spells it (`"qBittorrent"`).
    pub kind_label: String,
    /// `"torrent"` / `"usenet"`, derived from the kind.
    pub protocol: String,
    /// Base URL of the client's web interface.
    pub base_url: String,
    /// Category / label downloads are filed under, when set.
    pub category: Option<String>,
    /// Selection tie-break among clients of the same protocol.
    pub priority: u32,
    /// `false` ⇒ drained: keeps its config but receives no grabs.
    pub enabled: bool,
    /// Creation timestamp (ISO-8601).
    pub created_at: String,
}

/// HTMX partial for the root-folder table.
#[derive(Debug, Template)]
#[template(path = "partials/root_folders_list.html")]
pub struct RootFoldersListPartial {
    /// Configured destinations.
    pub root_folders: Vec<RootFolderView>,
}

/// The add-with-options dialog, returned by
/// `GET /library/add/options` and re-rendered on a validation failure so
/// the operator's picks survive.
#[derive(Debug, Template)]
#[template(path = "partials/library_add_options_modal.html")]
pub struct LibraryAddOptionsModalPartial {
    /// TMDB id being added.
    pub tmdb_id: i64,
    /// Localised title, for the header.
    pub title: String,
    /// Release / first-air year.
    pub year: Option<i32>,
    /// Drives which controls render: a movie has no season tree.
    pub is_series: bool,
    /// `true` when the title is already catalogued, so the dialog can
    /// say that submitting will overwrite the current configuration.
    pub already_in_library: bool,
    /// Root folders that serve this media type.
    pub root_folders: Vec<AddOptionFolder>,
    /// Configured quality profiles.
    pub profiles: Vec<AddOptionProfile>,
    /// `true` when "Sem perfil" is the current choice.
    pub no_profile_selected: bool,
    /// Threshold applied when no profile is chosen, shown so the
    /// fallback is not itself an invisible default.
    pub default_threshold: u32,
    /// Currently selected [`crate::db::library::MonitorScope`] label.
    pub scope: String,
    /// Validation message from a rejected submission.
    pub error: Option<String>,
}

/// One root-folder option in the add dialog.
#[derive(Debug)]
pub struct AddOptionFolder {
    /// Absolute path, which is also the posted value.
    pub path: String,
    /// `Filmes` / `Séries` / empty when it serves both.
    pub label: String,
    /// Pre-selected entry.
    pub selected: bool,
}

/// One quality-profile option in the add dialog.
#[derive(Debug)]
pub struct AddOptionProfile {
    /// Stringified UUID.
    pub id: String,
    /// Display name.
    pub name: String,
    /// `push_threshold`, shown so the operator can see what the choice
    /// actually changes.
    pub threshold: u32,
    /// Pre-selected entry.
    pub selected: bool,
}

/// HTMX partial for the path-mapping block: the rules, the clients that
/// can take one, and the failed imports an operator may want to retry
/// after adding one.
///
/// All three live in one swap target because they move together — adding
/// the rule that fixes an import is exactly when the retry button
/// becomes useful.
#[derive(Debug, Template)]
#[template(path = "partials/path_mappings.html")]
pub struct PathMappingsPartial {
    /// Configured rules, grouped by client in render order.
    pub mappings: Vec<PathMappingView>,
    /// Clients available in the "add" select. Named `mapping_clients`
    /// rather than `clients` because Askama renders an `{% include %}`
    /// against the parent's context, and `download_clients.html` already
    /// binds `clients` to a different type — the collision is a build
    /// error at best and the wrong list at worst.
    pub mapping_clients: Vec<PathMappingClientOption>,
    /// Failed imports whose download a client may still hold.
    pub stuck: Vec<StuckImportView>,
}

/// One row in the path-mapping table.
#[derive(Debug)]
pub struct PathMappingView {
    /// Stringified UUID.
    pub id: String,
    /// Which client reports paths in this namespace.
    pub client_name: String,
    /// Canonical remote prefix, as the client writes it.
    pub remote_prefix: String,
    /// Local prefix, in brarr's namespace.
    pub local_prefix: String,
    /// `false` when brarr cannot read the local side right now — an
    /// unmounted bind, say. The row renders muted rather than silently
    /// pointing somewhere useless.
    pub reachable: bool,
    /// How many components the remote side pins. Shown because the
    /// longest match wins, and two overlapping rules otherwise look
    /// interchangeable.
    pub specificity: usize,
}

/// One option in the client select of the "add mapping" form.
#[derive(Debug)]
pub struct PathMappingClientOption {
    /// Stringified UUID.
    pub id: String,
    /// Display name.
    pub name: String,
}

/// A failed import the operator can send back to the queue.
#[derive(Debug)]
pub struct StuckImportView {
    /// Stringified grab UUID.
    pub id: String,
    /// Release title snapshot.
    pub release_name: String,
    /// Catalogue title, when the item is still there.
    pub item_title: String,
    /// Why it failed, verbatim — the operator decides whether a mapping
    /// would have fixed it. brarr deliberately does not guess.
    pub error: String,
    /// Which client still holds the download.
    pub client_name: String,
}

/// One row in the root-folder table.
#[derive(Debug)]
pub struct RootFolderView {
    /// Stringified UUID.
    pub id: String,
    /// Absolute path, rendered mono.
    pub path: String,
    /// `Filmes` / `Séries` / `Filmes e séries`.
    pub content: String,
    /// `2.10 TiB livres de 8.00 TiB`, or a dash when the filesystem
    /// could not be read (an unmounted volume, say).
    pub free: String,
    /// `0..=100` for the usage bar.
    pub used_percent: u8,
    /// `false` when the path could not be read at all — the row renders
    /// muted instead of pretending the disk is empty.
    pub reachable: bool,
}

/// Edit-client modal, returned by `GET /download-clients/{id}/edit`.
///
/// Credentials are never echoed back — the fields render empty and a
/// blank submission keeps whatever is stored. [`Self::has_password`] /
/// [`Self::has_api_key`] only say *whether* one exists, so the operator
/// can tell "no password set" from "password hidden".
#[derive(Debug, Template)]
#[template(path = "partials/edit_download_client_modal.html")]
pub struct EditDownloadClientModalPartial {
    /// Stringified UUID.
    pub id: String,
    /// Current display name.
    pub name: String,
    /// Persisted kind label — drives which credential fields show.
    pub kind: String,
    /// Kind spelled the vendor's way, for the modal subtitle.
    pub kind_label: String,
    /// Current base URL.
    pub base_url: String,
    /// Current username (not a secret, so it is echoed).
    pub username: String,
    /// Current category.
    pub category: String,
    /// Current selection tie-break.
    pub priority: u32,
    /// Whether a password is on file.
    pub has_password: bool,
    /// Whether an apikey is on file.
    pub has_api_key: bool,
}

/// `/queue` view — what the download clients are doing right now.
///
/// Progress figures are read live when this renders, never stored: see
/// [`crate::queue`].
#[derive(Debug, Template)]
#[template(path = "queue.html")]
pub struct QueueTemplate {
    /// One row per in-flight grab, oldest first.
    pub entries: Vec<QueueEntryView>,
    /// Headline: `"3 baixando · 1 concluído"`, or an empty string when
    /// nothing is in flight.
    pub summary: String,
    /// Combined download rate across every row that reported one.
    pub total_speed: String,
    /// Seconds until the page asks again. Same field as
    /// [`QueueLiveTemplate`] because the page renders the fragment
    /// through `{% include %}`.
    pub poll_secs: u64,
}

/// The self-refreshing half of `/queue`, also served on its own at
/// `/queue/live`.
///
/// The fragment carries its own `hx-trigger`, so the server picks the
/// next interval on every cycle — see the template's own note for why
/// this is an adaptive cadence rather than htmx's `286` stop signal.
#[derive(Debug, Template)]
#[template(path = "partials/queue_live.html")]
pub struct QueueLiveTemplate {
    /// One row per in-flight grab, oldest first.
    pub entries: Vec<QueueEntryView>,
    /// Headline: `"3 baixando · 1 concluído"`, or an empty string when
    /// nothing is in flight.
    pub summary: String,
    /// Combined download rate across every row that reported one.
    pub total_speed: String,
    /// Seconds until the page asks again.
    pub poll_secs: u64,
}

/// One row of the queue.
#[derive(Debug)]
pub struct QueueEntryView {
    /// Library title the grab belongs to.
    pub title: String,
    /// Link target for the title.
    pub item_id: String,
    /// Release name, rendered mono.
    pub release_name: String,
    /// Provider the release came from.
    pub provider_name: String,
    /// `torrent` / `usenet`.
    pub protocol: String,
    /// Download client holding it, or `—`.
    pub client_name: String,
    /// Humanised size, or empty when the client didn't say.
    pub size: String,
    /// `0..=100`, for the bar's width.
    pub percent: u8,
    /// Rate, e.g. `8.2 MB/s`. Empty when unknown — SABnzbd never
    /// reports one per job.
    pub speed: String,
    /// `9 min restantes`, or empty.
    pub eta: String,
    /// Label for the status pill.
    pub status: String,
    /// Pill tone: `ok` / `warn` / `err` / `neutral`.
    pub tone: String,
    /// Why, when there is a why — a client-side failure, or the reason
    /// its client could not be asked.
    pub detail: Option<String>,
}

/// `/webhooks` view — recent inbound *arr webhook events (audit log).
#[derive(Debug, Template)]
#[template(path = "webhooks.html")]
pub struct WebhooksTemplate {
    /// Recent events, newest first.
    pub events: Vec<WebhookEventView>,
}

/// One row in the webhook audit table.
#[derive(Debug)]
pub struct WebhookEventView {
    /// Reception timestamp (ISO-8601; reformatted client-side by
    /// `datetime.js`).
    pub received_at: String,
    /// Display name of the *arr instance that fired it (`"(removida)"`
    /// when the instance was since deleted).
    pub arr_instance_name: String,
    /// `"sonarr"` / `"radarr"`.
    pub kind: String,
    /// Raw *arr `eventType` (e.g. `MovieAdded`, `Test`).
    pub event_type: String,
    /// UUID of the search this event triggered, if any (links to the
    /// search-detail page).
    pub triggered_search_id: Option<String>,
    /// Truncated payload JSON for the expandable detail.
    pub payload_preview: String,
}

/// `/health` view — provider fan-out health and Torznab/Newznab
/// endpoint latency aggregates over a rolling window.
#[derive(Debug, Template)]
#[template(path = "health.html")]
pub struct HealthTemplate {
    /// Aggregation window in hours (currently fixed at 24).
    pub window_hours: u32,
    /// Per-provider aggregates, sorted by name.
    pub providers: Vec<ProviderHealthView>,
    /// Per-endpoint-function aggregates, sorted by (endpoint, function).
    pub endpoints: Vec<EndpointHealthView>,
    /// Most recent endpoint requests, newest first.
    pub recent: Vec<EndpointRequestView>,
}

/// One row in the provider-health table.
#[derive(Debug)]
pub struct ProviderHealthView {
    /// Provider display name.
    pub name: String,
    /// `unit3d` / `newznab` / `torznab` / `plugin`.
    pub kind: String,
    /// Total dispatches in the window.
    pub total: u64,
    /// Successful dispatches.
    pub ok: u64,
    /// Errored dispatches.
    pub errors: u64,
    /// Budget-overrun dispatches.
    pub timeouts: u64,
    /// Mean dispatch duration (ms).
    pub avg_ms: u64,
    /// Median dispatch duration (ms).
    pub p50_ms: u64,
    /// 95th-percentile dispatch duration (ms) — the inconsistency signal.
    pub p95_ms: u64,
    /// Worst dispatch duration (ms).
    pub max_ms: u64,
    /// Total releases returned in the window.
    pub releases: u64,
    /// Most recent error text, when any dispatch failed.
    pub last_error: Option<String>,
    /// Timestamp of the latest sample (ISO-8601; reformatted
    /// client-side by `datetime.js`).
    pub last_seen: String,
    /// `true` when the window has no errors/timeouts — drives the badge.
    pub healthy: bool,
}

/// One row in the endpoint-latency table.
#[derive(Debug)]
pub struct EndpointHealthView {
    /// `torznab` / `newznab`.
    pub endpoint: String,
    /// `caps` / `movie` / `tvsearch` / `search` / `download`.
    pub function: String,
    /// Total requests in the window.
    pub total: u64,
    /// Requests answered 4xx/5xx (3xx redirects are successes).
    pub errors: u64,
    /// Search requests absorbed by the TTL cache.
    pub cache_hits: u64,
    /// Search requests that ran a full fan-out.
    pub cache_misses: u64,
    /// `100 * hits / (hits + misses)`; 0 when no search ran.
    pub hit_rate_pct: u32,
    /// Mean latency (ms).
    pub avg_ms: u64,
    /// Median latency (ms).
    pub p50_ms: u64,
    /// 95th-percentile latency (ms).
    pub p95_ms: u64,
    /// Worst latency (ms).
    pub max_ms: u64,
}

/// One row in the recent-requests table.
#[derive(Debug)]
pub struct EndpointRequestView {
    /// Request timestamp (ISO-8601; reformatted client-side).
    pub recorded_at: String,
    /// `torznab` / `newznab`.
    pub endpoint: String,
    /// `t=` function or `download`.
    pub function: String,
    /// HTTP status returned.
    pub status: u16,
    /// `true` for 2xx/3xx (redirects are the download proxy's success
    /// path) — drives the status badge color.
    pub ok: bool,
    /// Handler latency (ms).
    pub duration_ms: u64,
    /// `"hit"` / `"miss"` / `"—"` (non-search).
    pub cache: String,
}

/// `/pushes` view — recent push attempts grouped by release + *arr.
#[derive(Debug, Template)]
#[template(path = "pushes.html")]
pub struct PushesTemplate {
    /// One entry per (release, *arr) pair, newest cluster first.
    /// Repeat attempts on the same content render as a single
    /// collapsible group instead of N sibling rows in the table.
    pub groups: Vec<PushGroupView>,
    /// Filter state currently applied. Used to pre-fill the form so
    /// reloads / shares of the URL preserve the operator's view.
    pub filters: PushesFilterView,
    /// Dropdown options for the arr-instance filter (`(id, name)`).
    pub arr_options: Vec<(String, String)>,
    /// Total matches across all rows (denominator in the footer chip).
    pub total_count: u64,
}

/// Per-field current filter state for [`PushesTemplate`].
#[derive(Debug, Default)]
pub struct PushesFilterView {
    /// Selected arr_instance id (empty = any).
    pub arr_instance_id: String,
    /// Selected status (`"any"` / `"ok"` / `"http_error"` / `"transport_error"`).
    pub status: String,
    /// ISO date `YYYY-MM-DD` for the lower bound (or empty).
    pub from_date: String,
    /// ISO date `YYYY-MM-DD` for the upper bound (or empty).
    pub to_date: String,
    /// Free-text fragment matched against `release_name` via LIKE.
    pub release_query: String,
}

/// Cluster of push attempts targeting the same `(release, *arr)`.
#[derive(Debug)]
pub struct PushGroupView {
    /// Release title (from `decisions.release_name`).
    pub release_name: String,
    /// Provider that supplied this release.
    pub provider_name: String,
    /// *arr instance the cluster pushed to.
    pub arr_name: String,
    /// `"sonarr"` / `"radarr"`.
    pub arr_kind: String,
    /// Total attempts in the cluster.
    pub attempt_count: usize,
    /// ISO-8601 timestamp of the freshest attempt — used as the
    /// visible header line.
    pub latest_at: String,
    /// Same as `latest_at` as Unix seconds — used internally for
    /// sorting clusters newest-first.
    pub latest_at_unix: i64,
    /// `true` when at least one attempt in the cluster succeeded
    /// (HTTP 200, no `rejections`). Drives the badge colour.
    pub any_ok: bool,
    /// Individual attempts, newest first.
    pub attempts: Vec<PushHistoryView>,
}

/// Single row in the push history page.
#[derive(Debug)]
pub struct PushHistoryView {
    /// Stringified push UUID.
    pub id: String,
    /// Stringified decision UUID (links back to `/searches/{search_id}`
    /// via the decision row's lineage).
    pub decision_id: String,
    /// *arr display name snapshot at push time.
    pub arr_instance_name: String,
    /// `"sonarr"` / `"radarr"`.
    pub arr_kind: String,
    /// ISO-8601 timestamp.
    pub pushed_at: String,
    /// `"ok"` / `"http_error"` / `"transport_error"`.
    pub status: String,
    /// HTTP status if applicable.
    pub http_status: Option<u16>,
    /// *arr-side response body verbatim (8 KiB cap). Mostly for
    /// debugging when the parsed rejections list is empty but the
    /// grab still failed.
    pub response_body: String,
    /// Parsed `rejections` field from the response body. Empty Vec =
    /// *arr accepted cleanly (grab fired); non-empty = HTTP 200 but no
    /// grab (operator must fix *arr profile / custom formats / etc.).
    pub rejections: Vec<String>,
}

/// Releases (decisions) history view at `/releases`.
#[derive(Debug, Template)]
#[template(path = "releases.html")]
pub struct ReleasesTemplate {
    /// Most recent decision rows.
    pub decisions: Vec<DecisionView>,
    /// Every enabled *arr instance, rendered as a per-row "push" button
    /// so the operator can manually fire one decision at one *arr.
    pub arr_instances: Vec<ArrInstanceView>,
}

/// Login form view at `/login`.
#[derive(Debug, Template)]
#[template(path = "login.html")]
pub struct LoginTemplate {
    /// Optional error banner (wrong token, etc.).
    pub error_message: Option<String>,
}

/// Centered error page (404 + future 500). The fallback handler in
/// the router constructs this with the HTTP code that triggered the
/// fallback so the user sees a branded screen instead of axum's
/// default `Nothing matched` body.
#[derive(Debug, Template)]
#[template(path = "error.html")]
pub struct ErrorTemplate {
    /// HTTP status code (e.g. `"404"`, `"500"`).
    pub code: String,
    /// Headline (e.g. `"Página não encontrada"`).
    pub title: String,
    /// Human-friendly explanation. Supports `\n` for hard wraps.
    pub message: String,
}

/// Nova Busca dialog partial returned by `GET /searches/new`. Swapped
/// into the `#modal-target` slot in `base.html`; `modal.js` auto-opens
/// the <dialog> on `htmx:afterSwap`.
#[derive(Debug, Template)]
#[template(path = "partials/new_search_modal.html")]
pub struct NewSearchModalPartial {
    /// Number of provider rows currently enabled — copy in the
    /// footer reads "Buscará em N providers ativos".
    pub provider_count: usize,
    /// Persisted Quality Profiles — populates the "Avaliar com"
    /// dropdown so the operator can A/B a profile's scoring against
    /// the same search result set. Empty hides the dropdown entirely.
    pub profiles: Vec<ProfileView>,
}

/// Quality Profiles index at `/profiles`.
#[derive(Debug, Template)]
#[template(path = "profiles.html")]
pub struct ProfilesTemplate {
    /// Every profile row, presets first.
    pub profiles: Vec<ProfileView>,
}

/// Single quality-profile row for the index card grid.
#[derive(Debug)]
pub struct ProfileView {
    /// Stringified UUID.
    pub id: String,
    /// Operator-facing name.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Threshold integer (0..=1000).
    pub push_threshold: u32,
    /// `true` for the rows seeded by the migration.
    pub is_preset: bool,
}

/// New-profile dialog partial returned by `GET /profiles/new`.
#[derive(Debug, Template)]
#[template(path = "partials/new_profile_modal.html")]
pub struct NewProfileModalPartial;

/// Quality-profile editor view at `/profiles/{id}/edit`.
#[derive(Debug, Template)]
#[template(path = "profile_editor.html")]
pub struct ProfileEditorTemplate {
    /// Stringified UUID.
    pub id: String,
    /// Operator-facing name. Editable.
    pub name: String,
    /// Optional description. Editable.
    pub description: String,
    /// Threshold integer 0..=1000.
    pub push_threshold: u32,
    /// `true` for preset rows — surfaced as a banner so the operator
    /// knows tweaking a preset is supported but not the intended path.
    pub is_preset: bool,
    /// Rule list serialised to pretty JSON. The textarea binding round-
    /// trips through this field — operator-side typos surface as PUT
    /// validation errors.
    pub rules_json: String,
    /// Optional error banner shown after a failed PUT (validation /
    /// JSON parse / DB error).
    pub error_message: Option<String>,
    /// HTML-rendered breakdown returned by the preview endpoint. Empty
    /// on first render; populated by the HTMX preview swap target.
    pub preview_html: String,
}

/// Filtered + paginated history at `/searches`.
#[derive(Debug, Template)]
#[template(path = "searches_index.html")]
pub struct SearchesIndexTemplate {
    /// Search rows for the current page. Rendered by the shared
    /// `partials/search_row_list.html` partial (same one the dashboard
    /// uses).
    pub recent_searches: Vec<RecentSearchView>,
    /// Filter values currently applied. Used to pre-fill the form so
    /// the page is bookmarkable / reloadable.
    pub filters: SearchesFilterView,
    /// 1-indexed current page.
    pub page: u32,
    /// Total page count (>= 1 even when `recent_searches` is empty so
    /// the footer doesn't divide by zero).
    pub total_pages: u32,
    /// Whether to render the "previous page" link.
    pub has_prev: bool,
    /// Whether to render the "next page" link.
    pub has_next: bool,
    /// Rendered URL for the previous page. Empty when `has_prev` is
    /// false. The handler builds these so the template doesn't have
    /// to know the filter query string format.
    pub prev_href: String,
    /// Rendered URL for the next page.
    pub next_href: String,
    /// Total matches across all pages (denominator in the footer).
    pub total_count: u64,
}

/// Per-field current filter state for [`SearchesIndexTemplate`].
/// Every field is a string so the template can stuff it into `<input
/// value="...">` without further formatting. Empty strings mean "no
/// filter".
#[derive(Debug, Default)]
pub struct SearchesFilterView {
    /// TMDb id (numeric string or empty).
    pub tmdb_id: String,
    /// IMDb id (with or without `tt` prefix, or empty).
    pub imdb_id: String,
    /// TVDB id (numeric or empty).
    pub tvdb_id: String,
    /// Season number (or empty).
    pub season: String,
    /// Episode number (or empty).
    pub episode: String,
    /// ISO date `YYYY-MM-DD` for the lower bound (or empty).
    pub from_date: String,
    /// ISO date `YYYY-MM-DD` for the upper bound (or empty).
    pub to_date: String,
    /// `"any"` (default) | `"yes"` | `"no"`.
    pub has_kept_decision: String,
    /// Selected page size as a string for the `<select>` binding.
    pub page_size: String,
}

/// Runtime settings view at `/settings`. Each form posts to a
/// dedicated handler so a typo in one field doesn't roll back the
/// other sections.
#[derive(Debug, Template)]
#[template(path = "settings.html")]
pub struct SettingsTemplate {
    /// Which group the side menu has selected. Every section stays in
    /// the DOM — hidden ones are only `display: none`, so the single
    /// general form still submits every field it owns.
    pub section: String,
    /// Pre-filled fields + status flags driving the form rendering.
    pub values: SettingsValues,
    /// Optional flash banner (success / error) shown above the form.
    /// `None` on plain GETs; populated by POST handlers after they
    /// finish.
    pub flash: Option<SettingsFlash>,
}

/// Pre-filled values for the settings form. Strings only so the
/// template can stuff them straight into `<input value="...">` without
/// further formatting.
#[derive(Debug, Default)]
pub struct SettingsValues {
    /// `true` when an admin token is currently configured. Drives the
    /// "auth enabled / disabled" badge and hides the token-rotation
    /// form's `current_token` requirement when there's nothing to
    /// rotate from.
    pub auth_enabled: bool,
    /// Trusted-peer allowlist spec (matches `BRARR_BYPASS_AUTH_FROM`).
    pub bypass_auth_from: String,
    /// Trusted-proxy spec (matches `BRARR_TRUSTED_PROXIES`).
    pub trusted_proxies: String,
    /// Public base URL override (matches `BRARR_PUBLIC_URL`).
    pub public_url: String,
    /// Poller cadence in seconds (matches `BRARR_ARR_POLL_INTERVAL_SECS`).
    pub poll_interval_secs: String,
    /// Cadence of the passive *arr sweep, in seconds. Blank shows the
    /// default the task is actually using.
    pub arr_sync_interval_secs: String,
    /// History-retention window in days (matches
    /// `BRARR_DECISIONS_RETENTION_DAYS`). `"0"` = keep forever.
    pub decisions_retention_days: String,
    /// Keep-all override for search persistence (matches
    /// `BRARR_PERSIST_REJECTED`). `true` ⇒ persist even releases that
    /// every quality profile rejects; `false` (default) ⇒ drop them.
    pub persist_rejected: bool,
    /// How the importer places files: `hardlink` / `copy` / `move`.
    /// See [`crate::import::ImportMode`] — the default keeps the
    /// download client's copy in place so a private tracker keeps
    /// seeding.
    pub import_mode: String,
    /// Tracing env-filter spec (matches `RUST_LOG`).
    pub log_level: String,
    /// Backtrace mode persisted in the DB (matches `RUST_BACKTRACE`).
    /// Note the form shows a "restart required" badge because Rust
    /// 2024 made `std::env::set_var` unsafe and the workspace forbids
    /// `unsafe_code`.
    pub backtrace: String,
    /// Torznab (torrent) indexer base URL (`…/torznab/api`) — goes in
    /// the *arr indexer's URL field (no query; *arr appends `t=` and the
    /// apikey field separately). Host empty when no public URL is set.
    pub torznab_base: String,
    /// Newznab (usenet) indexer base URL (`…/newznab/api`). Registered as
    /// a separate Newznab indexer so *arr hands `.nzb` results to the
    /// usenet download client, not qBittorrent.
    pub newznab_base: String,
    /// Admin token for the *arr indexer's "API Key" field. Empty when
    /// auth is disabled.
    pub indexer_apikey: String,
    /// `(id, name)` of every quality profile — drives the card's profile
    /// picker, which builds the `&profile=<id>` additional parameter.
    pub profiles: Vec<(String, String)>,
    /// `true` when a TMDB credential is stored or present in the
    /// environment. The field itself is never echoed back.
    pub tmdb_configured: bool,
    /// Metadata language (default `pt-BR`).
    pub tmdb_language: String,
    /// Country used for release-date resolution (default `BR`).
    pub tmdb_country: String,
    /// Metadata refresh window in days.
    pub tmdb_ttl_days: String,
}

/// One-shot flash message rendered above the settings form. `kind`
/// is `"ok"` or `"err"` so the template can pick a colour without
/// pattern-matching enum variants in Askama.
#[derive(Debug)]
pub struct SettingsFlash {
    /// `"ok"` (green) or `"err"` (red).
    pub kind: String,
    /// User-facing message body (already localised).
    pub message: String,
}

/// `GET /providers/{id}/edit` HTMX partial.
#[derive(Debug, Template)]
#[template(path = "partials/edit_provider_modal.html")]
pub struct EditProviderModalPartial {
    /// Stringified provider UUID — used as the form's PUT target.
    pub id: String,
    /// Current name.
    pub name: String,
    /// Current base URL.
    pub base_url: String,
    /// Current api token (echoed back; operator can paste a new one).
    pub api_token: String,
    /// Current kind (`unit3d` / `newznab` / `torznab` / `plugin`).
    pub kind: String,
    /// Optional plugin path (empty string when no plugin attached).
    pub plugin_path: String,
}

/// `GET /arr-instances/{id}/edit` HTMX partial.
#[derive(Debug, Template)]
#[template(path = "partials/edit_arr_instance_modal.html")]
pub struct EditArrInstanceModalPartial {
    /// Stringified UUID — used as the form's PUT target.
    pub id: String,
    /// Display name.
    pub name: String,
    /// `"sonarr"` / `"radarr"` — read-only label (kind isn't editable).
    pub kind: String,
    /// Base URL.
    pub base_url: String,
    /// Api key.
    pub api_key: String,
    /// Push threshold (formatted).
    pub push_threshold: String,
    /// All quality profiles for the `<select>`.
    pub profiles: Vec<ProfileView>,
    /// Currently-attached profile id (empty means "none").
    pub profile_id: String,
}

/// Single-search view at `/searches/{id}`.
#[derive(Debug, Template)]
#[template(path = "search_detail.html")]
pub struct SearchDetailTemplate {
    /// Stringified search id.
    pub id: String,
    /// TMDb id used (formatted).
    pub tmdb_id: String,
    /// Submission timestamp (ISO-8601).
    pub submitted_at: String,
    /// All decision rows for this search (kept + rejected).
    pub decisions: Vec<DecisionView>,
    /// *arr instances enabled for push, so the shared release card
    /// partial can render per-instance push buttons. Empty when no
    /// *arr is configured — the card hides the buttons in that case.
    pub arr_instances: Vec<ArrInstanceView>,
    /// Per-provider failure messages (transient — not persisted).
    pub failures: Vec<(String, String)>,
}

// ---- library ---------------------------------------------------------

/// Library index at `/library`.
#[derive(Debug, Template)]
#[template(path = "library.html")]
pub struct LibraryTemplate {
    /// Monitored movies.
    pub movies: u64,
    /// Monitored series.
    pub series: u64,
    /// Entries with monitoring switched off.
    pub unmonitored: u64,
    /// `false` when no TMDB credential is configured, which is the only
    /// state where an empty library is not the operator's own doing.
    pub tmdb_ready: bool,

    // The results half, repeated field for field because Askama's
    // `{% include %}` renders against the *parent* context — the same
    // shape `QueueTemplate` carries for `partials/queue_live.html`.
    /// Catalogue rows, already filtered and ranked.
    pub items: Vec<LibraryItemView>,
    /// How many survived the filter and the search.
    pub matched: usize,
    /// `"grid"` or `"list"`.
    pub view: String,
    /// Active filter chip.
    pub filter: String,
    /// What the operator typed.
    pub query: String,
    /// Active ordering.
    pub sort: String,
    /// `(id, name)` for the bulk profile picker.
    pub profiles: Vec<(String, String)>,
    /// `(path, label)` for the bulk root-folder picker.
    pub root_folders: Vec<(String, String)>,
    /// What the last bulk action did, when it did not do all of it.
    pub notice: String,
}

/// The results half of `/library`, served on its own at
/// `/library/items` so a keystroke swaps the list instead of the page.
///
/// The page renders it through `{% include %}`, so first paint and every
/// filtered refresh come out of the same file.
#[derive(Debug, Template)]
#[template(path = "partials/library_items.html")]
pub struct LibraryItemsPartial {
    /// Catalogue rows, already filtered and ranked.
    pub items: Vec<LibraryItemView>,
    /// How many survived — the count the operator reads before doing
    /// something to all of them.
    pub matched: usize,
    /// `"grid"` or `"list"` — drives which layout renders.
    pub view: String,
    /// Active filter chip: `""`, `"movie"`, `"tv"`, `"unmonitored"`,
    /// `"missing"` or `"complete"`.
    pub filter: String,
    /// What the operator typed, echoed so a swap does not clear the box.
    pub query: String,
    /// Active ordering, echoed so the picker keeps its choice.
    pub sort: String,
    /// `(id, name)` for the bulk profile picker.
    pub profiles: Vec<(String, String)>,
    /// `(path, label)` for the bulk root-folder picker.
    pub root_folders: Vec<(String, String)>,
    /// What the last bulk action did, when it did not do all of it.
    /// Empty renders nothing.
    pub notice: String,
    /// Needed by the empty state, which distinguishes "you have not
    /// added anything" from "there is no TMDB credential, so you
    /// *cannot* add anything yet".
    pub tmdb_ready: bool,
}

/// One catalogue row.
#[derive(Debug)]
pub struct LibraryItemView {
    /// Stringified UUID.
    pub id: String,
    /// Localised title.
    pub title: String,
    /// Release / first-air year, or `—`.
    pub year: String,
    /// `Filme` or `Série`.
    pub kind_label: String,
    /// `true` for series — drives the badge tone and the episode line.
    pub is_series: bool,
    /// Full CDN URL, or `None` for the placeholder tile.
    pub poster_url: Option<String>,
    /// Whether the scanner should chase it.
    pub monitored: bool,
    /// Quality profile name, or `—`.
    pub profile: String,
    /// TMDB id, rendered as a mono chip.
    pub tmdb_id: i64,
    /// Canonical `ttNNNNNNN`, when known.
    pub imdb_id: Option<String>,
    /// `3 temporadas · 24 episódios` for series; empty for movies.
    pub tree_summary: String,
    /// Localised `dd/mm/aaaa` of when it was added.
    pub added_at: String,
    /// [`crate::coverage::ItemStatus::tone`] — drives the spine, the
    /// chip and the bar, so one colour means one thing on the screen.
    pub tone: String,
    /// What the chip says.
    pub status_label: String,
    /// Monitored episodes (or `1` for a monitored movie). **This is the
    /// denominator**, and it is never the series total — see
    /// [`crate::coverage`].
    pub monitored_count: usize,
    /// Of those, how many are on disk.
    pub have: usize,
    /// Aired, monitored and absent — the number the operator can act on.
    pub missing: usize,
    /// Monitored and not aired yet. Not a gap.
    pub upcoming: usize,
    /// `have / monitored_count`, for the bar width.
    pub percent: u8,
}

/// TMDB search page at `/library/add`.
#[derive(Debug, Template)]
#[template(path = "library_add.html")]
pub struct LibraryAddTemplate {
    /// Echoed back into the search box.
    pub query: String,
    /// `"all"`, `"movie"` or `"tv"`.
    pub kind: String,
    /// Hits, movies before series.
    pub results: Vec<TmdbHitView>,
    /// `false` when no credential is configured — the page then explains
    /// how to set one instead of showing an empty result list.
    pub tmdb_ready: bool,
    /// Non-empty when the search itself failed.
    pub error: Option<String>,
    /// `true` once a query has run, so "nenhum resultado" only shows
    /// after an actual search.
    pub searched: bool,
}

/// One TMDB hit on the add screen.
#[derive(Debug)]
pub struct TmdbHitView {
    /// TMDB id.
    pub tmdb_id: i64,
    /// `movie` or `tv` — posted back on add.
    pub media_type: String,
    /// `Filme` or `Série`.
    pub kind_label: String,
    /// `true` for series.
    pub is_series: bool,
    /// Localised title.
    pub title: String,
    /// Year, or `—`.
    pub year: String,
    /// Synopsis, after the en-US backfill.
    pub overview: Option<String>,
    /// Poster CDN URL.
    pub poster_url: Option<String>,
    /// Already in the catalogue — renders a pill instead of a button.
    pub in_library: bool,
}

/// Detail page at `/library/{id}`.
#[derive(Debug, Template)]
#[template(path = "library_detail.html")]
pub struct LibraryDetailTemplate {
    /// The catalogue entry.
    pub item: LibraryDetailView,
    /// Its status line, kept apart so a toggle can re-send just this.
    pub status: ItemStatusView,
    /// Seasons, ascending. Empty for movies.
    pub seasons: Vec<SeasonView>,
    /// How many acquisitions the dialog would show. The list itself is
    /// loaded on demand — a series the *arr import brought in carries
    /// hundreds of rows, and rendering them inline pushed the season
    /// tree, which is what the page is for, below all of them.
    pub grab_count: usize,
    /// Every quality profile, for the picker.
    pub profiles: Vec<(String, String)>,
    /// Registered root folders this item could live in, as
    /// `(path, label)`. Only the ones that serve the item's media type
    /// — pinning a series to a movies-only folder is not a choice worth
    /// offering.
    pub root_folders: Vec<(String, String)>,
    /// Path the item is currently pinned to, or empty for "use the rule".
    pub root_folder: String,
}

/// Hero data for the detail page.
#[derive(Debug)]
pub struct LibraryDetailView {
    /// Stringified UUID.
    pub id: String,
    /// Localised title.
    pub title: String,
    /// Original-language title, shown only when it differs.
    pub original_title: Option<String>,
    /// Year, or `—`.
    pub year: String,
    /// `Filme` or `Série`.
    pub kind_label: String,
    /// `true` for series.
    pub is_series: bool,
    /// Poster CDN URL at a larger size than the index uses.
    pub poster_url: Option<String>,
    /// Synopsis.
    pub overview: Option<String>,
    /// TMDB id.
    pub tmdb_id: i64,
    /// Canonical `ttNNNNNNN`.
    pub imdb_id: Option<String>,
    /// TVDB id — series only.
    pub tvdb_id: Option<i64>,
    /// Whether the scanner chases it.
    pub monitored: bool,
    /// Attached profile id, for preselecting the picker.
    pub profile_id: String,
    /// TMDB status string.
    pub status: Option<String>,
    /// `136 min`, or empty.
    pub runtime: String,
    /// `dd/mm/aaaa` of the next unaired episode, or empty.
    pub next_air_date: String,
    /// `dd/mm/aaaa` of the digital release, or empty.
    pub digital_release: String,
    /// `dd/mm/aaaa` of the physical release, or empty.
    pub physical_release: String,
    /// `true` while the digital release date is still in the future —
    /// searching before it usually only turns up cams.
    pub in_theatrical_window: bool,
}

/// The item's status line, as its own type because a season or episode
/// toggle has to send it back out-of-band.
///
/// Both the hero and the toggle response render these same fields; the
/// markup is written twice because the two live in different templates,
/// and `library_ui_integration` asserts the hero actually moves after a
/// toggle, which is what catches them drifting apart.
#[derive(Debug, Clone)]
pub struct ItemStatusView {
    /// [`crate::coverage::ItemStatus::tone`].
    pub tone: String,
    /// What the chip says.
    pub status_label: String,
    /// Monitored episodes, or `1` for a monitored movie. Specials count
    /// when they are monitored — see [`crate::coverage`].
    pub monitored_count: usize,
    /// Of those, how many are on disk.
    pub have: usize,
    /// Aired, monitored and absent — and only when it is worth calling
    /// out, see [`crate::coverage::ItemStatus::callout`].
    pub missing: usize,
    /// `have / monitored_count`.
    pub percent: u8,
}

/// One collapsed season header.
#[derive(Debug)]
pub struct SeasonView {
    /// Stringified UUID.
    pub id: String,
    /// Season number.
    pub number: i32,
    /// `Temporada 4`, or `Especiais` for season 0.
    pub label: String,
    /// Episodes TMDB reports.
    pub episode_count: i32,
    /// Whether the scanner chases the season.
    pub monitored: bool,
    /// [`crate::coverage::ItemStatus::tone`], scoped to this season.
    pub tone: String,
    /// What the chip says.
    pub status_label: String,
    /// Monitored episodes of this season — the denominator.
    pub monitored_count: usize,
    /// Of those, how many are on disk.
    pub have: usize,
    /// `have / monitored_count`.
    pub percent: u8,
}

/// Episode rows returned by the on-demand season expand.
#[derive(Debug, Template)]
#[template(path = "partials/library_season.html")]
pub struct LibrarySeasonPartial {
    /// Parent item id, for the toggle URLs.
    pub item_id: String,
    /// This season's id when the response **is** the season body, and
    /// `None` when it is a single episode row.
    ///
    /// The distinction is load-bearing: the season body is wrapped in a
    /// re-requestable `<div>`, and a single row is swapped straight into
    /// `#ep-{id}`. Wrapping the latter would nest a second
    /// `season-rows-…` inside the list and duplicate its id.
    pub season_id: Option<String>,
    /// Seconds until the rows ask again, or `None` to stay put.
    ///
    /// Set only while an episode is actually downloading. A season with
    /// nothing in flight renders a wrapper with no trigger and never
    /// asks again — polling a static list would be pure noise.
    pub poll_secs: Option<u64>,
    /// Episodes of one season, ascending.
    pub episodes: Vec<EpisodeView>,
    /// Set only when a *season* toggle produced this response: its
    /// bookmark lives in the `<summary>`, outside the swap target, and
    /// rides along out-of-band. A full-page refresh would be the other
    /// option, and it closes the accordion the operator just opened.
    pub oob: Option<SeasonMarkView>,
    /// The item's status line, sent by **both** toggles.
    ///
    /// Monitoring a season or an episode changes the denominator, so the
    /// hero above is wrong the instant either runs. Leaving it stale is
    /// what made unmarking a season look like it did nothing.
    pub item_status: Option<ItemStatusView>,
}

/// The out-of-band half of a season toggle: its bookmark and its status.
///
/// Both live in the `<summary>`, outside the swap target, and both are
/// wrong the instant the cascade runs — a season whose every episode was
/// just paused is no longer "faltando", it is "nada monitorado". Sending
/// only the bookmark leaves a chip contradicting the row underneath it.
#[derive(Debug)]
pub struct SeasonMarkView {
    /// Season whose fragments are being replaced.
    pub season_id: String,
    /// New state, which is also the bookmark's `aria-pressed`.
    pub monitored: bool,
    /// Recomputed tone.
    pub tone: String,
    /// Recomputed chip text.
    pub status_label: String,
    /// Monitored episodes after the cascade — `0` when it just paused
    /// the season, which is what hides the bar.
    pub monitored_count: usize,
    /// Of those, how many are on disk.
    pub have: usize,
    /// `have / monitored_count`.
    pub percent: u8,
}

/// One episode row.
#[derive(Debug)]
pub struct EpisodeView {
    /// Stringified UUID.
    pub id: String,
    /// `S04E07`.
    pub code: String,
    /// Season this episode belongs to. Carried separately from
    /// [`Self::code`] because the per-episode search button builds a
    /// query string, and parsing `S04E07` back into two numbers to
    /// rebuild what we already had would be a round trip for nothing.
    pub season_number: i32,
    /// Episode number within the season.
    pub episode_number: i32,
    /// Episode title, or `—`.
    pub title: String,
    /// `dd/mm/aaaa`, or empty when unaired.
    pub air_date: String,
    /// Whether the scanner chases it — the bookmark's state.
    pub monitored: bool,
    /// [`crate::coverage::EpisodeState::tone`] — which icon to draw.
    /// A different question from `monitored`: an episode can be chased
    /// and absent, or paused and on disk.
    pub state_tone: String,
    /// Accessible name for the icon.
    pub state_label: String,
    /// Tooltip detail — the **mapped file path** for a downloaded
    /// episode. Seeing which file brarr tied to an episode used to mean
    /// reading the grab table row by row.
    pub detail: String,
    /// Just the file name from [`Self::detail`], for the inline hint.
    pub file_name: String,
    /// Download percentage, when this episode has a grab in flight and
    /// the queue sync has seen it at least once.
    ///
    /// `None` covers three different things on purpose — not
    /// downloading, downloading but never yet probed, and a sync that
    /// stopped running long enough for the value to expire. The row
    /// shows the busy icon either way; only the number goes missing,
    /// which is better than a number frozen at 43% for an hour.
    pub percent: Option<u8>,
}

/// "Where and how" for one title, in a dialog.
///
/// The two `<select>`s used to sit loose in the detail page's control
/// row. They moved behind a gear when that row became icons — a select
/// among 36px squares breaks the alignment, and placement is
/// configuration rather than a daily action.
#[derive(Debug, Template)]
#[template(path = "partials/library_placement_modal.html")]
pub struct LibraryPlacementModalPartial {
    /// Stringified item UUID, for the form action.
    pub item_id: String,
    /// Title, for the dialog subheading.
    pub item_title: String,
    /// `(id, name)` for every quality profile.
    pub profiles: Vec<(String, String)>,
    /// Currently attached profile id, or empty.
    pub profile_id: String,
    /// `(path, label)` for every root folder serving this media type.
    pub root_folders: Vec<(String, String)>,
    /// Currently chosen root folder, or empty for "the type's default".
    pub root_folder: String,
}

/// The acquisition history, in a dialog.
#[derive(Debug, Template)]
#[template(path = "partials/library_grabs_modal.html")]
pub struct LibraryGrabsModalPartial {
    /// Item title, for the dialog header.
    pub title: String,
    /// Acquisition history, newest first.
    pub grabs: Vec<GrabView>,
}

/// Results of a manual search, swapped into the item detail page.
#[derive(Debug, Template)]
#[template(path = "partials/interactive_results.html")]
pub struct InteractiveResultsPartial {
    /// Item the search was run for.
    pub item_id: String,
    /// What was searched, echoed back: `"temporada 4"`, `"S04E07"`, or
    /// empty for a movie.
    pub axis: String,
    /// Season the grab should record, as a string for the form.
    pub season: String,
    /// Episode number the grab should record; empty means a season pack.
    pub episode: String,
    /// Candidates, best score first.
    pub results: Vec<InteractiveReleaseView>,
    /// Shown instead of the table when there is nothing to show.
    pub message: String,
}

/// One candidate release in the interactive search.
#[derive(Debug)]
pub struct InteractiveReleaseView {
    /// Decision id — what the grab button posts.
    pub id: String,
    /// Release title.
    pub release_name: String,
    /// Provider it came from.
    pub provider_name: String,
    /// `torrent` / `usenet`.
    pub protocol: String,
    /// Humanised size.
    pub size: String,
    /// Seeder count; `—` for usenet.
    pub seeders: String,
    /// Score under the item's profile, or baseline when it has none.
    pub score: u32,
    /// `true` when that score clears the profile's push threshold — the
    /// automatic sweep would have taken it. The operator can grab either
    /// way; this only says which side of the line it fell on.
    pub passes: bool,
    /// `true` when every quality profile rejected it.
    pub rejected: bool,
    /// Language chips, `(label, kind)` — same shape the release card uses.
    pub languages: Vec<(String, String)>,
    /// `false` when the release exposed no download URL, so there is
    /// nothing to hand a client.
    pub grabbable: bool,
}

/// One acquisition row on the detail page.
#[derive(Debug)]
pub struct GrabView {
    /// Stringified UUID, for the undo button.
    pub id: String,
    /// `protocol == "local"`. Not derived in the template: `protocol`
    /// there is a display string, and comparing it to a literal in the
    /// markup hides a rule where nobody looks for one.
    pub is_local: bool,
    /// Adopted where it stood, so undo has nothing to remove from disk.
    /// Changes what the confirmation says.
    pub in_place: bool,
    /// Release title snapshot.
    pub release_name: String,
    /// Provider name snapshot.
    pub provider_name: String,
    /// `torrent` or `usenet`.
    pub protocol: String,
    /// Lifecycle label.
    pub status: String,
    /// UI tone for the status pill: `ok` / `warn` / `err` / `neutral`.
    pub tone: String,
    /// `dd/mm/aaaa`.
    pub grabbed_at: String,
    /// Failure reason, when there is one — or, for a grab whose imported
    /// file has since vanished, where it used to be.
    pub error: Option<String>,
    /// `true` when the verification pass found the imported file gone.
    /// The status still reads `imported`, because it was; this is what
    /// says the library no longer has it.
    pub file_missing: bool,
}

/// The import-from-disk dialog, returned by `GET /library/import`.
///
/// One flat list — movies and series together, the identified and the
/// undecided side by side. The four collapsible groups an earlier design
/// had are gone: what was a group is now a state on the row, which is
/// what makes bulk editing across the whole list possible.
#[derive(Debug, Template)]
#[template(path = "partials/import_modal.html")]
pub struct ImportModalPartial {
    /// Folder that was scanned, as typed.
    pub folder: String,
    /// Item the dialog is pinned to, when opened from a title's page.
    pub item_id: Option<String>,
    /// Title of that item, for the header.
    pub item_title: Option<String>,
    /// Rows, in scan order.
    pub rows: Vec<ImportRowView>,
    /// Rows that would be written on confirm.
    pub ready: usize,
    /// Rows still waiting on a decision.
    pub undecided: usize,
    /// Rows a live grab already covers.
    pub covered: usize,
    /// Videos found beyond the preview ceiling.
    pub over_cap: usize,
    /// Ceiling itself, so the message can name it.
    pub max_files: usize,
    /// How many videos in this folder the operator has set aside.
    pub ignored_here: usize,
    /// Every path currently ignored, for the `Ignorados` filter.
    pub ignored: Vec<ImportIgnoredView>,
    /// `true` when the operator asked to see the ignored list.
    pub showing_ignored: bool,
    /// `true` while navigating: the folder has not been read yet.
    ///
    /// The dialog opens here on purpose. Scanning on open meant every
    /// visit walked the whole tree — thousands of files under a root —
    /// to answer a question the operator had not asked yet.
    pub browsing: bool,
    /// Subfolders of the current path, to navigate into.
    pub entries: Vec<ImportDirEntry>,
    /// The folder above, when there is one.
    pub parent: Option<String>,
    /// Registered root folders, as one-click jumps.
    pub shortcuts: Vec<ImportDirEntry>,
    /// Always false here. The shared row partial reads it, and Askama
    /// resolves an included template against the parent context — so
    /// the field has to exist on both. Only a bulk action sets it.
    pub oob: bool,
    /// Validation message — a folder that is not readable, say.
    pub error: Option<String>,
}

/// One file in the import dialog.
///
/// Rendered both inside the dialog's loop and on its own, when a picker
/// swaps a single row back in. `partials/import_row.html` is shared by
/// the two, which is why the index travels on the row rather than coming
/// from `loop.index0`: the standalone render has no loop.
#[derive(Debug)]
pub struct ImportRowView {
    /// Position in the list, so a picker can aim at `#import-row-N`.
    pub idx: usize,
    /// Round-trip token; `None` means the row cannot be confirmed yet.
    pub token: Option<String>,
    /// Absolute path, submitted when ignoring.
    pub path: String,
    /// File name, which is what the row shows.
    pub name: String,
    /// Human-friendly size.
    pub size: String,
    /// Assigned title, when one was matched.
    pub title: Option<String>,
    /// Its catalogue id, which the episode picker needs. Empty when no
    /// title is assigned yet.
    pub item: String,
    /// Drives the type chip and whether season/episode mean anything.
    pub is_series: bool,
    /// Season the name claims.
    pub season: Option<u16>,
    /// `7 — The Insider`.
    pub episode_label: Option<String>,
    /// Why this row still needs a human.
    pub reason: Option<String>,
    /// What would happen on confirm.
    pub effect: Option<String>,
    /// A live grab already covers this target.
    pub covered: bool,
}

/// One folder the operator can navigate into.
#[derive(Debug)]
pub struct ImportDirEntry {
    /// Folder name, for the row.
    pub name: String,
    /// Absolute path, for the link.
    pub path: String,
}

/// One path on the ignored list.
#[derive(Debug)]
pub struct ImportIgnoredView {
    /// Absolute path.
    pub path: String,
    /// File name, for the row.
    pub name: String,
}

/// One import row on its own, swapped in after a picker assigns a
/// target. Same template the dialog's loop includes.
#[derive(Debug, Template)]
#[template(path = "partials/import_row.html")]
pub struct ImportRowPartial {
    /// The row itself.
    pub row: ImportRowView,
    /// Folder being imported, so the row's pickers can carry it.
    pub folder: String,
    /// Item the dialog is pinned to, when it is.
    pub item_id: Option<String>,
    /// Emit `hx-swap-oob`, so a bulk action can return several rows in
    /// one response and leave everything else in the dialog alone.
    pub oob: bool,
}

/// The title picker — a dialog on top of the import dialog.
#[derive(Debug, Template)]
#[template(path = "partials/import_pick_title.html")]
pub struct ImportPickTitlePartial {
    /// Apply the choice to every ticked row instead of to one file.
    pub bulk: bool,
    /// File the choice applies to.
    pub file_name: String,
    /// Its absolute path, posted back.
    pub path: String,
    /// Row to swap when the operator picks.
    pub idx: usize,
    /// Folder being imported.
    pub folder: String,
    /// Item the dialog is pinned to, when it is.
    pub item_id: Option<String>,
    /// Current filter text.
    pub query: String,
    /// Matching catalogue entries.
    pub titles: Vec<PickTitleView>,
    /// How many entries the library has in total, so an empty filter
    /// result reads as "nothing matched" and not as "empty library".
    pub total: usize,
}

/// One catalogue entry in the title picker.
#[derive(Debug)]
pub struct PickTitleView {
    /// Catalogue id.
    pub id: String,
    /// Localised title.
    pub title: String,
    /// Release year.
    pub year: Option<i32>,
    /// Drives the type chip.
    pub is_series: bool,
    /// `4 temporadas · 32 episódios` for a series, the root folder for a
    /// movie — what tells two similar entries apart.
    pub meta: String,
}

/// The episode picker — a dialog on top of the import dialog.
#[derive(Debug, Template)]
#[template(path = "partials/import_pick_episode.html")]
pub struct ImportPickEpisodePartial {
    /// Number every ticked row from the chosen episode onwards.
    pub bulk: bool,
    /// File the choice applies to.
    pub file_name: String,
    /// Its absolute path, posted back.
    pub path: String,
    /// Row to swap when the operator picks.
    pub idx: usize,
    /// Folder being imported.
    pub folder: String,
    /// Item the dialog is pinned to, when it is.
    pub item_id: Option<String>,
    /// Catalogue entry the episodes belong to.
    pub target_item: String,
    /// Its title, for the header.
    pub target_title: String,
    /// Seasons to offer as chips.
    pub seasons: Vec<i32>,
    /// Season currently shown.
    pub season: Option<i32>,
    /// Episodes of that season.
    pub slots: Vec<PickEpisodeView>,
    /// How many of them nothing covers yet.
    pub free: usize,
}

/// One episode slot in the picker.
#[derive(Debug)]
pub struct PickEpisodeView {
    /// Catalogue id.
    pub id: String,
    /// `S04E07`.
    pub code: String,
    /// Episode title, when TMDB has one.
    pub title: String,
    /// A live grab already covers it.
    pub taken: bool,
}

/// The report the confirmation renders, in the same dialog shell.
#[derive(Debug, Template)]
#[template(path = "partials/import_report.html")]
pub struct ImportReportPartial {
    /// Files recorded where they already were.
    pub in_place: usize,
    /// Files hardlinked into the library.
    pub linked: usize,
    /// Files nothing was written for.
    pub skipped: usize,
    /// Files that appeared in the folder between preview and confirm.
    pub appeared: usize,
    /// One line per submitted file.
    pub outcomes: Vec<ImportOutcomeView>,
}

/// One line of the import report.
#[derive(Debug)]
pub struct ImportOutcomeView {
    /// File name.
    pub name: String,
    /// `adotado no lugar` / `vinculado` / `pulado`.
    pub label: String,
    /// Where it landed, or why it did not.
    pub detail: String,
    /// Drives the row's colour.
    pub skipped: bool,
    /// A link was created, as opposed to a path being recorded.
    pub linked: bool,
}

/// HTML-escapes a fragment for safe interpolation. Askama auto-escapes
/// `{{ x }}` by default; this helper is for when we build a string in
/// Rust before passing it to a template.
#[must_use]
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}
