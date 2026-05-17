//! [`TrackerProvider`] impl for [`Unit3dClient`].
//!
//! Bridges the strongly-typed `Unit3dClient::search_by_tmdb` API to the
//! erased-error `TrackerProvider` contract from `brarr_core`. Lives in a
//! dedicated module so the core client surface stays free of trait
//! boilerplate.

use brarr_core::{ProviderError, ProviderFuture, Release, TmdbId, TrackerProvider, TvdbId};

use crate::Unit3dClient;

impl TrackerProvider for Unit3dClient {
    fn name(&self) -> &str {
        &self.tracker_source().name
    }

    fn search_by_tmdb(
        &self,
        tmdb: TmdbId,
    ) -> ProviderFuture<'_, Result<Vec<Release>, ProviderError>> {
        let source_name = self.tracker_source().name.clone();
        Box::pin(async move {
            self.search_by_tmdb(tmdb)
                .await
                .map_err(|e| ProviderError::new(source_name, e.to_string()))
        })
    }

    fn search_by_tvdb(
        &self,
        tvdb: TvdbId,
        season: Option<u16>,
        episode: Option<u16>,
    ) -> ProviderFuture<'_, Result<Vec<Release>, ProviderError>> {
        let source_name = self.tracker_source().name.clone();
        Box::pin(async move {
            self.search_by_tvdb(tvdb, season, episode)
                .await
                .map_err(|e| ProviderError::new(source_name, e.to_string()))
        })
    }
}
