//! Tests against the **real** `TheTVDB` API.
//!
//! Ignored by default: they need a key and a network, and CI has
//! neither. Run them when the client changes, or when a response shape
//! is in doubt:
//!
//! ```text
//! BRARR_TVDB_API_KEY=$(cat tvdbapikey.txt) \
//!   cargo test -p brarr-tvdb --test live_api -- --ignored --nocapture
//! ```
//!
//! `brarr-tmdb`'s fixtures carry a note saying they were derived from
//! the schema rather than captured, because no token existed when it was
//! written. This is the other half of that: the wiremock suite pins the
//! shapes brarr expects, and these prove the shapes are real.

#![allow(
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on happy paths"
)]

use std::collections::BTreeMap;

use brarr_tvdb::{SeasonType, TvdbAuth, TvdbClient};

/// `None` when the key is not in the environment, which is how these
/// stay skippable without a `cfg`.
fn client() -> Option<TvdbClient> {
    let key = std::env::var("BRARR_TVDB_API_KEY").ok()?;
    let key = key.trim().to_owned();
    if key.is_empty() {
        return None;
    }
    Some(
        TvdbClient::new(TvdbAuth {
            api_key: key,
            pin: std::env::var("BRARR_TVDB_PIN")
                .ok()
                .map(|p| p.trim().to_owned())
                .filter(|p| !p.is_empty()),
        })
        .unwrap(),
    )
}

/// Episodes per season, specials excluded, in season order.
fn shape(episodes: &[brarr_tvdb::Episode]) -> Vec<usize> {
    let mut by_season: BTreeMap<i32, usize> = BTreeMap::new();
    for e in episodes.iter().filter(|e| e.season_number > 0) {
        *by_season.entry(e.season_number).or_default() += 1;
    }
    by_season.into_values().collect()
}

/// **The whole reason this crate exists, against live data.**
///
/// TMDB models Dragon Ball Super as one season of 131. The operator's
/// disk, Sonarr, and every release call it five seasons of
/// 14/13/19/30/55 — and so does `TheTVDB`'s `official` season type.
#[tokio::test]
#[ignore = "needs BRARR_TVDB_API_KEY and a network"]
async fn dragon_ball_super_is_five_seasons_under_official() {
    let Some(client) = client() else {
        eprintln!("BRARR_TVDB_API_KEY unset — skipping");
        return;
    };
    let found = client
        .series_episodes(295_068, SeasonType::Official, None)
        .await
        .unwrap();

    assert_eq!(
        shape(&found.episodes),
        vec![14, 13, 19, 30, 55],
        "this is the split on the operator's disk, and TMDB has none of it"
    );

    // **The name comes back in the original language** — this endpoint
    // answered `ドラゴンボール超[スーパー]`, not "Dragon Ball Super". The
    // `/{lang}` variant translates it. Harmless for brarr, which takes
    // every title from TMDB, and pinned here so nobody wires this field
    // into a screen expecting Portuguese.
    assert!(found.series_name.is_some());

    // The absolute number is what joins this back to TMDB's flat season:
    // arc 2 episode 1 is the fifteenth of the series.
    let arc2e1 = found
        .episodes
        .iter()
        .find(|e| e.season_number == 2 && e.number == 1)
        .expect("S02E01 exists");
    assert_eq!(arc2e1.absolute_number, Some(15));
}

/// Solo Leveling: one season of 25 on TMDB, two of 12 and 13 here, and
/// every release follows this one. The operator hit this by hand before
/// there was anything to derive it from.
#[tokio::test]
#[ignore = "needs BRARR_TVDB_API_KEY and a network"]
async fn solo_leveling_is_split_where_releases_split_it() {
    let Some(client) = client() else {
        eprintln!("BRARR_TVDB_API_KEY unset — skipping");
        return;
    };
    let found = client
        .series_episodes(389_597, SeasonType::Official, None)
        .await
        .unwrap();
    assert_eq!(shape(&found.episodes), vec![12, 13]);
}

/// The absolute axis is a single run of numbers, and it is what an anime
/// release named `Série - 224` carries instead of a marker.
#[tokio::test]
#[ignore = "needs BRARR_TVDB_API_KEY and a network"]
async fn the_absolute_season_type_is_one_long_run() {
    let Some(client) = client() else {
        eprintln!("BRARR_TVDB_API_KEY unset — skipping");
        return;
    };
    let found = client
        .series_episodes(295_068, SeasonType::Absolute, None)
        .await
        .unwrap();
    let shape = shape(&found.episodes);
    assert_eq!(shape.len(), 1, "absolute is one season, got {shape:?}");
    assert_eq!(shape[0], 131);
}

/// Pagination is real: Yu-Gi-Oh! is 224 episodes and the walk must
/// return all of them, not one page of them.
#[tokio::test]
#[ignore = "needs BRARR_TVDB_API_KEY and a network"]
async fn a_long_series_is_walked_to_the_end() {
    let Some(client) = client() else {
        eprintln!("BRARR_TVDB_API_KEY unset — skipping");
        return;
    };
    let found = client
        .series_episodes(76_894, SeasonType::Absolute, None)
        .await
        .unwrap();
    assert!(
        found.episodes.len() > 200,
        "expected the whole run, got {}",
        found.episodes.len()
    );
    // Deduplicated by episode id, so a record repeated across a page
    // boundary does not inflate the count.
    let ids: std::collections::HashSet<i64> = found.episodes.iter().map(|e| e.id).collect();
    assert_eq!(ids.len(), found.episodes.len(), "no duplicates survived");
}

/// A real key logs in, and the token is good for a month.
#[tokio::test]
#[ignore = "needs BRARR_TVDB_API_KEY and a network"]
async fn the_key_authenticates() {
    let Some(client) = client() else {
        eprintln!("BRARR_TVDB_API_KEY unset — skipping");
        return;
    };
    client.verify().await.unwrap();
}
