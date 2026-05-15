//! Idiomas reconhecidos e normalização específica para Português
//! brasileiro vs. europeu — o ponto chave do brarr.
//!
//! Esse tipo vive em `brarr-core` (e não em `brarr-mediainfo`) porque
//! todos os crates de aplicação consomem ele: o parser de `MediaInfo`
//! popula a partir do par `(Language, Title)`, o cliente de tracker
//! desserializa direto de JSON, o decision-service pontua releases por
//! presença de PT-BR, e o CLI/orchestrator exibe para o usuário.

/// Idioma de uma faixa de áudio ou legenda, normalizado.
///
/// Variantes nomeadas para os casos que o brarr precisa pontuar
/// diretamente (PT-BR, PT-PT, inglês). Tudo o mais cai em
/// [`Language::Other`] preservando a string original do campo
/// `Language` — caso a feature precise, basta adicionar uma variante
/// nova aqui em vez de espalhar `match` em consumidores.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Language {
    /// Português brasileiro. Reconhecido a partir de:
    /// - `Language: Portuguese (BR)`
    /// - `Language: Portuguese` + `Title` contendo `Brazilian` ou `Brasileir`.
    PtBr,
    /// Português europeu. Reconhecido a partir de:
    /// - `Language: Portuguese (PT)`
    /// - `Language: Portuguese` + `Title` contendo `European`, `Europeu` ou `Portugal`.
    PtPt,
    /// Português sem indicação regional clara — `Language: Portuguese`
    /// sem título informativo. Preserva a ambiguidade para que regras
    /// de pontuação a jusante decidam o que fazer (e.g., considerar
    /// `Pt` como meio-PT-BR num release brasileiro).
    Pt,
    /// Inglês (`Language: English`).
    En,
    /// Qualquer idioma fora do conjunto acima. Preserva a string
    /// original do campo `Language` (e.g., `"Spanish (Latin America)"`,
    /// `"Catalan (ES)"`, `"Serbian-Latn-RS"`).
    Other(String),
}

impl Language {
    /// Normaliza o par `(language_field, title)` de uma faixa do `MediaInfo`.
    ///
    /// Ordem das regras (primeira que casa ganha):
    /// 1. `language_field` igual a `Portuguese (BR)` (case-insensitive) →
    ///    [`Language::PtBr`].
    /// 2. `language_field` igual a `Portuguese (PT)` (case-insensitive) →
    ///    [`Language::PtPt`].
    /// 3. `language_field` igual a `Portuguese` + `title` contém
    ///    `brazilian`/`brasileir` (case-insensitive) → [`Language::PtBr`].
    /// 4. `language_field` igual a `Portuguese` + `title` contém
    ///    `european`/`europeu`/`portugal` → [`Language::PtPt`].
    /// 5. `language_field` igual a `Portuguese` puro →
    ///    [`Language::Pt`].
    /// 6. `language_field` igual a `English` → [`Language::En`].
    /// 7. Caso contrário → [`Language::Other`] com a string original
    ///    `trim`ada.
    #[must_use]
    pub fn from_mediainfo(language_field: &str, title: Option<&str>) -> Self {
        let lang = language_field.trim();
        let title_lc = title.map(str::to_lowercase);

        if lang.eq_ignore_ascii_case("Portuguese (BR)") {
            return Self::PtBr;
        }
        if lang.eq_ignore_ascii_case("Portuguese (PT)") {
            return Self::PtPt;
        }
        if lang.eq_ignore_ascii_case("Portuguese") {
            if let Some(t) = title_lc.as_deref() {
                if t.contains("brazilian") || t.contains("brasileir") {
                    return Self::PtBr;
                }
                if t.contains("european") || t.contains("europeu") || t.contains("portugal") {
                    return Self::PtPt;
                }
            }
            return Self::Pt;
        }
        if lang.eq_ignore_ascii_case("English") {
            return Self::En;
        }
        Self::Other(lang.to_string())
    }

    /// Indica se este idioma é uma variante de Português (qualquer
    /// região — útil em regras de scoring que aceitam ambígua).
    #[must_use]
    pub const fn is_portuguese(&self) -> bool {
        matches!(self, Self::PtBr | Self::PtPt | Self::Pt)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::Language;

    #[test]
    fn pt_br_from_explicit_region_tag() {
        assert_eq!(
            Language::from_mediainfo("Portuguese (BR)", None),
            Language::PtBr
        );
        // case-insensitive
        assert_eq!(
            Language::from_mediainfo("portuguese (br)", None),
            Language::PtBr
        );
    }

    #[test]
    fn pt_pt_from_explicit_region_tag() {
        assert_eq!(
            Language::from_mediainfo("Portuguese (PT)", None),
            Language::PtPt
        );
    }

    #[test]
    fn pt_br_from_brazilian_title_hint() {
        // capybara/shadow style
        assert_eq!(
            Language::from_mediainfo("Portuguese", Some("Brazilian Portuguese")),
            Language::PtBr,
        );
        assert_eq!(
            Language::from_mediainfo("Portuguese", Some("Brazilian")),
            Language::PtBr,
        );
        assert_eq!(
            Language::from_mediainfo("Portuguese", Some("Português Brasileiro")),
            Language::PtBr,
        );
    }

    #[test]
    fn pt_pt_from_european_title_hint() {
        assert_eq!(
            Language::from_mediainfo("Portuguese", Some("European")),
            Language::PtPt,
        );
        assert_eq!(
            Language::from_mediainfo("Portuguese", Some("Portugal")),
            Language::PtPt,
        );
    }

    #[test]
    fn pt_unspecified_when_no_title_or_uninformative_title() {
        assert_eq!(Language::from_mediainfo("Portuguese", None), Language::Pt);
        // Title "Forced" is not a regional hint
        assert_eq!(
            Language::from_mediainfo("Portuguese", Some("Forced")),
            Language::Pt,
        );
    }

    #[test]
    fn english_recognized() {
        assert_eq!(Language::from_mediainfo("English", None), Language::En);
        assert_eq!(Language::from_mediainfo("english", None), Language::En);
    }

    #[test]
    fn other_preserves_original_string() {
        assert_eq!(
            Language::from_mediainfo("Spanish (Latin America)", None),
            Language::Other("Spanish (Latin America)".to_string()),
        );
        assert_eq!(
            Language::from_mediainfo("Catalan (ES)", None),
            Language::Other("Catalan (ES)".to_string()),
        );
        assert_eq!(
            Language::from_mediainfo("Serbian-Latn-RS", None),
            Language::Other("Serbian-Latn-RS".to_string()),
        );
    }

    #[test]
    fn whitespace_in_language_field_trimmed() {
        assert_eq!(
            Language::from_mediainfo("  Portuguese (BR)  ", None),
            Language::PtBr,
        );
    }

    #[test]
    fn is_portuguese_returns_true_for_all_pt_variants() {
        assert!(Language::PtBr.is_portuguese());
        assert!(Language::PtPt.is_portuguese());
        assert!(Language::Pt.is_portuguese());
        assert!(!Language::En.is_portuguese());
        assert!(!Language::Other("Spanish".to_string()).is_portuguese());
    }
}
