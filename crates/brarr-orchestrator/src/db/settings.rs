//! `settings` table — k/v store for runtime-overridable env values.
//!
//! See `migrations/20260524130000_settings.sql` for schema notes and
//! the canonical key list. Empty values are persisted verbatim so the
//! operator can blank a key from the UI without dropping the row;
//! readers treat empty-string as "no override, fall back to env".

use std::collections::HashMap;

use sqlx::{Row, sqlite::SqliteRow};
use time::OffsetDateTime;

use crate::{AppError, db::Pool};

// ---- canonical key names -------------------------------------------

/// Admin token (replaces `BRARR_AUTH_TOKEN`).
pub const KEY_AUTH_TOKEN: &str = "auth_token";
/// Trusted-peer allowlist (replaces `BRARR_BYPASS_AUTH_FROM`).
pub const KEY_BYPASS_AUTH_FROM: &str = "bypass_auth_from";
/// Trusted-proxy list for `X-Forwarded-For` (replaces `BRARR_TRUSTED_PROXIES`).
pub const KEY_TRUSTED_PROXIES: &str = "trusted_proxies";
/// External base URL for push proxy links (replaces `BRARR_PUBLIC_URL`).
pub const KEY_PUBLIC_URL: &str = "public_url";
/// *arr poll cadence in seconds (replaces `BRARR_ARR_POLL_INTERVAL_SECS`).
pub const KEY_POLL_INTERVAL_SECS: &str = "poll_interval_secs";
/// Cadence, in seconds, of the passive \*arr sweep — the task that
/// re-reads every instance marked `sync_source` and catalogues whatever
/// appeared, always paused. Blank falls back to
/// [`crate::arr_import::DEFAULT_SYNC_INTERVAL`]. Read fresh on every
/// cycle, so an edit here lands without a restart.
pub const KEY_ARR_SYNC_INTERVAL_SECS: &str = "arr_sync_interval_secs";
/// How many days of `decisions` / `searches` history to keep before the
/// background maintenance task prunes them. `0` = keep forever (disabled).
/// Replaces `BRARR_DECISIONS_RETENTION_DAYS`.
pub const KEY_DECISIONS_RETENTION_DAYS: &str = "decisions_retention_days";
/// Keep-all override for search persistence. Truthy (`1`/`true`) ⇒
/// persist every evaluated release including ones every quality profile
/// rejects; empty / `0` ⇒ drop universally-rejected releases (default).
/// Replaces `BRARR_PERSIST_REJECTED`.
pub const KEY_PERSIST_REJECTED: &str = "persist_rejected";
/// TMDB **v4 read access token** (replaces `BRARR_TMDB_TOKEN`). Not the
/// v3 API key: that is a different string from the same account page and
/// is sent as a query parameter, so passing it here yields a 401.
pub const KEY_TMDB_TOKEN: &str = "tmdb_token";
/// Metadata language for TMDB calls. Default `pt-BR`.
pub const KEY_TMDB_LANGUAGE: &str = "tmdb_language";
/// Country used to resolve per-country release dates. Default `BR`.
pub const KEY_TMDB_COUNTRY: &str = "tmdb_country";
/// How stale a library item's metadata may get before the sweep
/// refreshes it. Default 30 days; the TMDB terms cap caching at six
/// months, so values above ~180 are not legitimate.
pub const KEY_TMDB_TTL_DAYS: &str = "tmdb_ttl_days";

/// `TheTVDB` v4 project API key. A UUID.
///
/// Write-only in the UI, like [`KEY_TMDB_TOKEN`]: blank means "keep what
/// is stored", because the field never echoes a credential back and so
/// blank cannot mean "erase".
pub const KEY_TVDB_API_KEY: &str = "tvdb_api_key";

/// Subscriber PIN, for a **user-supported** `TheTVDB` key only.
///
/// Empty for brarr's own key, which is funded by the revenue tier (free
/// under $50k/year, conditioned on attribution). Sending a PIN with a
/// project key is refused upstream, so this is omitted rather than sent
/// blank — see `brarr_tvdb::TvdbAuth::pin`.
pub const KEY_TVDB_PIN: &str = "tvdb_pin";

/// How often the `TheTVDB` numbering sweep runs, in seconds.
pub const KEY_TVDB_SYNC_INTERVAL_SECS: &str = "tvdb_sync_interval_secs";

/// `tracing-subscriber` env filter (replaces `RUST_LOG`).
pub const KEY_LOG_LEVEL: &str = "log_level";
/// Backtrace mode — `0` / `1` / `full`. Restart required (workspace
/// forbids `unsafe_code`; `std::env::set_var` is unsafe in Rust 2024).
pub const KEY_BACKTRACE: &str = "backtrace";

/// One persisted row.
#[derive(Debug, Clone)]
pub struct SettingRow {
    /// Setting key.
    pub key: String,
    /// Setting value (may be empty to mean "blanked from UI").
    pub value: String,
    /// Last update timestamp.
    pub updated_at: OffsetDateTime,
}

/// Fetch one setting by key, returning `None` when no row exists.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn get(pool: &Pool, key: &str) -> Result<Option<SettingRow>, AppError> {
    let row_opt = sqlx::query("SELECT key, value, updated_at FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    row_opt.map(|r| row_to_setting(&r)).transpose()
}

/// Insert or replace a setting. Empty `value` is persisted as-is so
/// callers can distinguish "blanked" from "unset" if they need to.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn set(pool: &Pool, key: &str, value: &str) -> Result<(), AppError> {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    sqlx::query(
        "INSERT INTO settings (key, value, updated_at) VALUES (?, ?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(key)
    .bind(value)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Load every persisted setting into a `HashMap<key, value>`. Used at
/// startup to seed [`crate::state::RuntimeConfig`] before env-var
/// fallback.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn get_all(pool: &Pool) -> Result<HashMap<String, String>, AppError> {
    let rows = sqlx::query("SELECT key, value, updated_at FROM settings")
        .fetch_all(pool)
        .await?;
    let mut out = HashMap::with_capacity(rows.len());
    for r in &rows {
        let s = row_to_setting(r)?;
        out.insert(s.key, s.value);
    }
    Ok(out)
}

/// Parse a persisted boolean flag. Truthy: `1`, `true`, `yes`, `on`
/// (case-insensitive, trimmed). Everything else — including empty,
/// meaning "unset / no override" — is `false`.
#[must_use]
pub fn parse_flag(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn row_to_setting(row: &SqliteRow) -> Result<SettingRow, AppError> {
    let updated_unix: i64 = row.try_get("updated_at")?;
    let updated_at = OffsetDateTime::from_unix_timestamp(updated_unix)
        .map_err(|e| AppError::InvalidInput(format!("invalid timestamp in settings: {e}")))?;
    Ok(SettingRow {
        key: row.try_get("key")?,
        value: row.try_get("value")?,
        updated_at,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::db::open_memory;

    #[tokio::test]
    async fn set_then_get_roundtrips() {
        let pool = open_memory().await.unwrap();
        set(&pool, KEY_PUBLIC_URL, "https://brarr.example")
            .await
            .unwrap();
        let row = get(&pool, KEY_PUBLIC_URL).await.unwrap().unwrap();
        assert_eq!(row.value, "https://brarr.example");
    }

    #[tokio::test]
    async fn set_overwrites_existing_row() {
        let pool = open_memory().await.unwrap();
        set(&pool, KEY_LOG_LEVEL, "info").await.unwrap();
        set(&pool, KEY_LOG_LEVEL, "debug").await.unwrap();
        let row = get(&pool, KEY_LOG_LEVEL).await.unwrap().unwrap();
        assert_eq!(row.value, "debug");
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let pool = open_memory().await.unwrap();
        assert!(get(&pool, "never_set").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn empty_value_is_preserved() {
        let pool = open_memory().await.unwrap();
        set(&pool, KEY_BYPASS_AUTH_FROM, "").await.unwrap();
        let row = get(&pool, KEY_BYPASS_AUTH_FROM).await.unwrap().unwrap();
        assert_eq!(row.value, "");
    }

    #[test]
    fn parse_flag_truthy_and_falsy() {
        for t in ["1", "true", "TRUE", "Yes", " on "] {
            assert!(parse_flag(t), "{t:?} should be truthy");
        }
        for f in ["", "0", "false", "no", "off", "x"] {
            assert!(!parse_flag(f), "{f:?} should be falsy");
        }
    }

    #[tokio::test]
    async fn get_all_returns_every_row() {
        let pool = open_memory().await.unwrap();
        set(&pool, KEY_PUBLIC_URL, "u").await.unwrap();
        set(&pool, KEY_LOG_LEVEL, "warn").await.unwrap();
        let map = get_all(&pool).await.unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get(KEY_PUBLIC_URL).map(String::as_str), Some("u"));
        assert_eq!(map.get(KEY_LOG_LEVEL).map(String::as_str), Some("warn"));
    }
}
