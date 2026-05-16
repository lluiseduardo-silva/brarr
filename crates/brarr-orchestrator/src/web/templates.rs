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
    /// Engine score.
    pub score: u32,
    /// Rejected flag.
    pub rejected: bool,
    /// Comma-joined tags.
    pub tags: String,
    /// Resolution label.
    pub resolution: String,
    /// Kind label.
    pub kind: String,
    /// Seeders count.
    pub seeders: u32,
    /// Human-friendly size (e.g. `1.23 GiB`).
    pub size_human: String,
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

/// Releases (decisions) history view at `/releases`.
#[derive(Debug, Template)]
#[template(path = "releases.html")]
pub struct ReleasesTemplate {
    /// Most recent decision rows.
    pub decisions: Vec<DecisionView>,
}

/// Login form view at `/login`.
#[derive(Debug, Template)]
#[template(path = "login.html")]
pub struct LoginTemplate {
    /// Optional error banner (wrong token, etc.).
    pub error_message: Option<String>,
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
