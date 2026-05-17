//! Decision rows. One row per release that survived the rules engine
//! (rejected releases are still persisted so the UI can show *why*
//! something was filtered out).

use brarr_core::{Language, ReleaseKind, Resolution};
use sqlx::{Row, sqlite::SqliteRow};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{AppError, db::Pool};

/// Persisted decision row.
#[derive(Debug, Clone)]
pub struct DecisionRow {
    /// Stable UUID v4.
    pub id: Uuid,
    /// FK → `searches.id`.
    pub search_id: Uuid,
    /// FK → `providers.id` (or `None` if the provider has since been deleted).
    pub provider_id: Option<Uuid>,
    /// Provider name snapshot (denormalized so the UI doesn't `JOIN` and
    /// historical rows survive provider deletions).
    pub provider_name: String,
    /// Release title.
    pub release_name: String,
    /// Provider-side numeric id (so the UI can deep-link).
    pub release_id_remote: u64,
    /// Score assigned by the rules engine.
    pub score: u32,
    /// Whether the engine rejected this release.
    pub rejected: bool,
    /// Tags attached by matched rules.
    pub tags: Vec<String>,
    /// Names of the rules that matched.
    pub matched_rules: Vec<String>,
    /// Seeder count snapshot.
    pub seeders: u32,
    /// Leecher count snapshot.
    pub leechers: u32,
    /// Total bytes.
    pub size_bytes: u64,
    /// Resolution label.
    pub resolution: String,
    /// Kind label.
    pub kind: String,
    /// Upstream `.torrent` / `.nzb` download URL (apikey may be
    /// embedded). Surfaced through the Torznab outbound feed via the
    /// `/torznab/download/{decision_id}` proxy so Sonarr / Radarr can
    /// actually grab the file. `None` when the provider didn't expose
    /// one.
    pub download_url: Option<String>,
    /// Details / comments page URL for the release on the provider.
    /// Surfaced as `<comments>` in the Torznab feed.
    pub details_url: Option<String>,
    /// Provider kind snapshot (`unit3d` / `newznab` / `torznab` /
    /// `plugin`). Used by the Torznab outbound feed to pick the
    /// correct `<enclosure type>` per item (`application/x-nzb` for
    /// Newznab, `application/x-bittorrent` elsewhere). Legacy rows
    /// pre-dating the 20260516140000 migration have `None`.
    pub provider_kind: Option<String>,
    /// Upstream upload timestamp captured at search time. Mapped to the
    /// Torznab feed's `<pubDate>` so Sonarr/Radarr can show the real
    /// age of each release instead of "Age: 0 minutes". Legacy rows
    /// pre-dating the 20260517120000 migration have `None`; the feed
    /// renderer falls back to `now()` in that case.
    pub published_at: Option<OffsetDateTime>,
    /// Audio track languages captured from the release's `MediaInfo`
    /// enrichment at search time. Empty when the provider didn't ship
    /// `MediaInfo` or the parser produced nothing. Powers explicit
    /// `PT-BR áudio` / `Dublado` chips on the release card without
    /// re-running the parser at render time.
    pub audio_languages: Vec<Language>,
    /// Subtitle track languages. Same shape and semantics as
    /// [`Self::audio_languages`].
    pub subtitle_languages: Vec<Language>,
    /// When the engine produced the outcome.
    pub decided_at: OffsetDateTime,
}

/// Input bundle used to insert a single decision row.
#[derive(Debug, Clone)]
pub struct DecisionInsert {
    /// FK → `searches.id`.
    pub search_id: Uuid,
    /// FK → `providers.id`.
    pub provider_id: Option<Uuid>,
    /// Provider name snapshot.
    pub provider_name: String,
    /// Release title.
    pub release_name: String,
    /// Provider-side numeric id.
    pub release_id_remote: u64,
    /// Engine score.
    pub score: u32,
    /// Rejected flag.
    pub rejected: bool,
    /// Tags collected from matched rules.
    pub tags: Vec<String>,
    /// Names of matched rules.
    pub matched_rules: Vec<String>,
    /// Seeders.
    pub seeders: u32,
    /// Leechers.
    pub leechers: u32,
    /// Size in bytes.
    pub size_bytes: u64,
    /// Resolution from `brarr_core`.
    pub resolution: Resolution,
    /// Release kind from `brarr_core`.
    pub kind: ReleaseKind,
    /// Upstream download URL (verbatim, including apikey if any).
    pub download_url: Option<String>,
    /// Upstream details / comments page URL.
    pub details_url: Option<String>,
    /// Provider kind snapshot — see [`DecisionRow::provider_kind`].
    pub provider_kind: Option<String>,
    /// Upstream upload timestamp — see [`DecisionRow::published_at`].
    pub published_at: Option<OffsetDateTime>,
    /// Audio languages — see [`DecisionRow::audio_languages`].
    pub audio_languages: Vec<Language>,
    /// Subtitle languages — see [`DecisionRow::subtitle_languages`].
    pub subtitle_languages: Vec<Language>,
}

/// Insert one decision row, returning the persisted form.
///
/// # Errors
///
/// Surfaces a [`sqlx::Error`] on SQL failure or a JSON error if `tags`/
/// `matched_rules` cannot be serialized.
pub async fn insert(pool: &Pool, ins: DecisionInsert) -> Result<DecisionRow, AppError> {
    let id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    let tags_json = serde_json::to_string(&ins.tags)?;
    let matched_json = serde_json::to_string(&ins.matched_rules)?;
    let audio_langs_json = serde_json::to_string(&ins.audio_languages)?;
    let subtitle_langs_json = serde_json::to_string(&ins.subtitle_languages)?;
    let resolution = resolution_label(&ins.resolution);
    let kind = kind_label(&ins.kind);

    sqlx::query(
        "INSERT INTO decisions ( \
            id, search_id, provider_id, provider_name, release_name, release_id_remote, \
            score, rejected, tags_json, matched_json, seeders, leechers, size_bytes, \
            resolution, kind, decided_at, download_url, details_url, provider_kind, \
            published_at, audio_langs_json, subtitle_langs_json \
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(ins.search_id.to_string())
    .bind(ins.provider_id.map(|t| t.to_string()))
    .bind(&ins.provider_name)
    .bind(&ins.release_name)
    .bind(i64_from_u64(ins.release_id_remote))
    .bind(i64::from(ins.score))
    .bind(i64::from(u8::from(ins.rejected)))
    .bind(&tags_json)
    .bind(&matched_json)
    .bind(i64::from(ins.seeders))
    .bind(i64::from(ins.leechers))
    .bind(i64_from_u64(ins.size_bytes))
    .bind(&resolution)
    .bind(&kind)
    .bind(now.unix_timestamp())
    .bind(&ins.download_url)
    .bind(&ins.details_url)
    .bind(&ins.provider_kind)
    .bind(ins.published_at.map(OffsetDateTime::unix_timestamp))
    .bind(&audio_langs_json)
    .bind(&subtitle_langs_json)
    .execute(pool)
    .await?;

    Ok(DecisionRow {
        id,
        search_id: ins.search_id,
        provider_id: ins.provider_id,
        provider_name: ins.provider_name,
        release_name: ins.release_name,
        release_id_remote: ins.release_id_remote,
        score: ins.score,
        rejected: ins.rejected,
        tags: ins.tags,
        matched_rules: ins.matched_rules,
        seeders: ins.seeders,
        leechers: ins.leechers,
        size_bytes: ins.size_bytes,
        resolution,
        kind,
        download_url: ins.download_url,
        details_url: ins.details_url,
        provider_kind: ins.provider_kind,
        published_at: ins.published_at,
        audio_languages: ins.audio_languages,
        subtitle_languages: ins.subtitle_languages,
        decided_at: now,
    })
}

/// Fetch all decisions for a given search, ordered by score DESC.
///
/// # Errors
///
/// Surfaces a [`sqlx::Error`] on SQL failure.
pub async fn list_for_search(pool: &Pool, search_id: Uuid) -> Result<Vec<DecisionRow>, AppError> {
    let rows = sqlx::query(
        "SELECT id, search_id, provider_id, provider_name, release_name, release_id_remote, \
                score, rejected, tags_json, matched_json, seeders, leechers, size_bytes, \
                resolution, kind, decided_at, download_url, details_url, provider_kind, \
                published_at, audio_langs_json, subtitle_langs_json \
         FROM decisions WHERE search_id = ? ORDER BY score DESC, seeders DESC",
    )
    .bind(search_id.to_string())
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_decision).collect()
}

/// Fetch a single decision row by id. Used by the Torznab download
/// proxy route to resolve `<enclosure url>` proxy hits back to the
/// persisted upstream URL.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] when no row matches; surfaces a
/// [`sqlx::Error`] otherwise.
pub async fn get_by_id(pool: &Pool, id: Uuid) -> Result<DecisionRow, AppError> {
    let row_opt = sqlx::query(
        "SELECT id, search_id, provider_id, provider_name, release_name, release_id_remote, \
                score, rejected, tags_json, matched_json, seeders, leechers, size_bytes, \
                resolution, kind, decided_at, download_url, details_url, provider_kind, \
                published_at, audio_langs_json, subtitle_langs_json \
         FROM decisions WHERE id = ?",
    )
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;
    match row_opt {
        Some(row) => row_to_decision(&row),
        None => Err(AppError::NotFound(format!("decision {id}"))),
    }
}

/// Return the most recent `limit` decision rows across all searches.
///
/// # Errors
///
/// Surfaces a [`sqlx::Error`] on SQL failure.
pub async fn recent(pool: &Pool, limit: u32) -> Result<Vec<DecisionRow>, AppError> {
    let limit = match limit {
        0 => 50,
        n if n > 500 => 500,
        n => n,
    };
    let rows = sqlx::query(
        "SELECT id, search_id, provider_id, provider_name, release_name, release_id_remote, \
                score, rejected, tags_json, matched_json, seeders, leechers, size_bytes, \
                resolution, kind, decided_at, download_url, details_url, provider_kind, \
                published_at, audio_langs_json, subtitle_langs_json \
         FROM decisions ORDER BY decided_at DESC LIMIT ?",
    )
    .bind(i64::from(limit))
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_decision).collect()
}

fn row_to_decision(row: &SqliteRow) -> Result<DecisionRow, AppError> {
    let id_str: String = row.try_get("id")?;
    let id = Uuid::parse_str(&id_str)
        .map_err(|e| AppError::InvalidInput(format!("invalid uuid in decisions.id: {e}")))?;
    let search_id_str: String = row.try_get("search_id")?;
    let search_id = Uuid::parse_str(&search_id_str)
        .map_err(|e| AppError::InvalidInput(format!("invalid uuid in search_id: {e}")))?;
    let provider_id_opt: Option<String> = row.try_get("provider_id")?;
    let provider_id = match provider_id_opt {
        Some(s) => Some(
            Uuid::parse_str(&s)
                .map_err(|e| AppError::InvalidInput(format!("invalid uuid in provider_id: {e}")))?,
        ),
        None => None,
    };
    let release_id_remote_i64: i64 = row.try_get("release_id_remote")?;
    let release_id_remote = u64_from_i64(release_id_remote_i64);
    let score_i64: i64 = row.try_get("score")?;
    let rejected_i64: i64 = row.try_get("rejected")?;
    let tags_json: String = row.try_get("tags_json")?;
    let matched_json: String = row.try_get("matched_json")?;
    let seeders_i64: i64 = row.try_get("seeders")?;
    let leechers_i64: i64 = row.try_get("leechers")?;
    let size_bytes_i64: i64 = row.try_get("size_bytes")?;
    let decided_unix: i64 = row.try_get("decided_at")?;
    let decided_at = OffsetDateTime::from_unix_timestamp(decided_unix)
        .map_err(|e| AppError::InvalidInput(format!("invalid timestamp: {e}")))?;

    let download_url: Option<String> = row.try_get("download_url").ok().flatten();
    let details_url: Option<String> = row.try_get("details_url").ok().flatten();
    let provider_kind: Option<String> = row.try_get("provider_kind").ok().flatten();
    // Legacy rows pre-dating the 20260519120000 migration get the column
    // default `'[]'`, so parsing always returns an empty vec rather than
    // failing. Malformed JSON (shouldn't happen — only this crate writes
    // the column) surfaces as an `AppError` via `?`.
    let audio_langs_json: String = row
        .try_get::<Option<String>, _>("audio_langs_json")
        .ok()
        .flatten()
        .unwrap_or_else(|| "[]".to_string());
    let subtitle_langs_json: String = row
        .try_get::<Option<String>, _>("subtitle_langs_json")
        .ok()
        .flatten()
        .unwrap_or_else(|| "[]".to_string());
    let audio_languages: Vec<Language> = serde_json::from_str(&audio_langs_json)?;
    let subtitle_languages: Vec<Language> = serde_json::from_str(&subtitle_langs_json)?;
    // Legacy rows pre-dating the 20260517120000 migration keep NULL —
    // `try_get` returns `Ok(None)` then. Bad UNIX timestamps (shouldn't
    // happen but defend against malformed test fixtures) silently drop
    // to None instead of failing the whole row.
    let published_at: Option<OffsetDateTime> = row
        .try_get::<Option<i64>, _>("published_at")
        .ok()
        .flatten()
        .and_then(|ts| OffsetDateTime::from_unix_timestamp(ts).ok());
    Ok(DecisionRow {
        id,
        search_id,
        provider_id,
        provider_name: row.try_get("provider_name")?,
        release_name: row.try_get("release_name")?,
        release_id_remote,
        score: u32::try_from(score_i64).unwrap_or(0),
        rejected: rejected_i64 != 0,
        tags: serde_json::from_str(&tags_json)?,
        matched_rules: serde_json::from_str(&matched_json)?,
        seeders: u32::try_from(seeders_i64).unwrap_or(0),
        leechers: u32::try_from(leechers_i64).unwrap_or(0),
        size_bytes: u64_from_i64(size_bytes_i64),
        resolution: row.try_get("resolution")?,
        kind: row.try_get("kind")?,
        download_url,
        details_url,
        provider_kind,
        published_at,
        audio_languages,
        subtitle_languages,
        decided_at,
    })
}

/// SQLite has no native u64; we store u64 reinterpreted as i64. Negative
/// values are impossible in practice for our domain (sizes, ids).
#[allow(
    clippy::cast_possible_wrap,
    reason = "release ids and byte counts comfortably fit in i64 positive range for this domain"
)]
const fn i64_from_u64(v: u64) -> i64 {
    v as i64
}

#[allow(
    clippy::cast_sign_loss,
    reason = "values were originally u64; sign is purely a storage artifact"
)]
const fn u64_from_i64(v: i64) -> u64 {
    v as u64
}

fn resolution_label(r: &Resolution) -> String {
    match r {
        Resolution::Sd => "SD".to_string(),
        Resolution::P720 => "720p".to_string(),
        Resolution::P1080 => "1080p".to_string(),
        Resolution::P2160 => "2160p".to_string(),
        Resolution::Other(s) => s.clone(),
    }
}

fn kind_label(k: &ReleaseKind) -> String {
    match k {
        ReleaseKind::WebDl => "WEB-DL".to_string(),
        ReleaseKind::BluRay => "BluRay".to_string(),
        ReleaseKind::Encode => "Encode".to_string(),
        ReleaseKind::HdTv => "HDTV".to_string(),
        ReleaseKind::Dvd => "DVD".to_string(),
        ReleaseKind::Other(s) => s.clone(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::db::searches::SearchRequestJson;
    use crate::db::{open_memory, searches};

    async fn make_search(pool: &Pool) -> Uuid {
        searches::create(
            pool,
            SearchRequestJson {
                tmdb_id: Some(603),
                ..SearchRequestJson::default()
            },
        )
        .await
        .unwrap()
        .id
    }

    fn sample_insert(search_id: Uuid, score: u32) -> DecisionInsert {
        DecisionInsert {
            search_id,
            provider_id: None,
            provider_name: "capybara".into(),
            release_name: "The Matrix 1999 1080p BluRay x264".into(),
            release_id_remote: 12345,
            score,
            rejected: false,
            tags: vec!["PT-BR".into()],
            matched_rules: vec!["PT-BR audio".into(), "1080p".into()],
            seeders: 42,
            leechers: 1,
            size_bytes: 9_608_016_733,
            resolution: Resolution::P1080,
            kind: ReleaseKind::BluRay,
            download_url: Some("https://capybara/torrents/download/12345".into()),
            details_url: Some("https://capybara/torrents/12345".into()),
            provider_kind: Some("unit3d".into()),
            published_at: None,
            audio_languages: Vec::new(),
            subtitle_languages: Vec::new(),
        }
    }

    #[tokio::test]
    async fn insert_and_list_roundtrips() {
        let pool = open_memory().await.unwrap();
        let search_id = make_search(&pool).await;
        let row = insert(&pool, sample_insert(search_id, 110)).await.unwrap();
        assert_eq!(row.score, 110);
        assert_eq!(row.tags, vec!["PT-BR".to_string()]);
        assert_eq!(row.resolution, "1080p");

        let list = list_for_search(&pool, search_id).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, row.id);
    }

    #[tokio::test]
    async fn list_for_search_orders_by_score_desc() {
        let pool = open_memory().await.unwrap();
        let search_id = make_search(&pool).await;
        insert(&pool, sample_insert(search_id, 50)).await.unwrap();
        insert(&pool, sample_insert(search_id, 200)).await.unwrap();
        insert(&pool, sample_insert(search_id, 100)).await.unwrap();
        let list = list_for_search(&pool, search_id).await.unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].score, 200);
        assert_eq!(list[1].score, 100);
        assert_eq!(list[2].score, 50);
    }

    #[tokio::test]
    async fn cascade_delete_removes_decisions_when_search_dropped() {
        let pool = open_memory().await.unwrap();
        let search_id = make_search(&pool).await;
        insert(&pool, sample_insert(search_id, 10)).await.unwrap();
        sqlx::query("DELETE FROM searches WHERE id = ?")
            .bind(search_id.to_string())
            .execute(&pool)
            .await
            .unwrap();
        let list = list_for_search(&pool, search_id).await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn rejected_flag_roundtrips() {
        let pool = open_memory().await.unwrap();
        let search_id = make_search(&pool).await;
        let mut ins = sample_insert(search_id, 0);
        ins.rejected = true;
        let row = insert(&pool, ins).await.unwrap();
        assert!(row.rejected);
        let list = list_for_search(&pool, search_id).await.unwrap();
        assert!(list[0].rejected);
    }

    #[tokio::test]
    async fn download_url_roundtrips() {
        let pool = open_memory().await.unwrap();
        let search_id = make_search(&pool).await;
        let ins = sample_insert(search_id, 100);
        let row = insert(&pool, ins).await.unwrap();
        assert_eq!(
            row.download_url.as_deref(),
            Some("https://capybara/torrents/download/12345")
        );
        let fetched = get_by_id(&pool, row.id).await.unwrap();
        assert_eq!(
            fetched.download_url.as_deref(),
            Some("https://capybara/torrents/download/12345")
        );
        assert_eq!(
            fetched.details_url.as_deref(),
            Some("https://capybara/torrents/12345")
        );
    }

    #[tokio::test]
    async fn get_by_id_404s_when_missing() {
        let pool = open_memory().await.unwrap();
        let err = get_by_id(&pool, Uuid::new_v4()).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)));
    }

    #[tokio::test]
    async fn language_vectors_roundtrip() {
        let pool = open_memory().await.unwrap();
        let search_id = make_search(&pool).await;
        let mut ins = sample_insert(search_id, 150);
        ins.audio_languages = vec![Language::PtBr, Language::En];
        ins.subtitle_languages = vec![Language::PtBr];
        let row = insert(&pool, ins).await.unwrap();
        assert_eq!(row.audio_languages, vec![Language::PtBr, Language::En]);
        assert_eq!(row.subtitle_languages, vec![Language::PtBr]);
        let fetched = get_by_id(&pool, row.id).await.unwrap();
        assert_eq!(fetched.audio_languages, vec![Language::PtBr, Language::En]);
        assert_eq!(fetched.subtitle_languages, vec![Language::PtBr]);
    }

    #[tokio::test]
    async fn legacy_default_language_columns_parse_as_empty() {
        // Simulates a legacy row written before the 20260519120000
        // migration: the column default `'[]'` keeps the read path
        // working without touching every callsite.
        let pool = open_memory().await.unwrap();
        let search_id = make_search(&pool).await;
        let row = insert(&pool, sample_insert(search_id, 50)).await.unwrap();
        assert!(row.audio_languages.is_empty());
        assert!(row.subtitle_languages.is_empty());
    }
}
