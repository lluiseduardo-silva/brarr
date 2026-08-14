//! Which providers are configured, and what each may be asked.
//!
//! Two things live here that could not live in `brarr-core`: reading a
//! credential out of `settings`, and holding the built clients. The
//! trait and the vocabulary stay in the leaf crate; this is the part
//! that needs a pool.
//!
//! ## Capabilities are consulted before dispatch
//!
//! [`Registry::for_structure`] filters on
//! [`Capabilities`](brarr_core::Capabilities) rather than calling and
//! seeing what comes back. That is what keeps
//! [`MetadataError::Unsupported`] a bug report instead of a routine
//! answer — the failure the sibling `TrackerProvider` still has, where
//! `search_by_tvdb` defaults to `Ok(vec![])` so "I do not speak this
//! axis" and "I found nothing" are the same value and every WASM plugin
//! shows a healthy zero on `/health`.
//!
//! ## A missing credential is absence, not failure
//!
//! A provider with no key is simply not in the registry. Callers ask for
//! what they need and get `None`, which reads as "not configured" at
//! every call site — the contract every background task in brarr already
//! has, where an unconfigured prerequisite means the worker no-ops
//! rather than logging an error every cycle.

use std::sync::Arc;

use brarr_core::{
    Capabilities, CredentialField, MediaType, MetadataError, MetadataProvider, MetadataSource,
    SourceKind,
};
use brarr_tmdb::TmdbClient;
use brarr_tvdb::{TvdbAuth, TvdbClient};

use crate::AppError;
use crate::db::{Pool, settings};

/// The providers this deployment can actually reach.
#[derive(Clone)]
pub struct Registry {
    built: Vec<Arc<dyn MetadataProvider>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field(
                "configured",
                &self.built.iter().map(|p| p.source()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Registry {
    /// Build from the stored credentials.
    ///
    /// Reads `settings` once and constructs whatever it has keys for. A
    /// provider whose client fails to build — a broken TLS stack, a base
    /// URL that will not parse — is left out with a warning rather than
    /// failing the whole registry: one unusable provider must not take
    /// the others down.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Database`] when the settings cannot be read.
    pub async fn build(pool: &Pool) -> Result<Self, AppError> {
        let stored = settings::get_all(pool).await?;
        let value = |key: &str| -> Option<String> {
            stored
                .get(key)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };

        let mut built: Vec<Arc<dyn MetadataProvider>> = Vec::new();

        if let Some(token) = tmdb_token(value(settings::KEY_TMDB_TOKEN), env_tmdb_token()) {
            match TmdbClient::new(&token) {
                Ok(client) => {
                    let client = match value(settings::KEY_TMDB_LANGUAGE) {
                        Some(language) => client.with_language(&language),
                        None => client,
                    };
                    let client = match value(settings::KEY_TMDB_COUNTRY) {
                        Some(country) => client.with_country(&country),
                        None => client,
                    };
                    built.push(Arc::new(client));
                }
                Err(e) => warn_unbuildable(MetadataSource::Tmdb, &e.to_string()),
            }
        }

        if let Some(api_key) = value(settings::KEY_TVDB_API_KEY) {
            match TvdbClient::new(TvdbAuth {
                api_key,
                // Absent, not empty: the API documentation says to remove
                // `pin` entirely for a project key, and an empty string
                // is a refused login rather than the same thing.
                pin: value(settings::KEY_TVDB_PIN),
            }) {
                Ok(client) => built.push(Arc::new(client.with_languages(episode_languages(
                    value(settings::KEY_TMDB_LANGUAGE).as_deref(),
                )))),
                Err(e) => warn_unbuildable(MetadataSource::Tvdb, &e.to_string()),
            }
        }

        Ok(Self { built })
    }

    /// Build from an explicit list.
    ///
    /// [`Self::build`] is the only production door, because a registry is
    /// what the stored credentials say it is. But the decisions this type
    /// drives — who is asked for a new series' shape, in what order, and
    /// what a refusal means — are worth exercising against providers that
    /// answer on command rather than against somebody else's live API.
    #[cfg(test)]
    pub(crate) fn from_providers(built: Vec<Arc<dyn MetadataProvider>>) -> Self {
        Self { built }
    }

    /// The provider for one source, when it is configured.
    #[must_use]
    pub fn get(&self, source: MetadataSource) -> Option<&Arc<dyn MetadataProvider>> {
        self.built.iter().find(|p| p.source() == source)
    }

    /// Every configured source, in enum order.
    pub fn configured(&self) -> impl Iterator<Item = MetadataSource> + '_ {
        MetadataSource::all().filter(|s| self.get(*s).is_some())
    }

    /// The providers that can build a tree for this media kind.
    ///
    /// **The filter is the point.** Asking a provider for something it
    /// does not do and reading the empty answer as "nothing found" is the
    /// defect this whole abstraction is shaped against.
    pub fn for_structure(
        &self,
        media: MediaType,
    ) -> impl Iterator<Item = &Arc<dyn MetadataProvider>> + '_ {
        self.built
            .iter()
            .filter(move |p| p.capabilities().structure.covers(media))
    }

    /// The providers that can resolve an id for this media kind.
    pub fn for_identity(
        &self,
        media: MediaType,
    ) -> impl Iterator<Item = &Arc<dyn MetadataProvider>> + '_ {
        self.built
            .iter()
            .filter(move |p| p.capabilities().identity.covers(media))
    }

    /// Ask for a provider that must be there, saying which one is not.
    ///
    /// # Errors
    ///
    /// [`AppError::Metadata`] wrapping [`MetadataError::Unauthorized`],
    /// because "no key configured" and "the key was refused" are the same
    /// thing to the operator: a credential to go and fix.
    pub fn require(&self, source: MetadataSource) -> Result<&Arc<dyn MetadataProvider>, AppError> {
        self.get(source)
            .ok_or_else(|| AppError::Metadata(MetadataError::Unauthorized { origin: source }))
    }
}

/// Every credential field `/settings` should render, in source order.
///
/// Derived from the providers rather than written into the form, so a
/// source added without a field — or a field added without a source —
/// fails a guard instead of leaving a key nobody can enter.
///
/// Built from the *types* and not from the registry, because the screen
/// has to offer the field for a provider that is **not** configured yet.
/// That is the whole reason the operator is on that screen.
#[must_use]
pub fn credential_fields() -> Vec<(MetadataSource, &'static CredentialField)> {
    let mut out = Vec::new();
    for source in MetadataSource::all() {
        for field in credentials_of(source) {
            out.push((source, field));
        }
    }
    out
}

/// The fields one source declares.
///
/// A `match` rather than a registry lookup: the answer must not depend on
/// whether a key happens to be stored.
fn credentials_of(source: MetadataSource) -> &'static [CredentialField] {
    match source {
        // Constructed with a placeholder purely to read the constant off
        // the impl. `credentials` takes `&self` because the trait is
        // `dyn`-compatible, and a `dyn`-compatible trait cannot have an
        // associated constant.
        MetadataSource::Tmdb => TmdbClient::new("x")
            .as_ref()
            .map_or(&[], MetadataProvider::credentials),
        MetadataSource::Tvdb => TvdbClient::new(TvdbAuth {
            api_key: "x".to_owned(),
            pin: None,
        })
        .as_ref()
        .map_or(&[], MetadataProvider::credentials),
        // No client, no credential. brarr stores IMDb ids and never calls
        // IMDb, so there is nothing to configure.
        MetadataSource::Imdb => &[],
    }
}

/// The languages TheTVDB episode names are asked for, in order.
///
/// **English is always in the chain and always after the operator's
/// language.** Measured on this catalogue: Frieren has 0 of 66 episodes
/// in Portuguese and 65 of 66 in English, so a chain of one would leave
/// the whole series in Japanese; Doctor Who has 154 of 322 in
/// Portuguese, so dropping English would leave half of it untitled. The
/// original is not listed because it is not a preference — it is what
/// the untranslated request returns when both have nothing.
///
/// Derived from the metadata language already configured rather than
/// from a setting of its own: an operator who set `pt-BR` for TMDB has
/// said what language they read. TheTVDB speaks ISO 639-3, so the tag is
/// mapped; **an unmapped tag yields English alone**, which is a worse
/// answer than their language and a much better one than Japanese.
fn episode_languages(tag: Option<&str>) -> Vec<&'static str> {
    let preferred = tag
        .map(|t| t.trim().to_ascii_lowercase())
        .and_then(|t| tvdb_language(t.split(['-', '_']).next().unwrap_or_default()));
    match preferred {
        Some("eng") | None => vec!["eng"],
        Some(other) => vec![other, "eng"],
    }
}

/// An ISO 639-1 subtag as TheTVDB spells it.
///
/// Deliberately short: these are the languages a deployment of this app
/// plausibly reads, and a tag that is not here falls through to English
/// rather than being guessed at — a three-letter code invented from a
/// two-letter one is a 404 per series.
fn tvdb_language(subtag: &str) -> Option<&'static str> {
    match subtag {
        "pt" => Some("por"),
        "en" => Some("eng"),
        "es" => Some("spa"),
        "fr" => Some("fra"),
        "de" => Some("deu"),
        "it" => Some("ita"),
        "ja" => Some("jpn"),
        _ => None,
    }
}

/// Which TMDB credential wins.
///
/// `settings` replaced `BRARR_TMDB_TOKEN`, but it did not retire it —
/// `tmdb_sync::load_config` still falls back to it and `CLAUDE.md` still
/// documents it, so a deployment that sets the variable and never opens
/// `/settings` is a supported one. Reading it in only one of the two
/// places is worse than reading it in neither: the client would build and
/// the registry would be empty, so `/library/add` would work and every
/// tree fetch would fail naming a credential the operator can see is set.
///
/// Stored first, for the same reason every other hot-reloadable value in
/// brarr is: the screen is what the operator just used, and a variable
/// baked into a container at deploy time must not silently outrank it.
fn tmdb_token(stored: Option<String>, environment: Option<String>) -> Option<String> {
    stored.or(environment)
}

/// The legacy environment token, normalised the way a stored one is.
fn env_tmdb_token() -> Option<String> {
    std::env::var("BRARR_TMDB_TOKEN")
        .ok()
        .as_deref()
        .and_then(non_blank)
}

/// A value that is only whitespace is unset, not empty.
fn non_blank(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// What [`Registry::build`] does with a client it cannot construct.
fn warn_unbuildable(source: MetadataSource, detail: &str) {
    tracing::warn!(
        target: "brarr_orchestrator::metadata",
        %source, error = detail,
        "could not build the metadata client; leaving it out of the registry"
    );
}

/// Whether a source is one brarr ever calls.
///
/// Read by the settings screen, so an id namespace does not get a
/// "testar conexão" button for an API nobody talks to.
#[must_use]
pub const fn is_callable(source: MetadataSource) -> bool {
    matches!(source.kind(), SourceKind::Provider)
}

/// What a provider offers, without building one.
#[must_use]
pub fn capabilities_of(source: MetadataSource) -> Option<Capabilities> {
    match source {
        MetadataSource::Tmdb => TmdbClient::new("x")
            .as_ref()
            .ok()
            .map(MetadataProvider::capabilities),
        MetadataSource::Tvdb => TvdbClient::new(TvdbAuth {
            api_key: "x".to_owned(),
            pin: None,
        })
        .as_ref()
        .ok()
        .map(MetadataProvider::capabilities),
        MetadataSource::Imdb => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use crate::db::open_memory;

    #[tokio::test]
    async fn an_unconfigured_deployment_has_an_empty_registry() {
        let pool = open_memory().await.unwrap();
        // Guard against a token leaking in from the developer's own env,
        // the same way `client_refuses_to_build_without_a_token` does.
        if env_tmdb_token().is_some() {
            return;
        }
        let registry = Registry::build(&pool).await.unwrap();
        assert_eq!(registry.configured().count(), 0);
        assert!(registry.get(MetadataSource::Tmdb).is_none());
        // And asking for one says which credential is missing, rather
        // than producing a client that fails on first use.
        assert!(matches!(
            registry.require(MetadataSource::Tvdb),
            Err(AppError::Metadata(MetadataError::Unauthorized {
                origin: MetadataSource::Tvdb
            }))
        ));
    }

    #[tokio::test]
    async fn a_stored_credential_builds_its_provider() {
        let pool = open_memory().await.unwrap();
        if env_tmdb_token().is_some() {
            return;
        }
        settings::set(&pool, settings::KEY_TVDB_API_KEY, "a-key")
            .await
            .unwrap();
        let registry = Registry::build(&pool).await.unwrap();

        assert_eq!(
            registry.configured().collect::<Vec<_>>(),
            vec![MetadataSource::Tvdb]
        );
        assert!(registry.require(MetadataSource::Tvdb).is_ok());
    }

    /// A blank setting is "unset", the contract every other
    /// hot-reloadable value in brarr has. Storing an empty key and
    /// getting a client that fails on first use would be worse than not
    /// having one.
    #[tokio::test]
    async fn a_blank_credential_configures_nothing() {
        let pool = open_memory().await.unwrap();
        if env_tmdb_token().is_some() {
            return;
        }
        settings::set(&pool, settings::KEY_TMDB_TOKEN, "   ")
            .await
            .unwrap();
        let registry = Registry::build(&pool).await.unwrap();
        assert_eq!(registry.configured().count(), 0);
    }

    /// **The chain is the operator's language, then English, never the
    /// original.** A chain of one leaves Frieren in Japanese (0 of 66
    /// translated into Portuguese); dropping English leaves half of
    /// Doctor Who untitled.
    #[test]
    fn the_episode_language_chain_always_ends_in_english() {
        assert_eq!(episode_languages(Some("pt-BR")), vec!["por", "eng"]);
        assert_eq!(episode_languages(Some("pt")), vec!["por", "eng"]);
        // Already English: asking twice would be a wasted request.
        assert_eq!(episode_languages(Some("en-US")), vec!["eng"]);
        // Unset, or a tag TheTVDB's three-letter codes do not cover.
        assert_eq!(episode_languages(None), vec!["eng"]);
        assert_eq!(episode_languages(Some("tlh")), vec!["eng"]);
    }

    /// **The two doors read the same credential.** `settings` replaced
    /// `BRARR_TMDB_TOKEN` without retiring it, and honouring it in the
    /// client but not in the registry is the worst of the three states:
    /// adding a title works and building its tree fails, naming a
    /// credential the operator can see is set.
    ///
    /// Asserted on the resolution rather than by setting the variable:
    /// the crate is `#![forbid(unsafe_code)]` and `std::env::set_var` is
    /// unsafe as of the 2024 edition, so a test that wrote the
    /// environment could not compile here.
    #[test]
    fn the_environment_token_is_a_fallback_and_the_stored_one_wins() {
        assert_eq!(
            tmdb_token(None, Some("from-env".to_owned())).as_deref(),
            Some("from-env")
        );
        assert_eq!(
            tmdb_token(Some("stored".to_owned()), Some("from-env".to_owned())).as_deref(),
            Some("stored"),
            "a variable baked in at deploy time must not outrank the screen"
        );
        assert_eq!(tmdb_token(None, None), None);
        // And whitespace is unset, the contract every other value has.
        assert_eq!(non_blank("   "), None);
        assert_eq!(non_blank(" k ").as_deref(), Some("k"));
    }

    /// **The filter that keeps `Unsupported` a bug report.** A film has
    /// no tree, so no provider is offered for one.
    #[tokio::test]
    async fn no_provider_is_offered_a_structure_it_cannot_build() {
        let pool = open_memory().await.unwrap();
        settings::set(&pool, settings::KEY_TMDB_TOKEN, "t")
            .await
            .unwrap();
        settings::set(&pool, settings::KEY_TVDB_API_KEY, "k")
            .await
            .unwrap();
        let registry = Registry::build(&pool).await.unwrap();

        assert_eq!(registry.for_structure(MediaType::Movie).count(), 0);
        assert_eq!(registry.for_structure(MediaType::Tv).count(), 2);
        // TMDB resolves ids for both kinds; TheTVDB, as this client reads
        // it, only for series.
        assert_eq!(registry.for_identity(MediaType::Movie).count(), 1);
        assert_eq!(registry.for_identity(MediaType::Tv).count(), 2);
    }

    /// Every provider declares coherent capabilities — a claim to build a
    /// tree for a film is one nothing can honour.
    #[test]
    fn every_provider_declares_something_it_could_meet() {
        for source in MetadataSource::all() {
            let Some(caps) = capabilities_of(source) else {
                assert!(
                    !is_callable(source),
                    "{source} is callable but declares no capabilities"
                );
                continue;
            };
            assert!(caps.is_coherent(), "{source} claims a tree for a film");
        }
    }

    /// **The guard for the screen.** Every callable source declares at
    /// least one credential field, and every field has a settings key
    /// that is not blank — a provider whose key nobody can enter is
    /// configured by editing the database.
    #[test]
    fn every_callable_source_declares_a_credential() {
        let fields = credential_fields();
        for source in MetadataSource::all().filter(|s| is_callable(*s)) {
            let declared: Vec<_> = fields.iter().filter(|(s, _)| *s == source).collect();
            assert!(
                !declared.is_empty(),
                "{source} declares no credential field"
            );
            for (_, field) in declared {
                assert!(!field.key.is_empty(), "{source} has a nameless field");
                assert!(!field.label.is_empty(), "{} has no label", field.key);
            }
        }
        // And a namespace declares none, because there is nothing to call.
        assert!(
            fields.iter().all(|(s, _)| is_callable(*s)),
            "a non-callable source asked for a credential"
        );
    }

    /// The declared keys are the ones `settings` actually stores. A field
    /// naming a key nothing reads is a form that silently does nothing.
    #[test]
    fn every_declared_key_is_a_settings_key() {
        let known = [
            settings::KEY_TMDB_TOKEN,
            settings::KEY_TVDB_API_KEY,
            settings::KEY_TVDB_PIN,
        ];
        for (source, field) in credential_fields() {
            assert!(
                known.contains(&field.key),
                "{source} declares `{}`, which no settings key matches",
                field.key
            );
        }
    }
}
