//! Push a brarr decision row to a configured Sonarr / Radarr instance.
//!
//! The autobrr-style inversion: brarr's rules engine already accepted a
//! release; this module sends it over to the matching *arr so its
//! download client can grab it. The path:
//!
//! ```text
//!   DecisionRow + ArrInstanceRow + base_url
//!         │
//!         ▼
//!   build_payload()      ── maps decision_id → /torznab/download/{id}
//!         │                  or /newznab/download/{id} based on
//!         │                  provider_kind
//!         ▼
//!   ArrClient::push_release()
//!         │
//!         ▼
//!   push_history row persisted (status + http_status + body)
//! ```
//!
//! Always writes a `push_history` row — both on success and on every
//! failure mode. The admin UI's `/pushes` page consults that table so
//! operators can audit "what brarr pushed and how *arr answered".

use brarr_arr::{ArrClient, ArrError, ArrProtocol, PushReleasePayload};
use time::OffsetDateTime;
use tracing::{info, warn};

use crate::db::arr_instances::ArrInstanceRow;
use crate::db::decisions::DecisionRow;
use crate::db::push_history::{self, NewPushHistory, PushHistoryRow, PushStatus};
use crate::{AppError, AppState};

/// Outcome of one [`push_decision`] call — the persisted history row.
/// `Ok(row)` means the row was inserted; the row's `status` field
/// tells you whether *arr accepted the push or rejected it. An `Err`
/// from this function only fires on DB-level failures (insert into
/// `push_history` failed); transport / HTTP failures are still
/// captured as a successful "I recorded the failure" return.
pub type PushOutcome = PushHistoryRow;

/// Push one decision row to one *arr instance and record the attempt
/// in `push_history`.
///
/// `public_base_url` is the externally-reachable origin (`http://host:port`)
/// that brarr's `/torznab/download/{id}` proxy lives at — *arr will
/// follow this URL from its download client, so it must resolve from
/// the *arr's network namespace (not from `127.0.0.1` if *arr runs in
/// another container).
///
/// # Errors
///
/// - [`AppError::Database`] if inserting the `push_history` row fails.
///
/// HTTP / transport errors from the *arr side are **not** propagated —
/// they're persisted into the history row and the function still
/// returns `Ok`. Callers wanting to surface those to the operator
/// should read the returned [`PushHistoryRow::status`].
#[allow(
    clippy::too_many_lines,
    reason = "linear error-record-and-return path is clearer flat than split"
)]
pub async fn push_decision(
    state: &AppState,
    decision: &DecisionRow,
    arr_instance: &ArrInstanceRow,
    public_base_url: &str,
) -> Result<PushOutcome, AppError> {
    // Embed brarr's own apikey into the download URL so *arr can fetch
    // the .torrent / .nzb later without 401ing against brarr's auth
    // middleware. The *arr download client doesn't carry headers when
    // dereferencing the URL — only the query string survives.
    // Snapshot the token as an owned String — the auth ArcSwap guard
    // only lives for the duration of `state.auth_token_owned()`, so
    // borrowing through it across the build_payload call would dangle.
    let apikey = state.auth_token_owned();
    let payload = build_payload(decision, public_base_url, apikey.as_deref());
    let client = match ArrClient::new(arr_instance.to_arr_instance()) {
        Ok(c) => c,
        Err(e) => {
            warn!(
                target: "brarr_orchestrator::push",
                decision_id = %decision.id,
                arr_id = %arr_instance.id,
                error = %e,
                "failed to build ArrClient"
            );
            return push_history::insert(
                state.pool(),
                NewPushHistory {
                    decision_id: decision.id,
                    arr_instance_id: arr_instance.id,
                    arr_instance_name: &arr_instance.name,
                    arr_kind: arr_instance.kind,
                    status: PushStatus::TransportError,
                    http_status: None,
                    response_body: Some(&format!("client builder: {e}")),
                    rejections: None,
                },
            )
            .await;
        }
    };

    match client.push_release(&payload).await {
        Ok(body) => {
            // *arr returns the parsed release + rejection array even on
            // 200. Non-empty `rejections` means "accepted the HTTP
            // request but won't grab" (e.g. wrong title, quality
            // mismatch). Parse the body so the audit page can render
            // reasons as a clean list and the log line includes them.
            let rejections = extract_rejections(&body);
            info!(
                target: "brarr_orchestrator::push",
                decision_id = %decision.id,
                arr_id = %arr_instance.id,
                arr_name = %arr_instance.name,
                rejected = rejections.as_ref().is_some_and(|v| !v.is_empty()),
                rejection_count = rejections.as_ref().map_or(0, Vec::len),
                "push accepted"
            );
            let body_opt = if body.is_empty() {
                None
            } else {
                Some(body.as_str())
            };
            push_history::insert(
                state.pool(),
                NewPushHistory {
                    decision_id: decision.id,
                    arr_instance_id: arr_instance.id,
                    arr_instance_name: &arr_instance.name,
                    arr_kind: arr_instance.kind,
                    status: PushStatus::Ok,
                    http_status: Some(200),
                    response_body: body_opt,
                    rejections,
                },
            )
            .await
        }
        Err(ArrError::Http { status, body, .. }) => {
            warn!(
                target: "brarr_orchestrator::push",
                decision_id = %decision.id,
                arr_id = %arr_instance.id,
                http_status = status,
                "push rejected by *arr"
            );
            let rejections = extract_rejections(&body);
            push_history::insert(
                state.pool(),
                NewPushHistory {
                    decision_id: decision.id,
                    arr_instance_id: arr_instance.id,
                    arr_instance_name: &arr_instance.name,
                    arr_kind: arr_instance.kind,
                    status: PushStatus::HttpError,
                    http_status: Some(status),
                    response_body: Some(&body),
                    rejections,
                },
            )
            .await
        }
        Err(e) => {
            warn!(
                target: "brarr_orchestrator::push",
                decision_id = %decision.id,
                arr_id = %arr_instance.id,
                error = %e,
                "push failed (transport)"
            );
            push_history::insert(
                state.pool(),
                NewPushHistory {
                    decision_id: decision.id,
                    arr_instance_id: arr_instance.id,
                    arr_instance_name: &arr_instance.name,
                    arr_kind: arr_instance.kind,
                    status: PushStatus::TransportError,
                    http_status: None,
                    response_body: Some(&e.to_string()),
                    rejections: None,
                },
            )
            .await
        }
    }
}

/// Translate a [`DecisionRow`] into a [`PushReleasePayload`] *arr can
/// consume.
///
/// `public_base_url` is the externally-reachable origin used to build
/// the brarr-side download proxy URL. Path prefix flips between
/// `/torznab/download/{id}` and `/newznab/download/{id}` so the *arr
/// download client routes to the matching protocol-specific handler.
fn build_payload(
    row: &DecisionRow,
    public_base_url: &str,
    apikey: Option<&str>,
) -> PushReleasePayload {
    let is_nzb = row
        .provider_kind
        .as_deref()
        .is_some_and(|k| k.eq_ignore_ascii_case("newznab"));
    let (protocol, prefix) = if is_nzb {
        (ArrProtocol::Usenet, "/newznab")
    } else {
        (ArrProtocol::Torrent, "/torznab")
    };
    let apikey_qs = match apikey {
        Some(k) if !k.is_empty() => format!("?apikey={k}"),
        _ => String::new(),
    };
    let download_url = format!(
        "{base}{prefix}/download/{id}{apikey_qs}",
        base = public_base_url.trim_end_matches('/'),
        id = row.id,
    );
    // Fall back to `decided_at` when the provider didn't carry a real
    // upload timestamp. *arr only requires a parseable date here; the
    // tracker-side "Age" filter degrades to "as old as the decision
    // run" rather than failing the push outright.
    let publish_date = row.published_at.unwrap_or(row.decided_at);
    // Optional seeders/leechers only useful for torrent items; *arr
    // ignores them on Usenet pushes.
    let (seeders, leechers) = if is_nzb {
        (None, None)
    } else {
        (Some(row.seeders), Some(row.leechers))
    };
    PushReleasePayload {
        title: row.release_name.clone(),
        download_url,
        protocol,
        publish_date,
        size: row.size_bytes,
        indexer: "brarr".to_string(),
        info_url: row.details_url.clone(),
        seeders,
        leechers,
    }
}

/// Mine the `rejections` array out of a *arr push response body.
///
/// *arr returns one of:
/// - `[]` — push accepted, no rejections, grab fired.
/// - `[ { ..., "rejections": ["reason 1", "reason 2"] } ]` — release
///   was parsed but blocked downstream (quality profile, custom
///   format, queue dedup, etc.). The audit page renders these as a
///   clean bullet list.
/// - Free-form text on 4xx/5xx — returns `None`, caller falls back
///   to the raw `response_body`.
///
/// Returns:
/// - `Some(vec![])` when the body parsed as an array of release
///   objects but no `rejections` field surfaced — confirms *arr
///   accepted cleanly.
/// - `Some(vec!["...", "..."])` when at least one release object
///   carried a `rejections` array.
/// - `None` when the body isn't parseable as JSON or has a shape
///   brarr doesn't recognize — the operator can still read the
///   raw `response_body` column.
fn extract_rejections(body: &str) -> Option<Vec<String>> {
    if body.trim().is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let releases = v.as_array()?;
    let mut out: Vec<String> = Vec::new();
    for release in releases {
        let Some(rejections) = release.get("rejections").and_then(|r| r.as_array()) else {
            continue;
        };
        for r in rejections {
            if let Some(s) = r.as_str() {
                out.push(s.to_string());
            }
        }
    }
    Some(out)
}

/// Resolve the externally-reachable origin (`scheme://host[:port]`)
/// that brarr's download proxy URLs should resolve through.
///
/// Resolution order:
///   1. [`AppState::public_url`] override (DB-backed runtime setting;
///      falls through when the operator hasn't configured one).
///   2. `BRARR_PUBLIC_URL` env var — kept as a startup fallback so
///      first-boot deployments behind a reverse proxy work without
///      having to log in and configure anything.
///   3. The per-request derivation in [`derive_request_base`] — only
///      useful when the push fires inside an HTTP handler that has
///      the request headers in hand.
///
/// The scheduled poller passes the state-derived origin via the
/// env-var-or-runtime path; the per-decision UI button passes the
/// live request origin.
#[must_use]
pub fn state_public_base_url(state: &AppState) -> Option<String> {
    if let Some(url) = state.public_url() {
        let trimmed = url.trim_end_matches('/').to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    env_public_base_url()
}

/// Env-only base URL resolver. Kept as a building block for the
/// state-aware variant and for the few startup paths that don't yet
/// have an `AppState` in scope.
#[must_use]
pub fn env_public_base_url() -> Option<String> {
    std::env::var("BRARR_PUBLIC_URL")
        .ok()
        .map(|s| s.trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
}

/// Derive the request-side origin from an axum `HeaderMap`. Mirrors
/// the rule used by the Torznab feed renderer. Prefers, in order:
/// the runtime setting on `state`, the env var, then the request's
/// own `X-Forwarded-Host` / `Host`.
#[must_use]
pub fn derive_request_base(state: &AppState, headers: &axum::http::HeaderMap) -> String {
    if let Some(url) = state_public_base_url(state) {
        return url;
    }
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(axum::http::header::HOST))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1:3000");
    format!("{scheme}://{host}")
}

#[allow(dead_code, reason = "exposed for the manual-push UI route")]
pub(crate) fn build_payload_for_test(
    row: &DecisionRow,
    public_base_url: &str,
) -> PushReleasePayload {
    build_payload(row, public_base_url, None)
}

#[allow(
    dead_code,
    reason = "used by the manual-push UI handler; kept private to the crate"
)]
pub(crate) fn fallback_publish_date() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::db::decisions::DecisionRow;
    use brarr_arr::ArrProtocol;
    use time::OffsetDateTime;
    use uuid::Uuid;

    fn decision_row(provider_kind: Option<&str>) -> DecisionRow {
        DecisionRow {
            id: Uuid::nil(),
            search_id: Uuid::nil(),
            provider_id: None,
            provider_name: "p".into(),
            release_name: "Matrix.1999.1080p.BluRay-FOO".into(),
            release_id_remote: 1,
            score: 700,
            rejected: false,
            tags: vec![],
            matched_rules: vec![],
            seeders: 42,
            leechers: 1,
            size_bytes: 1234,
            resolution: "1080p".into(),
            kind: "BluRay".into(),
            download_url: Some("https://up.example/grab".into()),
            details_url: Some("https://up.example/details".into()),
            provider_kind: provider_kind.map(String::from),
            published_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).ok(),
            audio_languages: Vec::new(),
            subtitle_languages: Vec::new(),
            profile_scores: std::collections::HashMap::new(),
            decided_at: OffsetDateTime::from_unix_timestamp(1_700_000_500).unwrap(),
        }
    }

    #[test]
    fn payload_routes_torrent_through_torznab_prefix() {
        let p = build_payload(&decision_row(Some("unit3d")), "http://h:3000", None);
        assert_eq!(p.protocol, ArrProtocol::Torrent);
        assert!(
            p.download_url.contains("/torznab/download/"),
            "{}",
            p.download_url
        );
        assert_eq!(p.seeders, Some(42));
        assert_eq!(p.leechers, Some(1));
    }

    #[test]
    fn payload_routes_nzb_through_newznab_prefix() {
        let p = build_payload(&decision_row(Some("newznab")), "http://h:3000", None);
        assert_eq!(p.protocol, ArrProtocol::Usenet);
        assert!(
            p.download_url.contains("/newznab/download/"),
            "{}",
            p.download_url
        );
        assert_eq!(p.seeders, None, "seeders must be None on usenet payload");
    }

    #[test]
    fn payload_trims_trailing_slash_on_base_url() {
        let p = build_payload(&decision_row(None), "http://h:3000/", None);
        // No double slash before /torznab/.
        assert!(!p.download_url.contains("//torznab"), "{}", p.download_url);
        assert!(p.download_url.starts_with("http://h:3000/torznab/"));
    }

    #[test]
    fn payload_falls_back_to_decided_at_when_published_at_missing() {
        let mut row = decision_row(Some("unit3d"));
        row.published_at = None;
        let p = build_payload(&row, "http://h", None);
        assert_eq!(p.publish_date, row.decided_at);
    }

    #[test]
    fn payload_uses_published_at_when_provider_carried_it() {
        let row = decision_row(Some("unit3d"));
        let p = build_payload(&row, "http://h", None);
        assert_eq!(p.publish_date.unix_timestamp(), 1_700_000_000);
    }

    #[test]
    fn extract_rejections_returns_empty_vec_for_clean_accept() {
        // `[]` = *arr accepted with no rejections; the only legit way
        // to know "grab actually fired" from the push response alone.
        let r = extract_rejections("[]").unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn extract_rejections_pulls_reasons_out_of_release_objects() {
        let body = r#"[
            {
              "title": "foo",
              "rejections": [
                "Custom Formats X have score 0 below profile minimum 1",
                "Release in queue and Quality Profile does not allow upgrades"
              ]
            }
        ]"#;
        let r = extract_rejections(body).unwrap();
        assert_eq!(r.len(), 2);
        assert!(r[0].contains("Custom Formats"));
        assert!(r[1].contains("queue"));
    }

    #[test]
    fn extract_rejections_returns_none_for_malformed_body() {
        assert!(extract_rejections("").is_none());
        assert!(extract_rejections("not json").is_none());
        // Object instead of array — *arr error pages sometimes look
        // like this.
        assert!(extract_rejections(r#"{"message":"bad"}"#).is_none());
    }

    #[test]
    fn extract_rejections_handles_release_without_rejections_field() {
        // Spec-compliant clean response: each release object has no
        // `rejections` key at all.
        let body = r#"[{"title":"foo"}]"#;
        let r = extract_rejections(body).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn payload_embeds_apikey_in_download_url_when_auth_enabled() {
        let p = build_payload(
            &decision_row(Some("unit3d")),
            "http://brarr:3000",
            Some("supersecret123"),
        );
        assert!(
            p.download_url.contains("?apikey=supersecret123"),
            "{}",
            p.download_url
        );
    }

    #[test]
    fn payload_omits_apikey_query_when_disabled() {
        let p = build_payload(&decision_row(Some("unit3d")), "http://brarr:3000", None);
        assert!(!p.download_url.contains("apikey"), "{}", p.download_url);
    }

    #[test]
    fn payload_omits_apikey_query_when_empty_token() {
        let p = build_payload(&decision_row(Some("unit3d")), "http://brarr:3000", Some(""));
        assert!(!p.download_url.contains("apikey"), "{}", p.download_url);
    }

    #[test]
    fn legacy_provider_kind_none_routes_to_torrent() {
        // Decisions persisted before the provider_kind migration have
        // None — they should default to torrent since brarr's
        // historical default was UNIT3D.
        let p = build_payload(&decision_row(None), "http://h", None);
        assert_eq!(p.protocol, ArrProtocol::Torrent);
    }
}
