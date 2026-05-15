//! Scoring hardcoded de releases, com pesos configuráveis (`ScoringWeights`)
//! que podem virar TOML-dirigidos numa fase futura.
//!
//! Filosofia: parar **bem simples** — não tentar replicar o que o
//! `brarr-decision-service` (Fase 6+) vai fazer. O objetivo aqui é só
//! ranquear releases por presença de PT-BR + qualidade básica
//! (resolução, HDR, seeders).

use brarr_core::{DecisionScore, Language, Release, Resolution};

/// Pesos aplicáveis por sinal. Constantes por enquanto; um dia podem
/// vir do arquivo de configuração para o usuário ajustar.
#[derive(Debug, Clone, Copy)]
pub struct ScoringWeights {
    /// Áudio PT-BR explícito.
    pub audio_pt_br: u32,
    /// Áudio PT-PT (Portugal) — vale menos que PT-BR para um público brasileiro.
    pub audio_pt_pt: u32,
    /// Áudio rotulado só como "Portuguese" sem hint regional — meio caminho.
    pub audio_pt_ambiguous: u32,
    /// Legenda PT-BR não-forced (legenda completa, idioma todo).
    pub sub_pt_br_full: u32,
    /// Apenas legenda PT-BR forced (só nomes/sinais).
    pub sub_pt_br_forced: u32,
    /// Legenda PT-PT.
    pub sub_pt_pt: u32,
    /// Bônus por HDR (vídeo).
    pub hdr_bonus: u32,
    /// Bônus por resolução 2160p.
    pub p2160_bonus: u32,
    /// Bônus por resolução 1080p.
    pub p1080_bonus: u32,
    /// Multiplicador de seeders (cada seeder ganha esse peso, cap em 50 seeders).
    pub seeder_weight: u32,
    /// Cap de seeders considerados (evita inflar com swarm gigante).
    pub seeder_cap: u32,
}

impl Default for ScoringWeights {
    fn default() -> Self {
        Self {
            audio_pt_br: 100,
            audio_pt_pt: 25,
            audio_pt_ambiguous: 50,
            sub_pt_br_full: 50,
            sub_pt_br_forced: 25,
            sub_pt_pt: 15,
            hdr_bonus: 10,
            p2160_bonus: 20,
            p1080_bonus: 10,
            seeder_weight: 1,
            seeder_cap: 50,
        }
    }
}

/// Pontua um release segundo os [`ScoringWeights`] dados.
///
/// Releases sem `enrichment` (parser de `MediaInfo` não rodou ou tracker
/// não forneceu o dump) ainda recebem pontos por resolução e seeders.
#[must_use]
pub fn score_release(release: &Release, w: &ScoringWeights) -> DecisionScore {
    let mut total: u32 = 0;

    if let Some(e) = &release.enrichment {
        if e.has_audio_in(&Language::PtBr) {
            total = total.saturating_add(w.audio_pt_br);
        }
        if e.has_audio_in(&Language::PtPt) {
            total = total.saturating_add(w.audio_pt_pt);
        }
        if e.has_audio_in(&Language::Pt) {
            total = total.saturating_add(w.audio_pt_ambiguous);
        }

        // Para subs PT-BR, distinguir forced-only de full: se houver
        // legenda PT-BR e nem todas elas forem `forced`, considera
        // "full" presente. Como a flag `has_forced_subs` é global,
        // a heurística aqui é: se há PT-BR em subtitles e a quantidade
        // for >1 OR has_forced_subs=false, assume full.
        let pt_br_subs = e.subtitle_count_in(&Language::PtBr);
        if pt_br_subs > 0 {
            if pt_br_subs > 1 || !e.has_forced_subs {
                total = total.saturating_add(w.sub_pt_br_full);
            } else {
                total = total.saturating_add(w.sub_pt_br_forced);
            }
        }

        if e.subtitle_count_in(&Language::PtPt) > 0 {
            total = total.saturating_add(w.sub_pt_pt);
        }

        if e.has_hdr {
            total = total.saturating_add(w.hdr_bonus);
        }
    }

    total = match release.resolution {
        Resolution::P2160 => total.saturating_add(w.p2160_bonus),
        Resolution::P1080 => total.saturating_add(w.p1080_bonus),
        _ => total,
    };

    let seeders_counted = release.seeders.min(w.seeder_cap);
    total = total.saturating_add(seeders_counted.saturating_mul(w.seeder_weight));

    DecisionScore::saturating(total)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{ScoringWeights, score_release};
    use brarr_core::{
        Language, Release, ReleaseEnrichment, ReleaseKind, Resolution, TrackerSource,
    };
    use url::Url;

    fn dummy_tracker() -> TrackerSource {
        TrackerSource::new("t", Url::parse("https://e.com/").unwrap()).unwrap()
    }

    fn release_with_enrichment(
        resolution: Resolution,
        seeders: u32,
        enrichment: Option<ReleaseEnrichment>,
    ) -> Release {
        let mut r =
            Release::new("1", dummy_tracker(), "x", ReleaseKind::WebDl, resolution, 0).unwrap();
        r.seeders = seeders;
        r.enrichment = enrichment;
        r
    }

    fn enrichment(
        audio: Vec<Language>,
        subs: Vec<Language>,
        forced_subs: bool,
        hdr: bool,
    ) -> ReleaseEnrichment {
        ReleaseEnrichment {
            container_format: None,
            duration: None,
            audio_languages: audio,
            subtitle_languages: subs,
            has_forced_subs: forced_subs,
            has_hdr: hdr,
        }
    }

    #[test]
    fn pt_br_audio_dominates_score() {
        let w = ScoringWeights::default();
        let with_pt_br = release_with_enrichment(
            Resolution::P1080,
            0,
            Some(enrichment(vec![Language::PtBr], vec![], false, false)),
        );
        let no_pt = release_with_enrichment(
            Resolution::P1080,
            0,
            Some(enrichment(vec![Language::En], vec![], false, false)),
        );
        assert!(score_release(&with_pt_br, &w) > score_release(&no_pt, &w));
        assert_eq!(score_release(&with_pt_br, &w).get(), 110); // 100 audio + 10 1080p
    }

    #[test]
    fn full_pt_br_sub_outweighs_forced_only() {
        let w = ScoringWeights::default();
        let two_pt_br_subs = release_with_enrichment(
            Resolution::P1080,
            0,
            Some(enrichment(
                vec![],
                vec![Language::PtBr, Language::PtBr],
                true,
                false,
            )),
        );
        let only_forced = release_with_enrichment(
            Resolution::P1080,
            0,
            Some(enrichment(vec![], vec![Language::PtBr], true, false)),
        );
        let two = score_release(&two_pt_br_subs, &w).get();
        let one = score_release(&only_forced, &w).get();
        assert!(two > one, "full ({two}) should beat forced ({one})");
    }

    #[test]
    fn hdr_2160p_release_scores_higher_than_sdr_1080p() {
        let w = ScoringWeights::default();
        let uhd_hdr = release_with_enrichment(
            Resolution::P2160,
            10,
            Some(enrichment(vec![Language::PtBr], vec![], false, true)),
        );
        let hd_sdr = release_with_enrichment(
            Resolution::P1080,
            10,
            Some(enrichment(vec![Language::PtBr], vec![], false, false)),
        );
        assert!(score_release(&uhd_hdr, &w) > score_release(&hd_sdr, &w));
    }

    #[test]
    fn seeders_contribute_but_are_capped() {
        let w = ScoringWeights::default();
        let many = release_with_enrichment(Resolution::P1080, 1_000_000, None);
        let some = release_with_enrichment(Resolution::P1080, 50, None);
        // Same score because we cap at 50.
        assert_eq!(score_release(&many, &w), score_release(&some, &w));
    }

    #[test]
    fn release_without_enrichment_still_scores_resolution_and_seeders() {
        let w = ScoringWeights::default();
        let bare = release_with_enrichment(Resolution::P2160, 5, None);
        let score = score_release(&bare, &w).get();
        assert_eq!(score, w.p2160_bonus + 5); // 20 + 5
    }

    #[test]
    fn pt_ambiguous_audio_partial_credit() {
        let w = ScoringWeights::default();
        let pt_only = release_with_enrichment(
            Resolution::P1080,
            0,
            Some(enrichment(vec![Language::Pt], vec![], false, false)),
        );
        assert_eq!(
            score_release(&pt_only, &w).get(),
            w.audio_pt_ambiguous + w.p1080_bonus,
        );
    }

    #[test]
    fn score_never_exceeds_max() {
        let w = ScoringWeights {
            audio_pt_br: 10_000,
            ..ScoringWeights::default()
        };
        let r = release_with_enrichment(
            Resolution::P2160,
            10_000,
            Some(enrichment(vec![Language::PtBr], vec![], false, true)),
        );
        let s = score_release(&r, &w);
        assert_eq!(s.get(), brarr_core::DecisionScore::MAX); // saturated
    }
}
