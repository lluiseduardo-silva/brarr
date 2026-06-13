//! [`ReleaseEnrichment`] — visão simplificada do conteúdo de mídia de
//! um release, derivada do parser de `MediaInfo` em `brarr-mediainfo`.
//!
//! Vive aqui (e não no crate de parser) porque é um tipo de domínio
//! consumido pelo decision-service, pela UI e pelo CLI. O parser
//! popula instâncias deste struct via método inerente
//! `ParsedMediaInfo::to_enrichment()` (definido em `brarr-mediainfo`,
//! que depende deste crate).

use std::time::Duration;

use crate::language::Language;

/// Resumo do conteúdo de mídia de um release.
///
/// Cobre o mínimo que regras de scoring precisam: presença de áudio e
/// legendas em cada idioma, container, duração, e flags de HDR e
/// legendas forçadas. Detalhes mais finos (codecs, bitrates, canais
/// por faixa) ficam no `ParsedMediaInfo` original — esse aqui é o
/// "destilado".
// Sem `#[non_exhaustive]`: este struct é populado de forma centralizada
// pelo `brarr-mediainfo::ParsedMediaInfo::to_enrichment()`, que precisa
// poder construir via literal cross-crate. Adicionar campos no futuro
// implica atualizar `to_enrichment` no mesmo PR.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReleaseEnrichment {
    /// Container do arquivo (e.g., `Matroska`). `None` se ausente no dump.
    pub container_format: Option<String>,
    /// Duração total reportada. `None` se ausente ou inválida.
    pub duration: Option<Duration>,
    /// Idiomas das faixas de áudio, na ordem em que aparecem no dump.
    /// Repetições são preservadas (duas dublagens PT-BR aparecem duas vezes).
    pub audio_languages: Vec<Language>,
    /// Idiomas das faixas de legenda, na ordem em que aparecem.
    pub subtitle_languages: Vec<Language>,
    /// `true` se houver pelo menos uma legenda marcada como `Forced: Yes`.
    pub has_forced_subs: bool,
    /// `true` se houver pelo menos uma faixa de vídeo com `HDR format` declarado.
    pub has_hdr: bool,
    /// Formato/codec da primeira faixa de vídeo (e.g. `"HEVC"`, `"AVC"`,
    /// `"AV1"`). `None` quando ausente no dump. Mais confiável que o
    /// codec adivinhado do título — os converters o preferem.
    pub video_codec: Option<String>,
    /// Profundidade de bits da primeira faixa de vídeo (8/10/12). `None`
    /// quando ausente.
    pub video_bit_depth: Option<u8>,
}

impl ReleaseEnrichment {
    /// Indica se há ao menos uma faixa de áudio no idioma dado.
    #[must_use]
    pub fn has_audio_in(&self, language: &Language) -> bool {
        self.audio_languages.contains(language)
    }

    /// Indica se há ao menos uma faixa de legenda no idioma dado.
    #[must_use]
    pub fn has_subtitle_in(&self, language: &Language) -> bool {
        self.subtitle_languages.contains(language)
    }

    /// Quantas faixas de áudio no idioma dado.
    #[must_use]
    pub fn audio_count_in(&self, language: &Language) -> usize {
        self.audio_languages
            .iter()
            .filter(|l| *l == language)
            .count()
    }

    /// Quantas faixas de legenda no idioma dado.
    #[must_use]
    pub fn subtitle_count_in(&self, language: &Language) -> usize {
        self.subtitle_languages
            .iter()
            .filter(|l| *l == language)
            .count()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::ReleaseEnrichment;
    use crate::language::Language;

    fn enrichment_with(audio: Vec<Language>, subs: Vec<Language>) -> ReleaseEnrichment {
        ReleaseEnrichment {
            audio_languages: audio,
            subtitle_languages: subs,
            ..ReleaseEnrichment::default()
        }
    }

    #[test]
    fn has_audio_in_matches_present_language() {
        let e = enrichment_with(vec![Language::PtBr, Language::En], vec![]);
        assert!(e.has_audio_in(&Language::PtBr));
        assert!(e.has_audio_in(&Language::En));
        assert!(!e.has_audio_in(&Language::PtPt));
    }

    #[test]
    fn audio_count_counts_duplicates() {
        let e = enrichment_with(vec![Language::PtBr, Language::PtBr, Language::En], vec![]);
        assert_eq!(e.audio_count_in(&Language::PtBr), 2);
        assert_eq!(e.audio_count_in(&Language::En), 1);
        assert_eq!(e.audio_count_in(&Language::PtPt), 0);
    }

    #[test]
    fn subtitle_count_distinguishes_pt_variants() {
        let e = enrichment_with(
            vec![],
            vec![Language::PtBr, Language::PtBr, Language::PtPt, Language::En],
        );
        assert_eq!(e.subtitle_count_in(&Language::PtBr), 2);
        assert_eq!(e.subtitle_count_in(&Language::PtPt), 1);
        assert_eq!(e.subtitle_count_in(&Language::En), 1);
        assert_eq!(e.subtitle_count_in(&Language::Pt), 0);
    }

    #[test]
    fn default_is_empty_and_flags_false() {
        let e = ReleaseEnrichment::default();
        assert!(e.audio_languages.is_empty());
        assert!(e.subtitle_languages.is_empty());
        assert!(!e.has_forced_subs);
        assert!(!e.has_hdr);
        assert!(e.container_format.is_none());
        assert!(e.duration.is_none());
    }
}
