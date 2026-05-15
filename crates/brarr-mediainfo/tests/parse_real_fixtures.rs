//! Testes de integração contra dumps reais de `MediaInfo` capturados de
//! dois trackers `UNIT3D` (capybarabr.com e locadora.cc) para o mesmo
//! filme — Matrix 1999. Cobertura prática das duas convenções de
//! marcação de idioma que aparecem na natureza:
//!
//! - **`shadow_2160p.txt`** (capybara, 2160p HDR HEVC, line endings `\r\n`):
//!   `Language: Portuguese` + `Title: Brazilian Portuguese`.
//! - **`vnlls_1080p.txt`** (locadora, 1080p AVC, line endings `\n`,
//!   28 faixas de legenda):
//!   `Language: Portuguese (BR)` direto, mais várias línguas exóticas
//!   (`Catalan (ES)`, `Serbian-Latn-RS`, etc.).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;

use brarr_mediainfo::{Language, parse};

const SHADOW_PATH: &str = "tests/fixtures/shadow_2160p.txt";
const VNLLS_PATH: &str = "tests/fixtures/vnlls_1080p.txt";

fn load(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("could not read fixture {path}: {e}"))
}

// =========================== shadow (capybara, 2160p) ===========================

#[test]
fn shadow_general_section() {
    let parsed = parse(&load(SHADOW_PATH)).expect("shadow fixture should parse");
    assert_eq!(parsed.general.container_format.as_deref(), Some("Matroska"));
    assert_eq!(
        parsed.general.complete_name.as_deref(),
        Some("Matrix.1999.2160p.HMAX.WEB-DL.DDP5.1.Atmos.HDR.x265.DUAL-sh4down.mkv"),
    );
    assert_eq!(parsed.general.file_size_raw.as_deref(), Some("18.1 GiB"));
    // 2 h 16 min == 8160 s
    assert_eq!(
        parsed.general.duration,
        Some(std::time::Duration::from_secs(2 * 3600 + 16 * 60)),
    );
}

#[test]
fn shadow_has_one_video_track_hevc_2160p_hdr() {
    let parsed = parse(&load(SHADOW_PATH)).expect("shadow fixture should parse");
    assert_eq!(parsed.video.len(), 1);
    let v = &parsed.video[0];
    assert_eq!(v.format.as_deref(), Some("HEVC"));
    assert_eq!(v.width, Some(3840));
    assert_eq!(v.height, Some(2160));
    assert_eq!(v.bit_depth, Some(10));
    assert!(v.hdr_format.is_some(), "HDR format should be detected");
    assert!(v.default);
}

#[test]
fn shadow_has_two_audio_tracks_pt_br_first_english_atmos_second() {
    let parsed = parse(&load(SHADOW_PATH)).expect("shadow fixture should parse");
    assert_eq!(parsed.audio.len(), 2);

    // Audio #1: Portuguese with Title "Brazilian Portuguese" → PtBr
    let pt = &parsed.audio[0];
    assert_eq!(pt.format.as_deref(), Some("E-AC-3"));
    assert_eq!(pt.channels, Some(2));
    assert_eq!(pt.language, Language::PtBr);
    assert_eq!(pt.title.as_deref(), Some("Brazilian Portuguese"));
    assert!(pt.default);
    assert!(!pt.forced);

    // Audio #2: English Atmos
    let en = &parsed.audio[1];
    assert_eq!(en.format.as_deref(), Some("E-AC-3 JOC"));
    assert_eq!(en.channels, Some(6));
    assert_eq!(en.language, Language::En);
    assert_eq!(
        en.commercial_name.as_deref(),
        Some("Dolby Digital Plus with Dolby Atmos"),
    );
    assert!(!en.default);
}

#[test]
fn shadow_has_two_subtitles_forced_pt_then_full_pt() {
    let parsed = parse(&load(SHADOW_PATH)).expect("shadow fixture should parse");
    assert_eq!(parsed.subtitles.len(), 2);

    // Text #1: forced PT (Language: Portuguese, Title: Forced → ambiguous Pt
    //          since Title="Forced" is not a regional hint)
    let forced = &parsed.subtitles[0];
    assert_eq!(forced.title.as_deref(), Some("Forced"));
    assert!(forced.forced);
    assert!(!forced.default);
    // Sem hint de região, cai em Pt ambíguo
    assert_eq!(forced.language, Language::Pt);

    // Text #2: full PT (no Title, Language: Portuguese)
    let full = &parsed.subtitles[1];
    assert!(!full.forced);
    assert_eq!(full.language, Language::Pt);
}

// =========================== vnlls (locadora, 1080p) ===========================

#[test]
fn vnlls_general_section() {
    let parsed = parse(&load(VNLLS_PATH)).expect("vnlls fixture should parse");
    assert_eq!(parsed.general.container_format.as_deref(), Some("Matroska"));
    assert_eq!(
        parsed.general.complete_name.as_deref(),
        Some("The.Matrix.1999.1080p.HMAX.WEB-DL.DD2.0.H.264.pt-BR.ENG-vnlls.mkv"),
    );
    assert_eq!(parsed.general.file_size_raw.as_deref(), Some("8.95 GiB"));
}

#[test]
fn vnlls_has_one_video_track_avc_1080p_sdr() {
    let parsed = parse(&load(VNLLS_PATH)).expect("vnlls fixture should parse");
    assert_eq!(parsed.video.len(), 1);
    let v = &parsed.video[0];
    assert_eq!(v.format.as_deref(), Some("AVC"));
    assert_eq!(v.width, Some(1920));
    assert_eq!(v.height, Some(816));
    assert_eq!(v.bit_depth, Some(8));
    // No HDR field → None (SDR)
    assert!(v.hdr_format.is_none());
}

#[test]
fn vnlls_audio_first_is_pt_br_via_explicit_region_tag() {
    let parsed = parse(&load(VNLLS_PATH)).expect("vnlls fixture should parse");
    assert_eq!(parsed.audio.len(), 2);

    let pt = &parsed.audio[0];
    assert_eq!(pt.format.as_deref(), Some("AC-3"));
    assert_eq!(pt.channels, Some(2));
    assert_eq!(pt.language, Language::PtBr);
    assert_eq!(pt.title.as_deref(), Some("Brazilian"));
    assert!(pt.default);

    let en = &parsed.audio[1];
    assert_eq!(en.format.as_deref(), Some("E-AC-3 JOC"));
    assert_eq!(en.channels, Some(6));
    assert_eq!(en.language, Language::En);
}

#[test]
fn vnlls_has_28_subtitle_tracks() {
    let parsed = parse(&load(VNLLS_PATH)).expect("vnlls fixture should parse");
    assert_eq!(
        parsed.subtitles.len(),
        28,
        "vnlls fixture should have 28 subtitle tracks (Text #1..Text #28)",
    );
}

#[test]
fn vnlls_first_subtitle_is_pt_br_forced() {
    let parsed = parse(&load(VNLLS_PATH)).expect("vnlls fixture should parse");
    let s = &parsed.subtitles[0];
    assert_eq!(s.language, Language::PtBr);
    assert_eq!(s.title.as_deref(), Some("Brazilian (Forced)"));
    assert!(s.forced);
    assert!(s.default);
}

#[test]
fn vnlls_subtitle_pt_pt_distinguished_from_pt_br() {
    let parsed = parse(&load(VNLLS_PATH)).expect("vnlls fixture should parse");
    // Text #3 (index 2): Language=Portuguese (PT), Title=European
    let pt_pt = &parsed.subtitles[2];
    assert_eq!(pt_pt.language, Language::PtPt);
    assert_eq!(pt_pt.title.as_deref(), Some("European"));
}

#[test]
fn vnlls_subtitle_languages_include_exotic_others() {
    let parsed = parse(&load(VNLLS_PATH)).expect("vnlls fixture should parse");
    // Confirma que idiomas fora do conjunto suportado caem em Other
    // preservando a string original.
    let has_serbian_latin = parsed
        .subtitles
        .iter()
        .any(|s| matches!(&s.language, Language::Other(raw) if raw == "Serbian-Latn-RS"));
    assert!(has_serbian_latin, "expected Serbian-Latn-RS in Other(_)");

    let has_catalan_es = parsed
        .subtitles
        .iter()
        .any(|s| matches!(&s.language, Language::Other(raw) if raw == "Catalan (ES)"));
    assert!(has_catalan_es, "expected Catalan (ES) in Other(_)");
}

#[test]
fn vnlls_pt_br_subtitle_count_exactly_two() {
    let parsed = parse(&load(VNLLS_PATH)).expect("vnlls fixture should parse");
    let pt_br_subs = parsed
        .subtitles
        .iter()
        .filter(|s| s.language == Language::PtBr)
        .count();
    assert_eq!(pt_br_subs, 2, "vnlls has one PT-BR forced + one PT-BR full");
}

// =========================== cross-fixture invariants ===========================

#[test]
fn both_fixtures_parse_with_at_least_one_pt_br_audio() {
    for path in [SHADOW_PATH, VNLLS_PATH] {
        let parsed = parse(&load(path)).expect("fixture should parse");
        let has_pt_br_audio = parsed.audio.iter().any(|t| t.language == Language::PtBr);
        assert!(
            has_pt_br_audio,
            "fixture {path} should have at least one PT-BR audio track",
        );
    }
}
