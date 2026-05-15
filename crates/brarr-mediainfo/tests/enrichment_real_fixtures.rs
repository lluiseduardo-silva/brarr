//! Testes de integração para [`ParsedMediaInfo::to_enrichment`] usando
//! os mesmos dois dumps reais que cobrem o parser
//! (shadow 2160p HDR, vnlls 1080p SDR com 28 legendas).
//!
//! Verifica que o "destilado" mantém a informação que regras de
//! scoring precisam: presença de PT-BR em áudio, distinção PT-BR vs
//! PT-PT em legendas, flags `has_hdr` e `has_forced_subs`,
//! `container_format` e `duration`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::time::Duration;

use brarr_core::Language;
use brarr_mediainfo::parse;

const SHADOW_PATH: &str = "tests/fixtures/shadow_2160p.txt";
const VNLLS_PATH: &str = "tests/fixtures/vnlls_1080p.txt";

fn load(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("could not read fixture {path}: {e}"))
}

#[test]
fn shadow_enrichment_has_pt_br_audio_and_hdr() {
    let parsed = parse(&load(SHADOW_PATH)).expect("shadow fixture should parse");
    let e = parsed.to_enrichment();

    assert_eq!(e.container_format.as_deref(), Some("Matroska"));
    assert_eq!(e.duration, Some(Duration::from_secs(2 * 3600 + 16 * 60)));

    // Áudios: PtBr (Title Brazilian Portuguese) + En Atmos
    assert_eq!(
        e.audio_languages,
        vec![Language::PtBr, Language::En],
        "shadow should have PT-BR then English audio",
    );
    assert!(e.has_audio_in(&Language::PtBr));
    assert!(e.has_audio_in(&Language::En));
    assert!(!e.has_audio_in(&Language::PtPt));

    // Legendas: dois PT ambíguos (Title=Forced e sem Title)
    assert_eq!(e.subtitle_languages, vec![Language::Pt, Language::Pt]);
    assert_eq!(e.subtitle_count_in(&Language::Pt), 2);

    // Flags
    assert!(e.has_hdr, "shadow has HDR HEVC video");
    assert!(e.has_forced_subs, "shadow has a forced PT subtitle");
}

#[test]
fn vnlls_enrichment_distinguishes_pt_variants_in_subs_and_lacks_hdr() {
    let parsed = parse(&load(VNLLS_PATH)).expect("vnlls fixture should parse");
    let e = parsed.to_enrichment();

    assert_eq!(e.container_format.as_deref(), Some("Matroska"));

    // Áudios: PtBr (Language=Portuguese (BR)) + En
    assert_eq!(e.audio_languages, vec![Language::PtBr, Language::En]);

    // Legendas: 28 trilhas. Composição:
    // - PT-BR x2 (Text #1 forced + Text #2 full)
    // - PT-PT x1 (Text #3 European)
    // - English x2 (Text #4 SDH + Text #5)
    // - Other(...) x23 (Bulgarian, Catalan (ES), Czech, ...)
    assert_eq!(e.subtitle_languages.len(), 28);
    assert_eq!(e.subtitle_count_in(&Language::PtBr), 2);
    assert_eq!(e.subtitle_count_in(&Language::PtPt), 1);
    assert_eq!(e.subtitle_count_in(&Language::En), 2);
    assert_eq!(e.subtitle_count_in(&Language::Pt), 0);

    // Flags
    assert!(!e.has_hdr, "vnlls is 1080p SDR AVC");
    assert!(e.has_forced_subs, "vnlls has a PT-BR forced subtitle");
}

#[test]
fn enrichment_is_pure_function_of_parsed_input() {
    // Roda duas vezes e compara: garante que to_enrichment é idempotente
    // e não depende de estado externo.
    let parsed = parse(&load(VNLLS_PATH)).expect("vnlls fixture should parse");
    assert_eq!(parsed.to_enrichment(), parsed.to_enrichment());
}
