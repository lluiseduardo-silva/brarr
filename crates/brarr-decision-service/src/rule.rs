//! Tipos das regras declarativas: [`Rule`], [`Condition`], [`RuleSet`].
//!
//! Uma regra tem um **predicado** ([`Condition`]) que decide se ela se
//! aplica a um dado [`Release`], e um conjunto de **efeitos**
//! (`add_score`, `tag`, `reject`) aplicados quando o predicado casa.
//!
//! Predicado combina os campos opcionais com **AND** — todos os
//! `Some(_)` devem casar; `None` significa "não importa". Predicado
//! totalmente vazio casa sempre (regra default).

use brarr_core::{Language, Release, Resolution};
use serde::{Deserialize, Serialize};

/// Conjunto completo de regras avaliadas em ordem para cada release.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSet {
    /// Lista de regras na ordem de avaliação. Cada regra que casa
    /// contribui com seus efeitos; ordem importa apenas para tags
    /// (são preservadas) e `reject` (curto-circuita).
    #[serde(default, rename = "rule")]
    pub rules: Vec<Rule>,
}

impl RuleSet {
    /// `RuleSet` correspondente ao scoring hardcoded antigo de
    /// `brarr-cli` (`ScoringWeights::default`). Útil para clientes que
    /// não fornecem `rules.toml` próprio.
    #[must_use]
    pub fn baseline() -> Self {
        Self {
            rules: vec![
                Rule {
                    name: Some("PT-BR audio".into()),
                    when: Condition::audio(AudioFilter::PtBr),
                    add_score: 100,
                    tag: None,
                    reject: false,
                },
                Rule {
                    name: Some("PT-PT audio".into()),
                    when: Condition::audio(AudioFilter::PtPt),
                    add_score: 25,
                    tag: None,
                    reject: false,
                },
                Rule {
                    name: Some("PT ambíguo (sem hint regional)".into()),
                    when: Condition::audio(AudioFilter::Pt),
                    add_score: 50,
                    tag: None,
                    reject: false,
                },
                Rule {
                    name: Some("Legenda PT-BR".into()),
                    when: Condition::subtitle(SubtitleFilter::PtBr),
                    add_score: 50,
                    tag: None,
                    reject: false,
                },
                Rule {
                    name: Some("Legenda PT-PT".into()),
                    when: Condition::subtitle(SubtitleFilter::PtPt),
                    add_score: 15,
                    tag: None,
                    reject: false,
                },
                Rule {
                    name: Some("HDR".into()),
                    when: Condition::hdr(true),
                    add_score: 10,
                    tag: None,
                    reject: false,
                },
                Rule {
                    name: Some("Resolução 2160p".into()),
                    when: Condition::resolution(ResolutionFilter::Exact2160),
                    add_score: 20,
                    tag: None,
                    reject: false,
                },
                Rule {
                    name: Some("Resolução 1080p".into()),
                    when: Condition::resolution(ResolutionFilter::Exact1080),
                    add_score: 10,
                    tag: None,
                    reject: false,
                },
            ],
        }
    }
}

/// Uma regra declarativa.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Nome opcional para identificar a regra em logs e diagnóstico.
    #[serde(default)]
    pub name: Option<String>,
    /// Predicado: critérios que o release precisa atender para a regra
    /// disparar. Campos `None` (omitidos no TOML) significam "não filtrar
    /// por esse aspecto".
    #[serde(default)]
    pub when: Condition,
    /// Quantos pontos somar ao score quando a regra casa.
    #[serde(default)]
    pub add_score: u32,
    /// Tag opcional anexada ao release quando a regra casa.
    #[serde(default)]
    pub tag: Option<String>,
    /// Se `true`, desqualifica o release (filtro de exclusão).
    #[serde(default)]
    pub reject: bool,
}

/// Predicado: condições combinadas com **AND**.
///
/// Cada campo `Option<_>`:
/// - `None` (omitido) → não filtra por esse aspecto.
/// - `Some(spec)` → release precisa satisfazer.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Condition {
    /// Filtro de áudio (`pt-br`, `pt-pt`, `pt`, `pt-any`).
    #[serde(default)]
    pub audio: Option<AudioFilter>,
    /// Filtro de legenda (`pt-br`, `pt-pt`, `pt-any`).
    #[serde(default)]
    pub subtitle: Option<SubtitleFilter>,
    /// Release tem (ou não tem) HDR no vídeo.
    #[serde(default)]
    pub hdr: Option<bool>,
    /// Filtro de resolução.
    #[serde(default)]
    pub resolution: Option<ResolutionFilter>,
    /// Seeders mínimos (release precisa ter ≥ esse valor).
    #[serde(default)]
    pub min_seeders: Option<u32>,
    /// Tamanho máximo em bytes (release não pode exceder).
    #[serde(default)]
    pub max_size_bytes: Option<u64>,
    /// Nome de tracker exato (case-sensitive).
    #[serde(default)]
    pub tracker: Option<String>,
}

impl Condition {
    fn audio(a: AudioFilter) -> Self {
        Self {
            audio: Some(a),
            ..Self::default()
        }
    }
    fn subtitle(s: SubtitleFilter) -> Self {
        Self {
            subtitle: Some(s),
            ..Self::default()
        }
    }
    fn hdr(v: bool) -> Self {
        Self {
            hdr: Some(v),
            ..Self::default()
        }
    }
    fn resolution(r: ResolutionFilter) -> Self {
        Self {
            resolution: Some(r),
            ..Self::default()
        }
    }

    /// Decide se o `release` satisfaz **todos** os campos do predicado.
    #[must_use]
    pub fn matches(&self, release: &Release) -> bool {
        if let Some(a) = &self.audio {
            if !audio_matches(release, a) {
                return false;
            }
        }
        if let Some(s) = &self.subtitle {
            if !subtitle_matches(release, s) {
                return false;
            }
        }
        if let Some(h) = self.hdr {
            let has_hdr = release.enrichment.as_ref().is_some_and(|e| e.has_hdr);
            if has_hdr != h {
                return false;
            }
        }
        if let Some(r) = &self.resolution {
            if !resolution_matches(&release.resolution, r) {
                return false;
            }
        }
        if let Some(min) = self.min_seeders {
            if release.seeders < min {
                return false;
            }
        }
        if let Some(max) = self.max_size_bytes {
            if release.size_bytes > max {
                return false;
            }
        }
        if let Some(name) = &self.tracker {
            if release.tracker.name != *name {
                return false;
            }
        }
        true
    }
}

/// Filtro de áudio: o release precisa ter pelo menos uma faixa que case.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AudioFilter {
    /// Português brasileiro explícito.
    PtBr,
    /// Português europeu.
    PtPt,
    /// Português ambíguo (sem hint regional).
    Pt,
    /// Qualquer variante de português (PT-BR, PT-PT ou Pt).
    PtAny,
    /// Inglês.
    En,
}

/// Filtro de legenda: o release precisa ter pelo menos uma faixa de
/// legenda que case.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubtitleFilter {
    /// PT-BR.
    PtBr,
    /// PT-PT.
    PtPt,
    /// PT-BR ou PT-PT.
    PtAny,
}

/// Filtro de resolução.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub enum ResolutionFilter {
    /// Pelo menos 720p (qualquer ≥ 720p casa).
    #[serde(rename = "min-720")]
    At720,
    /// Pelo menos 1080p (1080p ou 2160p).
    #[serde(rename = "min-1080")]
    At1080,
    /// Pelo menos 2160p.
    #[serde(rename = "min-2160")]
    At2160,
    /// Match exato 1080p (não casa 2160p).
    #[serde(rename = "exact-1080")]
    Exact1080,
    /// Match exato 2160p.
    #[serde(rename = "exact-2160")]
    Exact2160,
}

fn audio_matches(release: &Release, a: &AudioFilter) -> bool {
    let Some(e) = release.enrichment.as_ref() else {
        return false;
    };
    match a {
        AudioFilter::PtBr => e.has_audio_in(&Language::PtBr),
        AudioFilter::PtPt => e.has_audio_in(&Language::PtPt),
        AudioFilter::Pt => e.has_audio_in(&Language::Pt),
        AudioFilter::PtAny => {
            e.has_audio_in(&Language::PtBr)
                || e.has_audio_in(&Language::PtPt)
                || e.has_audio_in(&Language::Pt)
        }
        AudioFilter::En => e.has_audio_in(&Language::En),
    }
}

fn subtitle_matches(release: &Release, s: &SubtitleFilter) -> bool {
    let Some(e) = release.enrichment.as_ref() else {
        return false;
    };
    match s {
        SubtitleFilter::PtBr => e.has_subtitle_in(&Language::PtBr),
        SubtitleFilter::PtPt => e.has_subtitle_in(&Language::PtPt),
        SubtitleFilter::PtAny => {
            e.has_subtitle_in(&Language::PtBr) || e.has_subtitle_in(&Language::PtPt)
        }
    }
}

fn resolution_matches(r: &Resolution, filter: &ResolutionFilter) -> bool {
    let rank = resolution_rank(r);
    match filter {
        ResolutionFilter::At720 => rank >= 1,
        ResolutionFilter::At1080 => rank >= 2,
        ResolutionFilter::At2160 => rank >= 3,
        ResolutionFilter::Exact1080 => matches!(r, Resolution::P1080),
        ResolutionFilter::Exact2160 => matches!(r, Resolution::P2160),
    }
}

fn resolution_rank(r: &Resolution) -> u8 {
    match r {
        Resolution::P720 => 1,
        Resolution::P1080 => 2,
        Resolution::P2160 => 3,
        Resolution::Sd | Resolution::Other(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::too_many_arguments,
        clippy::similar_names
    )]

    use super::{
        AudioFilter, Condition, ResolutionFilter, SubtitleFilter, audio_matches,
        resolution_matches, subtitle_matches,
    };
    use brarr_core::{
        Language, Release, ReleaseEnrichment, ReleaseKind, Resolution, TrackerSource,
    };
    use url::Url;

    fn tracker(name: &str) -> TrackerSource {
        TrackerSource::new(name, Url::parse("https://e.com/").unwrap()).unwrap()
    }

    fn release(
        audio: Vec<Language>,
        subs: Vec<Language>,
        resolution: Resolution,
        seeders: u32,
        size: u64,
        hdr: bool,
        tracker_name: &str,
    ) -> Release {
        let mut r = Release::new(
            "1",
            tracker(tracker_name),
            "x",
            ReleaseKind::WebDl,
            resolution,
            size,
        )
        .unwrap();
        r.seeders = seeders;
        r.enrichment = Some(ReleaseEnrichment {
            container_format: None,
            duration: None,
            audio_languages: audio,
            subtitle_languages: subs,
            has_forced_subs: false,
            has_hdr: hdr,
        });
        r
    }

    #[test]
    fn empty_condition_matches_any_release() {
        let r = release(vec![], vec![], Resolution::P1080, 0, 0, false, "t");
        assert!(Condition::default().matches(&r));
    }

    #[test]
    fn audio_pt_br_matches_only_when_present() {
        let with_pt_br = release(
            vec![Language::PtBr],
            vec![],
            Resolution::P1080,
            0,
            0,
            false,
            "t",
        );
        let without = release(
            vec![Language::En],
            vec![],
            Resolution::P1080,
            0,
            0,
            false,
            "t",
        );
        assert!(audio_matches(&with_pt_br, &AudioFilter::PtBr));
        assert!(!audio_matches(&without, &AudioFilter::PtBr));
    }

    #[test]
    fn audio_pt_any_matches_any_pt_variant() {
        let pt_br = release(
            vec![Language::PtBr],
            vec![],
            Resolution::P1080,
            0,
            0,
            false,
            "t",
        );
        let pt_pt = release(
            vec![Language::PtPt],
            vec![],
            Resolution::P1080,
            0,
            0,
            false,
            "t",
        );
        let pt_ambiguous = release(
            vec![Language::Pt],
            vec![],
            Resolution::P1080,
            0,
            0,
            false,
            "t",
        );
        let only_en = release(
            vec![Language::En],
            vec![],
            Resolution::P1080,
            0,
            0,
            false,
            "t",
        );
        assert!(audio_matches(&pt_br, &AudioFilter::PtAny));
        assert!(audio_matches(&pt_pt, &AudioFilter::PtAny));
        assert!(audio_matches(&pt_ambiguous, &AudioFilter::PtAny));
        assert!(!audio_matches(&only_en, &AudioFilter::PtAny));
    }

    #[test]
    fn subtitle_pt_any_matches_pt_br_or_pt_pt() {
        let pt_br = release(
            vec![],
            vec![Language::PtBr],
            Resolution::P1080,
            0,
            0,
            false,
            "t",
        );
        let pt_pt = release(
            vec![],
            vec![Language::PtPt],
            Resolution::P1080,
            0,
            0,
            false,
            "t",
        );
        let en_only = release(
            vec![],
            vec![Language::En],
            Resolution::P1080,
            0,
            0,
            false,
            "t",
        );
        assert!(subtitle_matches(&pt_br, &SubtitleFilter::PtAny));
        assert!(subtitle_matches(&pt_pt, &SubtitleFilter::PtAny));
        assert!(!subtitle_matches(&en_only, &SubtitleFilter::PtAny));
    }

    #[test]
    fn resolution_at_1080_matches_1080_and_2160() {
        assert!(resolution_matches(
            &Resolution::P1080,
            &ResolutionFilter::At1080
        ));
        assert!(resolution_matches(
            &Resolution::P2160,
            &ResolutionFilter::At1080
        ));
        assert!(!resolution_matches(
            &Resolution::P720,
            &ResolutionFilter::At1080
        ));
        assert!(!resolution_matches(
            &Resolution::Sd,
            &ResolutionFilter::At1080
        ));
    }

    #[test]
    fn resolution_exact_1080_excludes_2160() {
        assert!(resolution_matches(
            &Resolution::P1080,
            &ResolutionFilter::Exact1080
        ));
        assert!(!resolution_matches(
            &Resolution::P2160,
            &ResolutionFilter::Exact1080
        ));
    }

    #[test]
    fn condition_combines_fields_with_and() {
        let r = release(
            vec![Language::PtBr],
            vec![],
            Resolution::P2160,
            10,
            1_000_000_000,
            true,
            "capybara",
        );
        let cond = Condition {
            audio: Some(AudioFilter::PtBr),
            hdr: Some(true),
            resolution: Some(ResolutionFilter::At2160),
            min_seeders: Some(5),
            tracker: Some("capybara".into()),
            ..Condition::default()
        };
        assert!(cond.matches(&r));

        let cond_seeders_too_high = Condition {
            min_seeders: Some(100),
            ..cond.clone()
        };
        assert!(!cond_seeders_too_high.matches(&r));

        let cond_wrong_tracker = Condition {
            tracker: Some("locadora".into()),
            ..cond
        };
        assert!(!cond_wrong_tracker.matches(&r));
    }

    #[test]
    fn condition_max_size_caps_release() {
        let r = release(
            vec![],
            vec![],
            Resolution::P1080,
            0,
            5_000_000_000,
            false,
            "t",
        );
        let small_only = Condition {
            max_size_bytes: Some(2_000_000_000),
            ..Condition::default()
        };
        assert!(!small_only.matches(&r));
        let permissive = Condition {
            max_size_bytes: Some(10_000_000_000),
            ..Condition::default()
        };
        assert!(permissive.matches(&r));
    }

    #[test]
    fn ruleset_serde_json_roundtrips_baseline_exactly() {
        // The orchestrator persists `RuleSet`s as JSON in
        // `quality_profiles.rules_json`. Round-tripping the baseline
        // through serde_json must preserve every rule + condition
        // exactly so the engine produces identical scores before and
        // after a save.
        let baseline = super::RuleSet::baseline();
        let json = serde_json::to_string(&baseline).unwrap();
        let parsed: super::RuleSet = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.rules.len(), baseline.rules.len());
        for (a, b) in parsed.rules.iter().zip(baseline.rules.iter()) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.add_score, b.add_score);
            assert_eq!(a.tag, b.tag);
            assert_eq!(a.reject, b.reject);
            assert_eq!(a.when.audio, b.when.audio);
            assert_eq!(a.when.subtitle, b.when.subtitle);
            assert_eq!(a.when.hdr, b.when.hdr);
            assert_eq!(a.when.resolution, b.when.resolution);
            assert_eq!(a.when.min_seeders, b.when.min_seeders);
            assert_eq!(a.when.max_size_bytes, b.when.max_size_bytes);
            assert_eq!(a.when.tracker, b.when.tracker);
        }
    }

    #[test]
    fn condition_hdr_required_matches_only_hdr() {
        let hdr = release(vec![], vec![], Resolution::P2160, 0, 0, true, "t");
        let sdr = release(vec![], vec![], Resolution::P1080, 0, 0, false, "t");
        let only_hdr = Condition {
            hdr: Some(true),
            ..Condition::default()
        };
        let only_sdr = Condition {
            hdr: Some(false),
            ..Condition::default()
        };
        assert!(only_hdr.matches(&hdr));
        assert!(!only_hdr.matches(&sdr));
        assert!(only_sdr.matches(&sdr));
        assert!(!only_sdr.matches(&hdr));
    }
}
