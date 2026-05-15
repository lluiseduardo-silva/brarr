//! [`TrackerSource`] — identifica o tracker que produziu um release.

use url::Url;

/// Identificação do tracker de origem de um [`Release`](crate::Release).
///
/// Não modela credenciais ou config (isso fica no nível da aplicação);
/// só o nome (display) e o `base_url` da API, que são suficientes para
/// distinguir releases, gerar links de detalhes/download corretos, e
/// servir como chave em mapas de configuração.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct TrackerSource {
    /// Nome curto para display (e.g., `"capybara"`, `"locadora"`).
    /// Garantido não-vazio após construção via [`TrackerSource::new`].
    pub name: String,
    /// URL base da API do tracker (e.g., `https://capybarabr.com/api`).
    pub base_url: Url,
}

/// Erros de construção de [`TrackerSource`].
#[derive(Debug, thiserror::Error)]
pub enum TrackerSourceError {
    /// Nome vazio (incluindo só whitespace).
    #[error("tracker name cannot be empty")]
    EmptyName,
}

impl TrackerSource {
    /// Constrói um [`TrackerSource`], rejeitando nome vazio.
    ///
    /// # Errors
    ///
    /// [`TrackerSourceError::EmptyName`] se `name` (após `trim`) for vazio.
    pub fn new(name: impl Into<String>, base_url: Url) -> Result<Self, TrackerSourceError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(TrackerSourceError::EmptyName);
        }
        Ok(Self { name, base_url })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{TrackerSource, TrackerSourceError};
    use url::Url;

    fn url() -> Url {
        Url::parse("https://example.com/api").expect("valid url literal")
    }

    #[test]
    fn new_accepts_non_empty_name() {
        let t = TrackerSource::new("capybara", url()).expect("valid");
        assert_eq!(t.name, "capybara");
        assert_eq!(t.base_url, url());
    }

    #[test]
    fn new_rejects_empty_name() {
        assert!(matches!(
            TrackerSource::new("", url()),
            Err(TrackerSourceError::EmptyName),
        ));
    }

    #[test]
    fn new_rejects_whitespace_only_name() {
        assert!(matches!(
            TrackerSource::new("   ", url()),
            Err(TrackerSourceError::EmptyName),
        ));
        assert!(matches!(
            TrackerSource::new("\t\n  ", url()),
            Err(TrackerSourceError::EmptyName),
        ));
    }
}
