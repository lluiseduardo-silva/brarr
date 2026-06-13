//! `arr_instances` table — Sonarr / Radarr endpoints brarr pushes
//! releases to.
//!
//! See `migrations/20260517130000_arr_instances.sql` for the schema
//! rationale (TL;DR: autobrr-style inversion — brarr decides, *arr
//! grabs). The admin UI writes through this module; the search
//! pipeline reads through it on every auto-push attempt.

use brarr_arr::{ArrInstance, ArrKind};
use sqlx::{Row, sqlite::SqliteRow};
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;

use crate::{AppError, db::Pool};

/// One configured *arr endpoint.
#[derive(Debug, Clone)]
pub struct ArrInstanceRow {
    /// Stable UUID v4 used in URLs and push history rows.
    pub id: Uuid,
    /// Operator-chosen display name. Unique.
    pub name: String,
    /// Sonarr / Radarr.
    pub kind: ArrKind,
    /// Base URL (`https://radarr.example/` or `.../radarr/`).
    pub base_url: Url,
    /// API key for the *arr's `X-Api-Key` header.
    pub api_key: String,
    /// Minimum [`brarr_core::DecisionScore`] required to auto-push
    /// when no quality profile is attached. Profile, when present,
    /// supersedes this value. Below the resolved threshold the
    /// release is persisted as a decision but never pushed.
    pub push_threshold: u32,
    /// Attached quality profile, if any. `None` falls back to
    /// `push_threshold` above. Resolved by `effective_threshold`
    /// in the push pipeline.
    pub profile_id: Option<Uuid>,
    /// `false` short-circuits push without deleting the row — useful
    /// for "drain mode" where the operator wants to silence one *arr
    /// without losing its config.
    pub enabled: bool,
    /// `true` ⇒ this instance is driven by inbound webhooks; the
    /// scheduled poller skips it (the manual "rodar agora" button still
    /// works). Default `false`.
    pub webhook_driven: bool,
    /// Row creation timestamp.
    pub created_at: OffsetDateTime,
}

impl ArrInstanceRow {
    /// Convert to a [`brarr_arr::ArrInstance`] usable by the HTTP
    /// client. Cheap clone — the wrapping client holds the
    /// `reqwest::Client` separately.
    #[must_use]
    pub fn to_arr_instance(&self) -> ArrInstance {
        ArrInstance {
            name: self.name.clone(),
            kind: self.kind,
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
        }
    }
}

/// Bundle of values used to create a new *arr instance row.
#[derive(Debug, Clone)]
pub struct NewArrInstance<'a> {
    /// Display name. Must be unique.
    pub name: &'a str,
    /// `ArrKind::Sonarr` / `ArrKind::Radarr`.
    pub kind: ArrKind,
    /// Base URL.
    pub base_url: &'a Url,
    /// API key.
    pub api_key: &'a str,
    /// Push threshold (0..=1000). Defaults to 150 when `None`.
    /// Ignored at push time when `profile_id` is set.
    pub push_threshold: Option<u32>,
    /// Optional quality-profile attachment. When set, the profile's
    /// threshold wins over `push_threshold` above.
    pub profile_id: Option<Uuid>,
    /// Enabled flag. Defaults to `true` when `None`.
    pub enabled: Option<bool>,
}

/// Insert a new *arr instance, returning the persisted row.
///
/// # Errors
///
/// - [`AppError::InvalidInput`] when the name is empty or the
///   threshold falls outside `0..=1000`.
/// - [`AppError::Database`] on `UNIQUE(name)` violation or other SQL
///   error.
pub async fn insert(pool: &Pool, new: NewArrInstance<'_>) -> Result<ArrInstanceRow, AppError> {
    if new.name.trim().is_empty() {
        return Err(AppError::InvalidInput(
            "arr instance name cannot be empty".into(),
        ));
    }
    // Default 150 ≈ "PT-BR audio confirmado + 1 bonus de qualidade
    // ou ~10 seeders". Baseline scoring tops out around 230 (PT-BR
    // audio=100 + sub=50 + 2160p=20 + HDR=10 + seeders cap=50), então
    // qualquer threshold acima disso na prática silencia o push.
    let threshold = new.push_threshold.unwrap_or(150);
    if threshold > 1000 {
        return Err(AppError::InvalidInput(format!(
            "push_threshold must be 0..=1000, got {threshold}"
        )));
    }
    let enabled = new.enabled.unwrap_or(true);
    let id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO arr_instances (id, name, kind, base_url, api_key, push_threshold, profile_id, enabled, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(new.name)
    .bind(new.kind.label())
    .bind(new.base_url.as_str())
    .bind(new.api_key)
    .bind(i64::from(threshold))
    .bind(new.profile_id.map(|u| u.to_string()))
    .bind(i64::from(u8::from(enabled)))
    .bind(now.unix_timestamp())
    .execute(pool)
    .await?;
    Ok(ArrInstanceRow {
        id,
        name: new.name.to_string(),
        kind: new.kind,
        base_url: new.base_url.clone(),
        api_key: new.api_key.to_string(),
        push_threshold: threshold,
        profile_id: new.profile_id,
        enabled,
        webhook_driven: false,
        created_at: now,
    })
}

/// List every *arr instance, ordered by name ascending.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn list_all(pool: &Pool) -> Result<Vec<ArrInstanceRow>, AppError> {
    let rows = sqlx::query(
        "SELECT id, name, kind, base_url, api_key, push_threshold, profile_id, enabled, webhook_driven, created_at \
         FROM arr_instances ORDER BY name ASC",
    )
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_instance).collect()
}

/// List only enabled *arr instances. Used by the auto-push path so
/// disabled rows don't get hit.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn list_enabled(pool: &Pool) -> Result<Vec<ArrInstanceRow>, AppError> {
    let rows = sqlx::query(
        "SELECT id, name, kind, base_url, api_key, push_threshold, profile_id, enabled, webhook_driven, created_at \
         FROM arr_instances WHERE enabled = 1 ORDER BY name ASC",
    )
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_instance).collect()
}

/// Fetch one row by id.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] if no row matches.
pub async fn get_by_id(pool: &Pool, id: Uuid) -> Result<ArrInstanceRow, AppError> {
    let row_opt = sqlx::query(
        "SELECT id, name, kind, base_url, api_key, push_threshold, profile_id, enabled, webhook_driven, created_at \
         FROM arr_instances WHERE id = ?",
    )
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;
    match row_opt {
        Some(row) => row_to_instance(&row),
        None => Err(AppError::NotFound(format!("arr_instance {id}"))),
    }
}

/// Update the `push_threshold` field of one row in-place.
///
/// # Errors
///
/// - [`AppError::InvalidInput`] when `threshold > 1000`.
/// - [`AppError::NotFound`] when no row matches the id.
/// - [`AppError::Database`] on SQL failure.
pub async fn update_threshold(
    pool: &Pool,
    id: Uuid,
    threshold: u32,
) -> Result<ArrInstanceRow, AppError> {
    if threshold > 1000 {
        return Err(AppError::InvalidInput(format!(
            "push_threshold must be 0..=1000, got {threshold}"
        )));
    }
    let res = sqlx::query("UPDATE arr_instances SET push_threshold = ? WHERE id = ?")
        .bind(i64::from(threshold))
        .bind(id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("arr_instance {id}")));
    }
    get_by_id(pool, id).await
}

/// Delete a row by id. Returns `true` if a row was removed.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn delete_by_id(pool: &Pool, id: Uuid) -> Result<bool, AppError> {
    let res = sqlx::query("DELETE FROM arr_instances WHERE id = ?")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Flip the `enabled` flag in place. Mirrors
/// [`crate::db::providers::set_enabled`] so the UI's "drain mode"
/// toggle works the same way for both surfaces.
///
/// # Errors
///
/// - [`AppError::NotFound`] when no row matches `id`.
/// - [`AppError::Database`] on SQL failure.
pub async fn set_enabled(pool: &Pool, id: Uuid, enabled: bool) -> Result<ArrInstanceRow, AppError> {
    let res = sqlx::query("UPDATE arr_instances SET enabled = ? WHERE id = ?")
        .bind(i64::from(u8::from(enabled)))
        .bind(id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("arr_instance {id}")));
    }
    get_by_id(pool, id).await
}

/// Flip the `webhook_driven` flag in place. When `true`, the scheduled
/// poller skips this instance (see [`crate::poll`]); the manual poll
/// button is unaffected.
///
/// # Errors
///
/// - [`AppError::NotFound`] when no row matches `id`.
/// - [`AppError::Database`] on SQL failure.
pub async fn set_webhook_driven(
    pool: &Pool,
    id: Uuid,
    webhook_driven: bool,
) -> Result<ArrInstanceRow, AppError> {
    let res = sqlx::query("UPDATE arr_instances SET webhook_driven = ? WHERE id = ?")
        .bind(i64::from(u8::from(webhook_driven)))
        .bind(id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("arr_instance {id}")));
    }
    get_by_id(pool, id).await
}

/// Bundle of fields the edit form may overwrite.
#[derive(Debug, Clone)]
pub struct ArrInstanceUpdate<'a> {
    /// New display name.
    pub name: &'a str,
    /// New base URL.
    pub base_url: &'a Url,
    /// New api key.
    pub api_key: &'a str,
    /// New push threshold. Validated 0..=1000.
    pub push_threshold: u32,
    /// Attached quality profile id. `None` detaches.
    pub profile_id: Option<Uuid>,
}

/// Rewrite the editable fields of one row. Kind is intentionally
/// not editable here — switching Sonarr ↔ Radarr changes the
/// semantics of every linked push_history row, which the operator
/// should resolve by deleting + recreating instead.
///
/// # Errors
///
/// - [`AppError::InvalidInput`] when `name` is empty or `push_threshold > 1000`.
/// - [`AppError::NotFound`] when no row matches `id`.
/// - [`AppError::Database`] on SQL failure (including `UNIQUE(name)` violation).
pub async fn update(
    pool: &Pool,
    id: Uuid,
    upd: ArrInstanceUpdate<'_>,
) -> Result<ArrInstanceRow, AppError> {
    if upd.name.trim().is_empty() {
        return Err(AppError::InvalidInput(
            "arr instance name cannot be empty".into(),
        ));
    }
    if upd.push_threshold > 1000 {
        return Err(AppError::InvalidInput(format!(
            "push_threshold must be 0..=1000, got {}",
            upd.push_threshold
        )));
    }
    let res = sqlx::query(
        "UPDATE arr_instances SET name = ?, base_url = ?, api_key = ?, push_threshold = ?, profile_id = ? \
         WHERE id = ?",
    )
    .bind(upd.name)
    .bind(upd.base_url.as_str())
    .bind(upd.api_key)
    .bind(i64::from(upd.push_threshold))
    .bind(upd.profile_id.map(|u| u.to_string()))
    .bind(id.to_string())
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("arr_instance {id}")));
    }
    get_by_id(pool, id).await
}

fn row_to_instance(row: &SqliteRow) -> Result<ArrInstanceRow, AppError> {
    let id_str: String = row.try_get("id")?;
    let id = Uuid::parse_str(&id_str)
        .map_err(|e| AppError::InvalidInput(format!("invalid uuid in arr_instances.id: {e}")))?;
    let kind_str: String = row.try_get("kind")?;
    let kind = match kind_str.as_str() {
        "sonarr" => ArrKind::Sonarr,
        "radarr" => ArrKind::Radarr,
        other => {
            return Err(AppError::InvalidInput(format!(
                "unknown arr_instances.kind: {other}"
            )));
        }
    };
    let base_url_str: String = row.try_get("base_url")?;
    let base_url = Url::parse(&base_url_str).map_err(|e| {
        AppError::InvalidInput(format!("invalid url in arr_instances.base_url: {e}"))
    })?;
    let threshold_i64: i64 = row.try_get("push_threshold")?;
    let push_threshold = u32::try_from(threshold_i64).unwrap_or(0);
    let profile_id_str: Option<String> = row.try_get("profile_id")?;
    let profile_id = profile_id_str
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|e| {
            AppError::InvalidInput(format!("invalid uuid in arr_instances.profile_id: {e}"))
        })?;
    let enabled_i64: i64 = row.try_get("enabled")?;
    let webhook_driven_i64: i64 = row.try_get("webhook_driven")?;
    let created_unix: i64 = row.try_get("created_at")?;
    let created_at = OffsetDateTime::from_unix_timestamp(created_unix)
        .map_err(|e| AppError::InvalidInput(format!("invalid timestamp: {e}")))?;
    Ok(ArrInstanceRow {
        id,
        name: row.try_get("name")?,
        kind,
        base_url,
        api_key: row.try_get("api_key")?,
        push_threshold,
        profile_id,
        enabled: enabled_i64 != 0,
        webhook_driven: webhook_driven_i64 != 0,
        created_at,
    })
}

/// Update which quality profile is attached to one instance. Pass
/// `None` to detach (falls back to the row's own `push_threshold`
/// integer).
///
/// # Errors
///
/// - [`AppError::NotFound`] when no row matches `id`.
/// - [`AppError::Database`] on SQL failure.
pub async fn update_profile_id(
    pool: &Pool,
    id: Uuid,
    profile_id: Option<Uuid>,
) -> Result<ArrInstanceRow, AppError> {
    let res = sqlx::query("UPDATE arr_instances SET profile_id = ? WHERE id = ?")
        .bind(profile_id.map(|u| u.to_string()))
        .bind(id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("arr_instance {id}")));
    }
    get_by_id(pool, id).await
}

/// Resolve the effective push threshold for one *arr instance —
/// reads the attached profile's threshold when set, otherwise falls
/// back to the row's `push_threshold` integer.
///
/// # Errors
///
/// - [`AppError::NotFound`] when `profile_id` points at a deleted
///   profile (shouldn't happen in practice: the FK is
///   `ON DELETE SET NULL`).
/// - [`AppError::Database`] on SQL failure.
pub async fn effective_threshold(pool: &Pool, row: &ArrInstanceRow) -> Result<u32, AppError> {
    if let Some(pid) = row.profile_id {
        let p = crate::db::quality_profiles::get_by_id(pool, pid).await?;
        Ok(p.push_threshold)
    } else {
        Ok(row.push_threshold)
    }
}

/// Resolve the [`brarr_decision_service::Engine`] for one *arr
/// instance. When the row has a profile attached the engine is built
/// from the profile's persisted rule list (falling back to baseline
/// when the list is empty); otherwise the baseline engine is returned.
/// Used by the push pipeline so a Sonarr "Anime JP" profile can score
/// releases differently from a Radarr "Filmes dublados" profile even
/// though both consume the same search results.
///
/// # Errors
///
/// - [`AppError::NotFound`] when `profile_id` points at a deleted
///   profile.
/// - [`AppError::Database`] on SQL failure.
pub async fn effective_engine(
    pool: &Pool,
    row: &ArrInstanceRow,
) -> Result<brarr_decision_service::Engine, AppError> {
    if let Some(pid) = row.profile_id {
        let p = crate::db::quality_profiles::get_by_id(pool, pid).await?;
        Ok(brarr_decision_service::Engine::from_profile_rules(p.rules))
    } else {
        Ok(brarr_decision_service::Engine::baseline())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::db::open_memory;

    fn ni<'a>(name: &'a str, kind: ArrKind, base: &'a Url, key: &'a str) -> NewArrInstance<'a> {
        NewArrInstance {
            name,
            kind,
            base_url: base,
            api_key: key,
            push_threshold: None,
            profile_id: None,
            enabled: None,
        }
    }

    #[tokio::test]
    async fn insert_and_list_roundtrips() {
        let pool = open_memory().await.unwrap();
        let url = Url::parse("https://radarr.example/").unwrap();
        let row = insert(&pool, ni("radarr-main", ArrKind::Radarr, &url, "k"))
            .await
            .unwrap();
        assert_eq!(row.kind, ArrKind::Radarr);
        assert_eq!(row.push_threshold, 150);
        assert!(row.enabled);

        let all = list_all(&pool).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "radarr-main");
    }

    #[tokio::test]
    async fn duplicate_name_violates_unique() {
        let pool = open_memory().await.unwrap();
        let url = Url::parse("https://x/").unwrap();
        insert(&pool, ni("dupe", ArrKind::Sonarr, &url, "k"))
            .await
            .unwrap();
        let err = insert(&pool, ni("dupe", ArrKind::Radarr, &url, "k"))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Database(_)));
    }

    #[tokio::test]
    async fn empty_name_rejected() {
        let pool = open_memory().await.unwrap();
        let url = Url::parse("https://x/").unwrap();
        let err = insert(&pool, ni("  ", ArrKind::Sonarr, &url, "k"))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn threshold_above_1000_rejected() {
        let pool = open_memory().await.unwrap();
        let url = Url::parse("https://x/").unwrap();
        let mut new = ni("over", ArrKind::Radarr, &url, "k");
        new.push_threshold = Some(1500);
        let err = insert(&pool, new).await.unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn list_enabled_skips_disabled_rows() {
        let pool = open_memory().await.unwrap();
        let url = Url::parse("https://x/").unwrap();
        let mut on = ni("on", ArrKind::Radarr, &url, "k");
        on.enabled = Some(true);
        let mut off = ni("off", ArrKind::Sonarr, &url, "k");
        off.enabled = Some(false);
        insert(&pool, on).await.unwrap();
        insert(&pool, off).await.unwrap();
        let enabled = list_enabled(&pool).await.unwrap();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].name, "on");
        assert_eq!(list_all(&pool).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn webhook_driven_defaults_false_and_roundtrips() {
        let pool = open_memory().await.unwrap();
        let url = Url::parse("https://x/").unwrap();
        let row = insert(&pool, ni("wh", ArrKind::Radarr, &url, "k"))
            .await
            .unwrap();
        assert!(!row.webhook_driven);
        let on = set_webhook_driven(&pool, row.id, true).await.unwrap();
        assert!(on.webhook_driven);
        assert!(get_by_id(&pool, row.id).await.unwrap().webhook_driven);
        let off = set_webhook_driven(&pool, row.id, false).await.unwrap();
        assert!(!off.webhook_driven);
    }

    #[tokio::test]
    async fn delete_returns_true_only_when_row_existed() {
        let pool = open_memory().await.unwrap();
        let url = Url::parse("https://x/").unwrap();
        let row = insert(&pool, ni("t", ArrKind::Radarr, &url, "k"))
            .await
            .unwrap();
        assert!(delete_by_id(&pool, row.id).await.unwrap());
        assert!(!delete_by_id(&pool, row.id).await.unwrap());
    }

    #[tokio::test]
    async fn get_by_id_404s_when_missing() {
        let pool = open_memory().await.unwrap();
        let err = get_by_id(&pool, Uuid::new_v4()).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)));
    }

    #[tokio::test]
    async fn update_threshold_persists_new_value() {
        let pool = open_memory().await.unwrap();
        let url = Url::parse("https://x/").unwrap();
        let row = insert(&pool, ni("t", ArrKind::Radarr, &url, "k"))
            .await
            .unwrap();
        assert_eq!(row.push_threshold, 150);
        let updated = update_threshold(&pool, row.id, 200).await.unwrap();
        assert_eq!(updated.push_threshold, 200);
        let fetched = get_by_id(&pool, row.id).await.unwrap();
        assert_eq!(fetched.push_threshold, 200);
    }

    #[tokio::test]
    async fn update_threshold_rejects_above_1000() {
        let pool = open_memory().await.unwrap();
        let url = Url::parse("https://x/").unwrap();
        let row = insert(&pool, ni("t", ArrKind::Radarr, &url, "k"))
            .await
            .unwrap();
        let err = update_threshold(&pool, row.id, 1500).await.unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn update_threshold_404s_on_unknown_id() {
        let pool = open_memory().await.unwrap();
        let err = update_threshold(&pool, Uuid::new_v4(), 200)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)));
    }

    #[tokio::test]
    async fn effective_engine_falls_back_to_baseline_without_profile() {
        let pool = open_memory().await.unwrap();
        let url = Url::parse("https://r.example/").unwrap();
        let row = insert(&pool, ni("solo", ArrKind::Radarr, &url, "k"))
            .await
            .unwrap();
        let engine = effective_engine(&pool, &row).await.unwrap();
        // Build a fixture release and compare against baseline output.
        let baseline = brarr_decision_service::Engine::baseline();
        let release = brarr_core::Release::new(
            "1",
            brarr_core::TrackerSource::new("t", Url::parse("https://t.example/").unwrap()).unwrap(),
            "Some.Movie.1080p.WEB-DL",
            brarr_core::ReleaseKind::WebDl,
            brarr_core::Resolution::P1080,
            0,
        )
        .unwrap();
        let a = engine.evaluate(&release);
        let b = baseline.evaluate(&release);
        assert_eq!(a.score.get(), b.score.get());
    }

    #[tokio::test]
    async fn effective_engine_uses_profile_rules_when_attached() {
        use brarr_decision_service::{AudioFilter, Condition, Rule, RuleSet};
        let pool = open_memory().await.unwrap();
        // Seed a custom profile with a single big rule.
        let profile = crate::db::quality_profiles::insert(
            &pool,
            crate::db::quality_profiles::NewQualityProfile {
                name: "anime jp",
                description: None,
                push_threshold: 100,
            },
        )
        .await
        .unwrap();
        let custom_rules = RuleSet {
            rules: vec![Rule {
                name: Some("PT subtitle only".into()),
                when: Condition {
                    audio: Some(AudioFilter::PtBr),
                    ..Condition::default()
                },
                add_score: 700,
                tag: None,
                reject: false,
            }],
        };
        crate::db::quality_profiles::update_rules(&pool, profile.id, &custom_rules)
            .await
            .unwrap();
        let url = Url::parse("https://r.example/").unwrap();
        let mut new = ni("attached", ArrKind::Radarr, &url, "k");
        new.profile_id = Some(profile.id);
        let row = insert(&pool, new).await.unwrap();
        let engine = effective_engine(&pool, &row).await.unwrap();
        let tracker =
            brarr_core::TrackerSource::new("t", Url::parse("https://t.example/").unwrap()).unwrap();
        let mut release = brarr_core::Release::new(
            "1",
            tracker,
            "x",
            brarr_core::ReleaseKind::WebDl,
            brarr_core::Resolution::P1080,
            0,
        )
        .unwrap();
        release.enrichment = Some(brarr_core::ReleaseEnrichment {
            audio_languages: vec![brarr_core::Language::PtBr],
            ..brarr_core::ReleaseEnrichment::default()
        });
        let out = engine.evaluate(&release);
        // Custom rule: 700 + 0 seeders. Baseline would give 110.
        assert_eq!(out.score.get(), 700);
    }

    #[tokio::test]
    async fn row_converts_to_arr_instance() {
        let pool = open_memory().await.unwrap();
        let url = Url::parse("https://r.example/radarr/").unwrap();
        let row = insert(&pool, ni("conv", ArrKind::Radarr, &url, "abc"))
            .await
            .unwrap();
        let inst = row.to_arr_instance();
        assert_eq!(inst.name, "conv");
        assert_eq!(inst.kind, ArrKind::Radarr);
        assert_eq!(inst.base_url, url);
        assert_eq!(inst.api_key, "abc");
    }
}
