//! Axum-based admin web UI.
//!
//! Server-side rendered with Askama templates and HTMX for partial
//! updates. CSS is hand-authored and committed at `static/app.css` —
//! there is no frontend pipeline and no build step.

use brarr_core::MetadataSource;

pub mod ip;
pub mod render;
pub mod routes;
pub mod templates;
pub mod torznab;
pub mod webhooks;

pub use routes::router;
pub use routes::serve;

/// Cache-busting stamp appended to every `/static` URL in `base.html`.
///
/// The crate version, because that is exactly what changes when the
/// assets change: a release rewrites `app.css` and the `<link>` becomes
/// a URL the browser has never seen. Together with the `no-cache`
/// header on the static tree it makes a stale stylesheet impossible —
/// the failure it replaces was silent, since fresh markup against old
/// CSS just renders every new class as a no-op.
pub const ASSET_VERSION: &str = env!("CARGO_PKG_VERSION");

/// One metadata source's required attribution.
///
/// **A licence condition, not a courtesy.** It went unrendered for as
/// long as `brarr-tmdb` has existed — the constant was there, `pub`, and
/// no template read it — which is the failure mode this shape is built
/// against: nothing breaks when a footer disappears in a refactor.
///
/// Derived from the enum rather than written out per provider, so a
/// source added without a licence line fails a test instead of shipping
/// unattributed.
#[derive(Debug, Clone, Copy)]
pub struct Attribution {
    /// The sentence, verbatim. The shorter FAQ wording is not the one
    /// TMDB's terms require.
    pub text: &'static str,
    /// The link the sentence must carry. TheTVDB's free tier is
    /// explicitly conditioned on a **direct** link to TheTVDB.com; the
    /// allowance for an about or readme page covers command line products
    /// and libraries, and brarr has a UI.
    pub url: &'static str,
    /// What the link reads as.
    pub label: &'static str,
}

/// Every attribution the footer has to render.
///
/// Walks [`MetadataSource::all`], so this list cannot fall short of the
/// enum the way a hand-written one can.
#[must_use]
pub fn attributions() -> Vec<Attribution> {
    MetadataSource::all().map(attribution_for).collect()
}

/// The licence line one source requires.
fn attribution_for(source: MetadataSource) -> Attribution {
    match source {
        MetadataSource::Tmdb => Attribution {
            text: brarr_tmdb::ATTRIBUTION,
            url: "https://www.themoviedb.org",
            label: "TMDB",
        },
        MetadataSource::Tvdb => Attribution {
            text: brarr_tvdb::ATTRIBUTION,
            url: brarr_tvdb::ATTRIBUTION_URL,
            label: "TheTVDB.com",
        },
        // brarr stores IMDb ids and never calls IMDb, so there is no
        // licence to satisfy. Spelled out rather than filtered, so a
        // provider added here has to say which case it is.
        MetadataSource::Imdb => Attribution {
            text: "",
            url: "",
            label: "",
        },
    }
}

impl Attribution {
    /// Whether there is anything to render.
    #[must_use]
    pub const fn is_required(&self) -> bool {
        !self.text.is_empty()
    }
}
