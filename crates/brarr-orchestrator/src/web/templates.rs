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

/// Compact search summary for the dashboard list.
#[derive(Debug)]
pub struct RecentSearchView {
    /// Stringified UUID.
    pub id: String,
    /// TMDb id used in the request (formatted as `"-"` if absent).
    pub tmdb_id: String,
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

/// `/pushes` view — recent push attempts grouped by release + *arr.
#[derive(Debug, Template)]
#[template(path = "pushes.html")]
pub struct PushesTemplate {
    /// One entry per (release, *arr) pair, newest cluster first.
    /// Repeat attempts on the same content render as a single
    /// collapsible group instead of N sibling rows in the table.
    pub groups: Vec<PushGroupView>,
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
