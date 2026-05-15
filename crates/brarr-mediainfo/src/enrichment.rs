//! Conversão de [`ParsedMediaInfo`] para [`brarr_core::ReleaseEnrichment`].
//!
//! Implementada como método inerente em `ParsedMediaInfo` (e não como
//! `From`/`Into`) por causa da regra órfã do Rust: `ReleaseEnrichment`
//! mora em `brarr-core`, então `impl From<ParsedMediaInfo> for ReleaseEnrichment`
//! lá daria pra fazer, mas exigiria que `brarr-core` dependesse de
//! `brarr-mediainfo` — ciclo de deps. Método inerente no tipo local
//! (`ParsedMediaInfo`) resolve sem ciclo e sem trait nova.

use brarr_core::ReleaseEnrichment;

use crate::types::ParsedMediaInfo;

impl ParsedMediaInfo {
    /// Destila o `ParsedMediaInfo` no formato consumido por
    /// `brarr-decision-service` e pelo `brarr-cli`.
    ///
    /// Preserva: container, duração, lista de idiomas de áudio (com
    /// repetições, para refletir múltiplas dublagens), lista de
    /// idiomas de legenda, e flags derivadas
    /// (`has_forced_subs`, `has_hdr`).
    #[must_use]
    pub fn to_enrichment(&self) -> ReleaseEnrichment {
        ReleaseEnrichment {
            container_format: self.general.container_format.clone(),
            duration: self.general.duration,
            audio_languages: self.audio.iter().map(|t| t.language.clone()).collect(),
            subtitle_languages: self.subtitles.iter().map(|t| t.language.clone()).collect(),
            has_forced_subs: self.subtitles.iter().any(|s| s.forced),
            has_hdr: self.video.iter().any(|v| v.hdr_format.is_some()),
        }
    }
}
