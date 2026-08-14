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

        if let Some(token) = value(settings::KEY_TMDB_TOKEN) {
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
                Ok(client) => built.push(Arc::new(client)),
                Err(e) => warn_unbuildable(MetadataSource::Tvdb, &e.to_string()),
            }
        }

        Ok(Self { built })
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
        settings::set(&pool, settings::KEY_TMDB_TOKEN, "   ")
            .await
            .unwrap();
        let registry = Registry::build(&pool).await.unwrap();
        assert_eq!(registry.configured().count(), 0);
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
