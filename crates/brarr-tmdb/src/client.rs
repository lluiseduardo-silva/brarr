//! Async HTTP client for the TMDB v3 API.

use std::time::Duration;

use reqwest::{Client, StatusCode, header};
use serde::de::DeserializeOwned;
use url::Url;

use crate::dto;
use crate::error::TmdbError;
use crate::model::{FindResults, MovieDetails, MovieSummary, SeasonDetails, TvDetails, TvSummary};
use crate::retry::{RetryConfig, run_with_retry};

/// Public TMDB v3 base. Overridable so tests can point at a mock.
const DEFAULT_BASE_URL: &str = "https://api.themoviedb.org/3/";

/// Timeout for a single request. Metadata calls are small; a slow one is
/// a broken one.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// Advertised on every request so TMDB can attribute the traffic.
const USER_AGENT: &str = concat!("brarr/", env!("CARGO_PKG_VERSION"));

/// Default metadata language. TMDB has no automatic fallback, so
/// [`crate::model`] walks pt-BR → pt-PT → en-US when a field is empty.
const DEFAULT_LANGUAGE: &str = "pt-BR";

/// Default country for release-date lookups.
const DEFAULT_COUNTRY: &str = "BR";

/// Everything appended to a movie details call. TMDB allows up to 20
/// appended namespaces; three is well inside that and turns what would
/// be four round-trips into one.
const MOVIE_APPEND: &str = "external_ids,release_dates,translations";

/// Series need no `release_dates` — air dates live on the season records.
const TV_APPEND: &str = "external_ids,translations";

/// How the credential is presented to TMDB.
///
/// The same account page issues two different strings and they are *not*
/// interchangeable:
///
/// - the **v4 read access token** is a JWT and travels in an
///   `Authorization: Bearer` header;
/// - the **v3 API key** is 32 hex characters and travels as an `api_key`
///   query parameter.
///
/// Sending one the other's way yields a 401 (verified against the live
/// API). Rather than make the operator work out which they pasted, the
/// client detects the shape and uses the matching mechanism.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Auth {
    /// v4 read access token, sent as a bearer header.
    BearerToken,
    /// v3 API key, appended to every request's query string.
    ApiKey(String),
}

impl Auth {
    /// JWTs are three base64url segments and always start with `eyJ`
    /// (`{"` encoded). Anything else is treated as a v3 key.
    fn detect(credential: &str) -> Self {
        if credential.starts_with("eyJ") {
            Self::BearerToken
        } else {
            Self::ApiKey(credential.to_owned())
        }
    }
}

/// Client for the TMDB v3 API.
///
/// Cheap to clone: the inner `reqwest::Client` is reference counted, so
/// a single instance can be shared across tasks.
///
/// Accepts either TMDB credential — see [`Auth`] for how they differ and
/// how the right one is picked.
#[derive(Debug, Clone)]
pub struct TmdbClient {
    http: Client,
    base_url: Url,
    auth: Auth,
    language: String,
    country: String,
    retry: RetryConfig,
}

impl TmdbClient {
    /// Build a client from a TMDB credential — either the v4 read access
    /// token or the v3 API key. The shape decides how it is sent; see
    /// [`Auth`].
    ///
    /// # Errors
    ///
    /// - [`TmdbError::InvalidToken`] when the credential is blank, or
    ///   cannot become an HTTP header value.
    /// - [`TmdbError::ClientBuild`] when the `reqwest` builder fails.
    /// - [`TmdbError::BadUrl`] if the compiled-in base URL is unparseable.
    pub fn new(credential: &str) -> Result<Self, TmdbError> {
        let credential = credential.trim();
        if credential.is_empty() {
            return Err(TmdbError::InvalidToken);
        }
        let auth = Auth::detect(credential);

        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            header::HeaderValue::from_static("application/json"),
        );
        if auth == Auth::BearerToken {
            let mut value = header::HeaderValue::from_str(&format!("Bearer {credential}"))
                .map_err(|_| TmdbError::InvalidToken)?;
            value.set_sensitive(true);
            headers.insert(header::AUTHORIZATION, value);
        }

        let http = Client::builder()
            .user_agent(USER_AGENT)
            .default_headers(headers)
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .map_err(TmdbError::ClientBuild)?;

        Ok(Self {
            http,
            base_url: Url::parse(DEFAULT_BASE_URL)?,
            auth,
            language: DEFAULT_LANGUAGE.to_owned(),
            country: DEFAULT_COUNTRY.to_owned(),
            retry: RetryConfig::default(),
        })
    }

    /// Point the client at a different origin. Only useful for tests.
    ///
    /// # Errors
    ///
    /// Returns [`TmdbError::BadUrl`] when `base` is not a valid URL.
    pub fn with_base_url(mut self, base: &str) -> Result<Self, TmdbError> {
        // A trailing slash matters: `Url::join` replaces the last
        // segment without one.
        let normalised = if base.ends_with('/') {
            base.to_owned()
        } else {
            format!("{base}/")
        };
        self.base_url = Url::parse(&normalised)?;
        Ok(self)
    }

    /// Override the metadata language (default `pt-BR`).
    #[must_use]
    pub fn with_language(mut self, language: &str) -> Self {
        language.clone_into(&mut self.language);
        self
    }

    /// Override the country used for release-date lookups (default `BR`).
    #[must_use]
    pub fn with_country(mut self, country: &str) -> Self {
        country.clone_into(&mut self.country);
        self
    }

    /// Replace the retry policy.
    #[must_use]
    pub const fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    /// Country used for release-date resolution.
    #[must_use]
    pub fn country(&self) -> &str {
        &self.country
    }

    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, TmdbError> {
        let url = self.base_url.join(path)?;
        let mut req = self
            .http
            .get(url)
            .query(&[("language", self.language.as_str())])
            .query(query);
        if let Auth::ApiKey(key) = &self.auth {
            req = req.query(&[("api_key", key.as_str())]);
        }
        let resp = req.send().await?;

        match resp.status() {
            StatusCode::UNAUTHORIZED => return Err(TmdbError::Unauthorized),
            StatusCode::NOT_FOUND => return Err(TmdbError::NotFound(path.to_owned())),
            StatusCode::TOO_MANY_REQUESTS => return Err(TmdbError::RateLimited),
            _ => {}
        }
        let resp = resp.error_for_status()?;

        // Read as text first so a shape mismatch reports as BadJson with
        // the serde message rather than a bare reqwest decode error.
        let body = resp.text().await?;
        serde_json::from_str(&body).map_err(TmdbError::BadJson)
    }

    /// Search movies by free text, optionally pinned to a release year.
    ///
    /// # Errors
    ///
    /// See [`TmdbError`].
    pub async fn search_movies(
        &self,
        query: &str,
        year: Option<i32>,
    ) -> Result<Vec<MovieSummary>, TmdbError> {
        let mut params = vec![("query", query.to_owned())];
        if let Some(y) = year {
            params.push(("primary_release_year", y.to_string()));
        }
        let page: dto::PageDto<dto::MovieSummaryDto> =
            run_with_retry(self.retry, "search_movies", || {
                self.get("search/movie", &params)
            })
            .await?;
        Ok(page
            .results
            .into_iter()
            .map(MovieSummary::from_dto)
            .collect())
    }

    /// Search series by free text, optionally pinned to a first-air year.
    ///
    /// # Errors
    ///
    /// See [`TmdbError`].
    pub async fn search_tv(
        &self,
        query: &str,
        year: Option<i32>,
    ) -> Result<Vec<TvSummary>, TmdbError> {
        let mut params = vec![("query", query.to_owned())];
        if let Some(y) = year {
            params.push(("first_air_date_year", y.to_string()));
        }
        let page: dto::PageDto<dto::TvSummaryDto> =
            run_with_retry(self.retry, "search_tv", || self.get("search/tv", &params)).await?;
        Ok(page.results.into_iter().map(TvSummary::from_dto).collect())
    }

    /// Resolve an external id to TMDB records. This is what closes the
    /// loop with the axes brarr already carries — a search submitted by
    /// `IMDb` or TVDB id lands on a catalogue entry without a text match.
    ///
    /// `external_source` is TMDB's own tag: `imdb_id`, `tvdb_id`, …
    ///
    /// # Errors
    ///
    /// See [`TmdbError`].
    pub async fn find_by_external_id(
        &self,
        external_id: &str,
        external_source: &str,
    ) -> Result<FindResults, TmdbError> {
        let path = format!("find/{external_id}");
        let params = vec![("external_source", external_source.to_owned())];
        let found: dto::FindDto = run_with_retry(self.retry, "find_by_external_id", || {
            self.get(&path, &params)
        })
        .await?;
        Ok(FindResults {
            movies: found
                .movie_results
                .into_iter()
                .map(MovieSummary::from_dto)
                .collect(),
            series: found
                .tv_results
                .into_iter()
                .map(TvSummary::from_dto)
                .collect(),
        })
    }

    /// Resolve an `IMDb` id (`ttNNNNNNN`).
    ///
    /// # Errors
    ///
    /// See [`TmdbError`].
    pub async fn find_by_imdb(&self, imdb_id: &str) -> Result<FindResults, TmdbError> {
        self.find_by_external_id(imdb_id, "imdb_id").await
    }

    /// Resolve a TVDB id. Series only — TMDB has no tvdb mapping for
    /// movies.
    ///
    /// # Errors
    ///
    /// See [`TmdbError`].
    pub async fn find_by_tvdb(&self, tvdb_id: i64) -> Result<FindResults, TmdbError> {
        self.find_by_external_id(&tvdb_id.to_string(), "tvdb_id")
            .await
    }

    /// Full movie record, with external ids, release dates and
    /// translations folded in.
    ///
    /// # Errors
    ///
    /// See [`TmdbError`].
    pub async fn movie(&self, tmdb_id: i64) -> Result<MovieDetails, TmdbError> {
        let path = format!("movie/{tmdb_id}");
        let params = vec![("append_to_response", MOVIE_APPEND.to_owned())];
        let details: dto::MovieDetailsDto =
            run_with_retry(self.retry, "movie", || self.get(&path, &params)).await?;
        Ok(MovieDetails::from_dto(details, &self.country))
    }

    /// Full series record, including the season list.
    ///
    /// # Errors
    ///
    /// See [`TmdbError`].
    pub async fn tv(&self, tmdb_id: i64) -> Result<TvDetails, TmdbError> {
        let path = format!("tv/{tmdb_id}");
        let params = vec![("append_to_response", TV_APPEND.to_owned())];
        let details: dto::TvDetailsDto =
            run_with_retry(self.retry, "tv", || self.get(&path, &params)).await?;
        Ok(TvDetails::from_dto(details))
    }

    /// One season with its episodes.
    ///
    /// # Errors
    ///
    /// See [`TmdbError`].
    pub async fn season(
        &self,
        tmdb_id: i64,
        season_number: i32,
    ) -> Result<SeasonDetails, TmdbError> {
        let path = format!("tv/{tmdb_id}/season/{season_number}");
        let details: dto::SeasonDetailsDto =
            run_with_retry(self.retry, "season", || self.get(&path, &[])).await?;
        Ok(SeasonDetails::from_dto(details))
    }

    /// Cheap round-trip that proves the token works. Backs the "testar
    /// conexão" button on the settings page.
    ///
    /// # Errors
    ///
    /// [`TmdbError::Unauthorized`] when the token is wrong; see
    /// [`TmdbError`] for the rest.
    pub async fn verify_token(&self) -> Result<(), TmdbError> {
        // `/configuration` is the lightest authenticated endpoint and is
        // not rate-limited any differently from the rest.
        let _: serde_json::Value = self.get("configuration", &[]).await?;
        Ok(())
    }
}
