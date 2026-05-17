//! [`Engine`] — avalia um [`Release`] contra um [`RuleSet`] e produz um
//! [`DecisionOutcome`].
//!
//! Estratégia: itera todas as regras em ordem. Para cada regra que
//! casa, soma o `add_score`, anexa `tag` (se presente), e marca
//! `rejected = true` se a regra pediu rejeição. **Não** curto-circuita
//! ao primeiro `reject` — o caller pode querer ver todos os motivos.

use brarr_core::{DecisionScore, Release};

use crate::outcome::DecisionOutcome;
use crate::rule::RuleSet;

/// Motor de regras. Fino wrapper em torno de um [`RuleSet`].
#[derive(Debug, Clone)]
pub struct Engine {
    rules: RuleSet,
}

impl Engine {
    /// Constrói um motor com o `RuleSet` dado.
    #[must_use]
    pub const fn new(rules: RuleSet) -> Self {
        Self { rules }
    }

    /// Motor com regras default (equivalente ao scoring hardcoded da
    /// Fase 5 — `ScoringWeights::default()` em `brarr-cli`).
    #[must_use]
    pub fn baseline() -> Self {
        Self::new(RuleSet::baseline())
    }

    /// Build an engine from a profile's persisted rule list. When the
    /// list is empty the call falls back to [`Self::baseline`] so
    /// profiles created via the UI without any rules behave identically
    /// to no-profile attachment. Used by the orchestrator's push
    /// pipeline to pick the right engine per ARR instance once it
    /// reaches the profile-resolution step.
    #[must_use]
    pub fn from_profile_rules(rules: RuleSet) -> Self {
        if rules.rules.is_empty() {
            Self::baseline()
        } else {
            Self::new(rules)
        }
    }

    /// Aplica todas as regras contra `release`.
    #[must_use]
    pub fn evaluate(&self, release: &Release) -> DecisionOutcome {
        let mut score_acc: u32 = 0;
        let mut tags = Vec::new();
        let mut matched = Vec::new();
        let mut rejected = false;

        for (idx, rule) in self.rules.rules.iter().enumerate() {
            if !rule.when.matches(release) {
                continue;
            }
            score_acc = score_acc.saturating_add(rule.add_score);
            if let Some(t) = rule.tag.clone() {
                tags.push(t);
            }
            if rule.reject {
                rejected = true;
            }
            matched.push(rule.name.clone().unwrap_or_else(|| format!("rule[{idx}]")));
        }

        // Bonus de seeders fora das regras: replicar comportamento da
        // Fase 5 onde cada seeder vale +1 ponto até cap=50. Esse era um
        // efeito implícito do scoring antigo; movido pra cá pra manter
        // paridade quando RuleSet::baseline() é usado e ninguém define
        // regra customizada por seeders.
        let seeders_counted = release.seeders.min(50);
        score_acc = score_acc.saturating_add(seeders_counted);

        DecisionOutcome {
            score: DecisionScore::saturating(score_acc),
            tags,
            rejected,
            matched_rules: matched,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::Engine;
    use crate::rule::{AudioFilter, Condition, ResolutionFilter, Rule, RuleSet};
    use brarr_core::{
        Language, Release, ReleaseEnrichment, ReleaseKind, Resolution, TrackerSource,
    };
    use url::Url;

    fn tracker(name: &str) -> TrackerSource {
        TrackerSource::new(name, Url::parse("https://e.com/").unwrap()).unwrap()
    }

    fn release(audio: Vec<Language>, resolution: Resolution, seeders: u32, hdr: bool) -> Release {
        let mut r =
            Release::new("1", tracker("t"), "x", ReleaseKind::WebDl, resolution, 0).unwrap();
        r.seeders = seeders;
        r.enrichment = Some(ReleaseEnrichment {
            container_format: None,
            duration: None,
            audio_languages: audio,
            subtitle_languages: vec![],
            has_forced_subs: false,
            has_hdr: hdr,
        });
        r
    }

    #[test]
    fn baseline_engine_reproduces_legacy_scoring_for_pt_br_1080p() {
        // Fase 5 baseline: PT-BR audio (100) + 1080p (10) = 110, sem seeders.
        let r = release(vec![Language::PtBr], Resolution::P1080, 0, false);
        let out = Engine::baseline().evaluate(&r);
        assert_eq!(out.score.get(), 110);
        assert!(!out.rejected);
    }

    #[test]
    fn baseline_engine_adds_seeders_capped_at_50() {
        let r_few = release(vec![Language::PtBr], Resolution::P1080, 10, false);
        let r_many = release(vec![Language::PtBr], Resolution::P1080, 1_000, false);
        let engine = Engine::baseline();
        assert_eq!(engine.evaluate(&r_few).score.get(), 110 + 10);
        assert_eq!(engine.evaluate(&r_many).score.get(), 110 + 50);
    }

    #[test]
    fn baseline_engine_hdr_2160_pt_br_high_score() {
        // PT-BR (100) + HDR (10) + 2160p (20) = 130
        let r = release(vec![Language::PtBr], Resolution::P2160, 0, true);
        let out = Engine::baseline().evaluate(&r);
        assert_eq!(out.score.get(), 130);
    }

    #[test]
    fn baseline_engine_pt_ambiguous_scores_50() {
        let r = release(vec![Language::Pt], Resolution::P1080, 0, false);
        let out = Engine::baseline().evaluate(&r);
        // PT ambíguo (50) + 1080p (10) = 60
        assert_eq!(out.score.get(), 60);
    }

    #[test]
    fn engine_collects_matched_rule_names_in_order() {
        let r = release(vec![Language::PtBr], Resolution::P2160, 0, true);
        let out = Engine::baseline().evaluate(&r);
        assert!(out.matched_rules.iter().any(|n| n.contains("PT-BR")));
        assert!(out.matched_rules.iter().any(|n| n == "HDR"));
        assert!(out.matched_rules.iter().any(|n| n.contains("2160p")));
    }

    #[test]
    fn custom_rule_with_reject_marks_outcome() {
        let r = release(vec![Language::En], Resolution::P1080, 0, false);
        let rules = RuleSet {
            rules: vec![Rule {
                name: Some("reject non-PT".into()),
                when: Condition {
                    audio: None,
                    ..Condition::default()
                },
                add_score: 0,
                tag: Some("sem PT".into()),
                reject: true,
            }],
        };
        let out = Engine::new(rules).evaluate(&r);
        assert!(out.rejected);
        assert_eq!(out.tags, vec!["sem PT".to_string()]);
    }

    #[test]
    fn custom_compound_rule_only_fires_when_all_conditions_match() {
        let rules = RuleSet {
            rules: vec![Rule {
                name: Some("PT-BR + 2160p + HDR jackpot".into()),
                when: Condition {
                    audio: Some(AudioFilter::PtBr),
                    hdr: Some(true),
                    resolution: Some(ResolutionFilter::At2160),
                    ..Condition::default()
                },
                add_score: 500,
                tag: Some("jackpot".into()),
                reject: false,
            }],
        };
        let engine = Engine::new(rules);
        let jackpot = release(vec![Language::PtBr], Resolution::P2160, 0, true);
        let close = release(vec![Language::PtBr], Resolution::P2160, 0, false); // sem HDR
        assert_eq!(engine.evaluate(&jackpot).score.get(), 500);
        assert_eq!(engine.evaluate(&close).score.get(), 0);
    }

    #[test]
    fn empty_ruleset_scores_only_seeders() {
        let r = release(vec![Language::PtBr], Resolution::P2160, 25, true);
        let out = Engine::new(RuleSet::default()).evaluate(&r);
        assert_eq!(out.score.get(), 25);
    }

    #[test]
    fn from_profile_rules_falls_back_to_baseline_when_empty() {
        // Profiles created via the orchestrator UI without any custom
        // rules carry an empty RuleSet. Treat that as "use baseline"
        // so a fresh profile behaves identically to no attachment.
        let r = release(vec![Language::PtBr], Resolution::P1080, 0, false);
        let custom = Engine::from_profile_rules(RuleSet::default()).evaluate(&r);
        let baseline = Engine::baseline().evaluate(&r);
        assert_eq!(custom.score.get(), baseline.score.get());
        assert_eq!(custom.matched_rules, baseline.matched_rules);
    }

    #[test]
    fn from_profile_rules_uses_explicit_set_when_provided() {
        let r = release(vec![Language::PtBr], Resolution::P1080, 0, false);
        let only_pt = RuleSet {
            rules: vec![crate::rule::Rule {
                name: Some("just PT-BR".into()),
                when: Condition {
                    audio: Some(AudioFilter::PtBr),
                    ..Condition::default()
                },
                add_score: 999,
                tag: None,
                reject: false,
            }],
        };
        let custom = Engine::from_profile_rules(only_pt).evaluate(&r);
        // Custom 999 + 0 seeders = 999. Baseline 110 would be way lower.
        assert_eq!(custom.score.get(), 999);
    }
}
