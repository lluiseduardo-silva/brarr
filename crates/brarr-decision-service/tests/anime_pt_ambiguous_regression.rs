//! Regressão de mundo real: dois releases do mesmo título de anime
//! ("That Time I Got Reincarnated as a Slime S01 REPACK 1080p"), um de um
//! tracker que marca PT-BR regional (samaritano) e outro que entrega PT
//! ambíguo sem hint regional (capybara), avaliados sob o profile "via
//! Animes" do operador.
//!
//! **Histórico e resolução (2026-07-08).** Este teste nasceu diagnóstico:
//! na UI, samaritano = 332 e capybara = 187, e a pergunta era se o gap de
//! 145 pontos (o bônus `subtitle = "pt-any"` que só o samaritano ganhava)
//! era o comportamento desejado. Um caso concreto do nzbgeek — o release
//! `How.a.Realist.Hero...S01E01.1080p.WEB.H264-SUGOI`, com attrs
//! `language=Japanese` + `subs=Portuguese` (genérico, sem região) —
//! deixou claro que a fonte só fornece "Portuguese" ambíguo, e o operador
//! decidiu que **legenda PT ambígua deve contar** (o card mostrava "PT
//! legenda" mas pontuava 180 em vez de 330).
//!
//! A correção tornou `SubtitleFilter::PtAny` simétrico com
//! `AudioFilter::PtAny`: ambos agora incluem `Language::Pt` ambíguo. Logo:
//!
//! 1. Ambos os releases coletam o +150 do bônus de legenda PT.
//! 2. `subtitle = "pt-any"` casa PT-BR, PT-PT **e** PT ambíguo.
//! 3. A regra "Rejeita sem legenda PT" (`not pt-any`) **não** dispara para
//!    o capybara — ele tem legenda PT ambígua, que agora satisfaz `pt-any`.
//!
//! O único delta remanescente entre os dois é a contagem de seeders.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use brarr_core::{Language, Release, ReleaseEnrichment, ReleaseKind, Resolution, TrackerSource};
use brarr_decision_service::{Engine, RuleSet};
use url::Url;

const TITLE: &str = "That Time I Got Reincarnated as a Slime S01 REPACK 1080p";

/// O `rules_json` do profile "via Animes" exatamente como persistido —
/// colado verbatim do que o operador relatou para garantir que estamos
/// testando as MESMAS regras que rodaram em produção.
const VIA_ANIMES_RULES_JSON: &str = r#"{
  "rule": [
    {
      "name": "Bônus 1080p+",
      "when": {
        "audio": null,
        "subtitle": null,
        "hdr": null,
        "resolution": "min-1080",
        "min_seeders": null,
        "max_size_bytes": null,
        "tracker": null
      },
      "add_score": 100,
      "tag": null,
      "reject": false
    },
    {
      "name": "Rejeita abaixo de 1080p",
      "when": {
        "audio": null,
        "subtitle": null,
        "hdr": null,
        "resolution": null,
        "min_seeders": null,
        "max_size_bytes": null,
        "tracker": null,
        "not": {
          "audio": null,
          "subtitle": null,
          "hdr": null,
          "resolution": "min-1080",
          "min_seeders": null,
          "max_size_bytes": null,
          "tracker": null
        }
      },
      "add_score": 0,
      "tag": null,
      "reject": true
    },
    {
      "name": "Áudio japonês",
      "when": {
        "audio": "jp",
        "subtitle": null,
        "hdr": null,
        "resolution": null,
        "min_seeders": null,
        "max_size_bytes": null,
        "tracker": null
      },
      "add_score": 80,
      "tag": "anime-original",
      "reject": false
    },
    {
      "name": "Legenda PT (bônus)",
      "when": {
        "audio": null,
        "subtitle": "pt-any",
        "hdr": null,
        "resolution": null,
        "min_seeders": null,
        "max_size_bytes": null,
        "tracker": null
      },
      "add_score": 150,
      "tag": "anime-leg",
      "reject": false
    },
    {
      "name": "Rejeita sem legenda PT",
      "when": {
        "audio": null,
        "subtitle": null,
        "hdr": null,
        "resolution": null,
        "min_seeders": null,
        "max_size_bytes": null,
        "tracker": null,
        "not": {
          "audio": null,
          "subtitle": "pt-any",
          "hdr": null,
          "resolution": null,
          "min_seeders": null,
          "max_size_bytes": null,
          "tracker": null
        }
      },
      "add_score": 0,
      "tag": null,
      "reject": true
    }
  ]
}"#;

fn tracker(name: &str) -> TrackerSource {
    TrackerSource::new(name, Url::parse("https://e.com/").unwrap()).unwrap()
}

/// Constrói um release de anime 1080p com as faixas de áudio/legenda e
/// seeders dados.
fn anime_release(
    tracker_name: &str,
    audio: Vec<Language>,
    subtitles: Vec<Language>,
    seeders: u32,
) -> Release {
    let mut r = Release::new(
        "1",
        tracker(tracker_name),
        TITLE,
        ReleaseKind::Encode,
        Resolution::P1080,
        35_000_000_000,
    )
    .unwrap();
    r.seeders = seeders;
    r.enrichment = Some(ReleaseEnrichment {
        audio_languages: audio,
        subtitle_languages: subtitles,
        ..ReleaseEnrichment::default()
    });
    r
}

/// samaritano: áudio PT-BR + JP, legenda PT-BR (regional) + EN + AR, 2 seeders.
fn samaritano() -> Release {
    anime_release(
        "samaritano",
        vec![Language::PtBr, Language::Jp],
        vec![
            Language::PtBr,
            Language::En,
            Language::Other("Arabic".into()),
        ],
        2,
    )
}

/// capybara: áudio PT ambíguo + JP, legenda PT ambígua + EN + AR + DE, 7 seeders.
/// Mesma mídia, mas o tracker entrega "Portuguese" sem sufixo regional —
/// a mesma forma que o nzbgeek emite (`subs=Portuguese`).
fn capybara() -> Release {
    anime_release(
        "capybara",
        vec![Language::Pt, Language::Jp],
        vec![
            Language::Pt,
            Language::En,
            Language::Other("Arabic".into()),
            Language::Other("German".into()),
        ],
        7,
    )
}

fn via_animes_engine() -> Engine {
    let rules: RuleSet = serde_json::from_str(VIA_ANIMES_RULES_JSON)
        .expect("rules_json do profile via Animes deve desserializar");
    Engine::new(rules)
}

#[test]
fn samaritano_scores_332_and_is_kept() {
    let out = via_animes_engine().evaluate(&samaritano());
    // 100 (1080p) + 80 (jp) + 150 (legenda PT-BR casa pt-any) + 2 seeders.
    assert_eq!(out.score.get(), 332, "score do samaritano");
    assert!(
        !out.rejected,
        "samaritano tem legenda PT-BR; não deve rejeitar"
    );
}

#[test]
fn capybara_scores_337_with_ambiguous_pt_bonus() {
    let out = via_animes_engine().evaluate(&capybara());
    // 100 (1080p) + 80 (jp) + 150 (legenda PT ambígua AGORA casa pt-any) + 7 seeders.
    assert_eq!(
        out.score.get(),
        337,
        "score do capybara com o bônus PT ambíguo"
    );
}

#[test]
fn capybara_legenda_bonus_now_fires_for_ambiguous_pt() {
    // O cerne da correção: o bônus de +150 passa a ser coletado porque
    // `subtitle = "pt-any"` agora inclui PT ambíguo (simétrico com áudio).
    let out = via_animes_engine().evaluate(&capybara());
    assert!(
        out.matched_rules.iter().any(|n| n == "Legenda PT (bônus)"),
        "bônus de legenda PT deve disparar para legenda ambígua; matched={:?}",
        out.matched_rules
    );
}

#[test]
fn engine_keeps_capybara_with_ambiguous_pt_subtitle() {
    // Contrapartida da correção: como o capybara agora satisfaz `pt-any`,
    // a regra "Rejeita sem legenda PT" (`not pt-any`) NÃO dispara.
    let out = via_animes_engine().evaluate(&capybara());
    assert!(
        !out.rejected,
        "capybara tem legenda PT ambígua ⇒ não deve rejeitar; matched={:?}",
        out.matched_rules
    );
    assert!(
        !out.matched_rules
            .iter()
            .any(|n| n == "Rejeita sem legenda PT"),
        "a regra de rejeição não deve constar nas casadas; matched={:?}",
        out.matched_rules
    );
}

#[test]
fn neither_release_rejected_by_sub_1080p_rule() {
    // Ambos são 1080p ⇒ a regra "Rejeita abaixo de 1080p" (not min-1080)
    // nunca dispara para nenhum dos dois — controle para isolar a causa.
    let s = via_animes_engine().evaluate(&samaritano());
    let c = via_animes_engine().evaluate(&capybara());
    assert!(
        !s.matched_rules
            .iter()
            .any(|n| n == "Rejeita abaixo de 1080p")
    );
    assert!(
        !c.matched_rules
            .iter()
            .any(|n| n == "Rejeita abaixo de 1080p")
    );
}
