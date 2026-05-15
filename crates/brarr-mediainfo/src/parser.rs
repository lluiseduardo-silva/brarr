//! Implementação do parser. Algoritmo:
//!
//! 1. Normaliza `\r\n` para `\n` (e descarta `\r` órfãos).
//! 2. Itera linhas. Linhas vazias fecham a seção corrente.
//! 3. Linha com `:` → campo (`key: value`) anexado à seção corrente.
//!    Sem seção corrente, é orfã e ignorada (caso típico:
//!    `ReportBy : MediaInfoLib - v23.10` no final do dump).
//! 4. Linha sem `:` → cabeçalho de seção. Mapeada para
//!    `General`/`Video`/`Audio`/`Text` (case-insensitive, `Audio #1`
//!    cai em `Audio`).
//! 5. Por seção, campos são distribuídos por tipo via `match` de
//!    `key.as_str()` — chaves não mapeadas são silenciosamente
//!    descartadas (parser é tolerante a campos desconhecidos).

use std::time::Duration;

use brarr_core::Language;

use crate::error::ParseError;
use crate::types::{AudioTrack, GeneralInfo, ParsedMediaInfo, SubtitleTrack, VideoTrack};

/// Faz o parse de um dump textual de `MediaInfo`.
///
/// Tolera tanto `\r\n` (típico de saídas via UNIT3D/Windows) quanto
/// `\n` (Unix) como separador. Campos desconhecidos são ignorados;
/// valores numéricos malformados viram `None`.
///
/// # Errors
///
/// - [`ParseError::Empty`] se a entrada é vazia ou só whitespace.
/// - [`ParseError::NoSections`] se nenhuma seção reconhecida
///   (`General`, `Video`, `Audio`, `Text`) aparece — sinal de que a
///   entrada não é um dump válido.
pub fn parse(text: &str) -> Result<ParsedMediaInfo, ParseError> {
    if text.trim().is_empty() {
        return Err(ParseError::Empty);
    }

    let sections = tokenize(text);

    // Conta seções *reconhecidas* — um dump que só tem linhas órfãs
    // (ou apenas `Other`) é considerado sem seções.
    let recognized = sections
        .iter()
        .filter(|s| !matches!(s.header, SectionHeader::Other))
        .count();
    if recognized == 0 {
        return Err(ParseError::NoSections);
    }

    let mut out = ParsedMediaInfo {
        general: GeneralInfo::default(),
        video: Vec::new(),
        audio: Vec::new(),
        subtitles: Vec::new(),
    };

    for section in sections {
        match section.header {
            SectionHeader::General => out.general = parse_general(&section.fields),
            SectionHeader::Video => out.video.push(parse_video(&section.fields)),
            SectionHeader::Audio => out.audio.push(parse_audio(&section.fields)),
            SectionHeader::Text => out.subtitles.push(parse_subtitle(&section.fields)),
            SectionHeader::Other => {}
        }
    }

    Ok(out)
}

// --- tokenização ---

#[derive(Debug)]
struct Section {
    header: SectionHeader,
    fields: Vec<(String, String)>,
}

#[derive(Debug, PartialEq, Eq)]
enum SectionHeader {
    General,
    Video,
    Audio,
    Text,
    Other,
}

impl SectionHeader {
    fn from_line(line: &str) -> Self {
        let lc = line.trim().to_ascii_lowercase();
        if lc == "general" {
            Self::General
        } else if lc == "video" || lc.starts_with("video #") {
            Self::Video
        } else if lc == "audio" || lc.starts_with("audio #") {
            Self::Audio
        } else if lc == "text" || lc.starts_with("text #") {
            Self::Text
        } else {
            Self::Other
        }
    }
}

fn tokenize(text: &str) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    let mut current: Option<Section> = None;

    // Normaliza line endings: `\r\n` → `\n`, descarta `\r` solto.
    let normalized = text.replace("\r\n", "\n").replace('\r', "");

    for line in normalized.split('\n') {
        let trimmed = line.trim_end();

        if trimmed.is_empty() {
            if let Some(s) = current.take() {
                sections.push(s);
            }
        } else if let Some((key, value)) = trimmed.split_once(':') {
            if let Some(s) = current.as_mut() {
                s.fields
                    .push((key.trim().to_string(), value.trim().to_string()));
            }
            // sem seção corrente → linha órfã ignorada
        } else {
            // cabeçalho de seção
            if let Some(s) = current.take() {
                sections.push(s);
            }
            current = Some(Section {
                header: SectionHeader::from_line(trimmed),
                fields: Vec::new(),
            });
        }
    }
    if let Some(s) = current.take() {
        sections.push(s);
    }
    sections
}

// --- distribuição por tipo ---

fn parse_general(fields: &[(String, String)]) -> GeneralInfo {
    let mut g = GeneralInfo::default();
    for (k, v) in fields {
        match k.as_str() {
            "Format" => g.container_format = Some(v.clone()),
            "Complete name" => g.complete_name = Some(v.clone()),
            "Duration" => g.duration = parse_duration(v),
            "File size" => g.file_size_raw = Some(v.clone()),
            _ => {}
        }
    }
    g
}

fn parse_video(fields: &[(String, String)]) -> VideoTrack {
    let mut t = VideoTrack::default();
    for (k, v) in fields {
        match k.as_str() {
            "ID" => t.id = parse_u32(v),
            "Format" => t.format = Some(v.clone()),
            "Width" => t.width = parse_unsigned_with_spaces(v),
            "Height" => t.height = parse_unsigned_with_spaces(v),
            "Bit depth" => {
                t.bit_depth = parse_unsigned_with_spaces(v).and_then(|n| u8::try_from(n).ok());
            }
            "HDR format" => t.hdr_format = Some(v.clone()),
            "Default" => t.default = parse_yes(v),
            "Forced" => t.forced = parse_yes(v),
            _ => {}
        }
    }
    t
}

fn parse_audio(fields: &[(String, String)]) -> AudioTrack {
    let mut id = None;
    let mut format = None;
    let mut commercial_name = None;
    let mut channels = None;
    let mut language_raw: Option<String> = None;
    let mut title: Option<String> = None;
    let mut default = false;
    let mut forced = false;

    for (k, v) in fields {
        match k.as_str() {
            "ID" => id = parse_u32(v),
            "Format" => format = Some(v.clone()),
            "Commercial name" => commercial_name = Some(v.clone()),
            "Channel(s)" => {
                channels = parse_unsigned_with_spaces(v).and_then(|n| u8::try_from(n).ok());
            }
            "Language" => language_raw = Some(v.clone()),
            "Title" => title = Some(v.clone()),
            "Default" => default = parse_yes(v),
            "Forced" => forced = parse_yes(v),
            _ => {}
        }
    }

    let language =
        Language::from_mediainfo(language_raw.as_deref().unwrap_or(""), title.as_deref());

    AudioTrack {
        id,
        format,
        commercial_name,
        channels,
        language,
        title,
        default,
        forced,
    }
}

fn parse_subtitle(fields: &[(String, String)]) -> SubtitleTrack {
    let mut id = None;
    let mut format = None;
    let mut language_raw: Option<String> = None;
    let mut title: Option<String> = None;
    let mut default = false;
    let mut forced = false;

    for (k, v) in fields {
        match k.as_str() {
            "ID" => id = parse_u32(v),
            "Format" => format = Some(v.clone()),
            "Language" => language_raw = Some(v.clone()),
            "Title" => title = Some(v.clone()),
            "Default" => default = parse_yes(v),
            "Forced" => forced = parse_yes(v),
            _ => {}
        }
    }

    let language =
        Language::from_mediainfo(language_raw.as_deref().unwrap_or(""), title.as_deref());

    SubtitleTrack {
        id,
        format,
        language,
        title,
        default,
        forced,
    }
}

// --- value parsers ---

fn parse_u32(s: &str) -> Option<u32> {
    s.trim().parse().ok()
}

fn parse_yes(s: &str) -> bool {
    s.trim().eq_ignore_ascii_case("Yes")
}

/// Parse de inteiros que o `MediaInfo` escreve com espaço como
/// separador de milhar: `"3 840 pixels"` → `Some(3840)`,
/// `"10 bits"` → `Some(10)`, `"2 channels"` → `Some(2)`.
///
/// Estratégia: extrai todos os dígitos ASCII consecutivos do começo
/// (ignorando espaços) até bater num não-dígito-não-espaço (a unidade).
fn parse_unsigned_with_spaces(s: &str) -> Option<u32> {
    let mut digits = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else if ch.is_whitespace() {
            // Whitespace funciona como separador de milhar
            // (e.g., `"3 840 pixels"`) ou como prefixo antes dos dígitos.
            // Em qualquer caso, segue tentando.
        } else {
            // unidade ou outro caractere — para
            break;
        }
    }
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// Parse `"2 h 16 min"` / `"45 min 30 s"` / `"3 h"` / `"7 s"` em
/// [`Duration`]. Aceita componentes `h`, `min`, `s` em qualquer ordem
/// (na prática o `MediaInfo` emite sempre h → min → s).
fn parse_duration(s: &str) -> Option<Duration> {
    let mut hours: u64 = 0;
    let mut mins: u64 = 0;
    let mut secs: u64 = 0;
    let mut tokens = s.split_whitespace();
    while let Some(num_tok) = tokens.next() {
        let unit = tokens.next()?;
        let value: u64 = num_tok.parse().ok()?;
        match unit {
            "h" => hours = value,
            "min" => mins = value,
            "s" => secs = value,
            _ => return None,
        }
    }
    if hours == 0 && mins == 0 && secs == 0 {
        return None;
    }
    Some(Duration::from_secs(hours * 3600 + mins * 60 + secs))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn empty_input_returns_empty_error() {
        assert!(matches!(parse(""), Err(ParseError::Empty)));
        assert!(matches!(parse("   \n  \r\n  "), Err(ParseError::Empty)));
    }

    #[test]
    fn input_with_only_unrecognized_section_returns_no_sections() {
        let dump = "RandomThing\nField : value\n";
        assert!(matches!(parse(dump), Err(ParseError::NoSections)));
    }

    #[test]
    fn single_general_section_parses() {
        let dump = "General\nFormat : Matroska\n";
        let parsed = parse(dump).expect("valid");
        assert_eq!(parsed.general.container_format.as_deref(), Some("Matroska"));
        assert!(parsed.audio.is_empty());
        assert!(parsed.video.is_empty());
        assert!(parsed.subtitles.is_empty());
    }

    #[test]
    fn audio_section_with_indexed_header() {
        let dump = "\
General
Format : Matroska

Audio #1
Format : AC-3
Channel(s) : 2 channels
Language : Portuguese (BR)
Default : Yes

Audio #2
Format : E-AC-3 JOC
Channel(s) : 6 channels
Language : English
";
        let parsed = parse(dump).expect("valid");
        assert_eq!(parsed.audio.len(), 2);
        assert_eq!(parsed.audio[0].language, Language::PtBr);
        assert_eq!(parsed.audio[0].channels, Some(2));
        assert!(parsed.audio[0].default);
        assert_eq!(parsed.audio[1].language, Language::En);
        assert_eq!(parsed.audio[1].channels, Some(6));
    }

    #[test]
    fn forced_subtitle_detected() {
        let dump = "\
General
Format : Matroska

Text
Language : Portuguese (BR)
Title : Brazilian (Forced)
Default : Yes
Forced : Yes
";
        let parsed = parse(dump).expect("valid");
        assert_eq!(parsed.subtitles.len(), 1);
        assert!(parsed.subtitles[0].forced);
        assert!(parsed.subtitles[0].default);
        assert_eq!(parsed.subtitles[0].language, Language::PtBr);
    }

    #[test]
    fn duration_parses_h_min() {
        assert_eq!(
            parse_duration("2 h 16 min"),
            Some(Duration::from_secs(2 * 3600 + 16 * 60))
        );
        assert_eq!(
            parse_duration("45 min 30 s"),
            Some(Duration::from_secs(45 * 60 + 30))
        );
        assert_eq!(parse_duration("3 h"), Some(Duration::from_secs(3 * 3600)));
    }

    #[test]
    fn duration_rejects_garbage() {
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("blah"), None);
        assert_eq!(parse_duration("2 hh 16 min"), None);
    }

    #[test]
    fn unsigned_with_thousand_separator() {
        assert_eq!(parse_unsigned_with_spaces("3 840 pixels"), Some(3840));
        assert_eq!(parse_unsigned_with_spaces("2 160 pixels"), Some(2160));
        assert_eq!(parse_unsigned_with_spaces("10 bits"), Some(10));
        assert_eq!(parse_unsigned_with_spaces("2 channels"), Some(2));
        assert_eq!(parse_unsigned_with_spaces("garbage"), None);
        assert_eq!(parse_unsigned_with_spaces(""), None);
    }

    #[test]
    fn yes_no_parsing_case_insensitive() {
        assert!(parse_yes("Yes"));
        assert!(parse_yes("yes"));
        assert!(parse_yes("  YES  "));
        assert!(!parse_yes("No"));
        assert!(!parse_yes(""));
        assert!(!parse_yes("anything"));
    }

    #[test]
    fn fields_without_values_skipped_cleanly() {
        // Campo com chave mas valor vazio depois do colon — não deve panicar
        let dump = "\
General
Format :
Complete name :

Audio
Language : English
";
        let parsed = parse(dump).expect("valid");
        // Vazia → string vazia (lenient)
        assert_eq!(parsed.general.container_format.as_deref(), Some(""));
        assert_eq!(parsed.audio[0].language, Language::En);
    }

    #[test]
    fn section_without_fields_yields_default_track() {
        let dump = "\
General
Format : Matroska

Audio
";
        let parsed = parse(dump).expect("valid");
        assert_eq!(parsed.audio.len(), 1);
        // Sem Language field → fallback Other("")
        assert_eq!(parsed.audio[0].language, Language::Other(String::new()));
        assert!(!parsed.audio[0].default);
        assert!(!parsed.audio[0].forced);
    }

    #[test]
    fn crlf_and_lf_both_work() {
        let lf = "General\nFormat : Matroska\n\nAudio\nLanguage : English\n";
        let crlf = "General\r\nFormat : Matroska\r\n\r\nAudio\r\nLanguage : English\r\n";
        assert_eq!(parse(lf).unwrap(), parse(crlf).unwrap());
    }

    #[test]
    fn orphan_field_at_eof_does_not_become_section() {
        // `ReportBy` line at end of mediainfo dumps appears as a key:value
        // line after the last section has been closed by an empty line.
        let dump = "\
General
Format : Matroska

ReportBy : MediaInfoLib - v23.10";
        let parsed = parse(dump).expect("valid");
        // ReportBy should not have created any section
        assert!(parsed.audio.is_empty());
        assert!(parsed.video.is_empty());
        assert!(parsed.subtitles.is_empty());
    }

    #[test]
    fn multiple_pt_br_audio_tracks_preserved_in_order() {
        let dump = "\
General
Format : Matroska

Audio #1
Language : Portuguese (BR)
Title : Dublagem original
Channel(s) : 2 channels

Audio #2
Language : Portuguese (BR)
Title : Redublagem
Channel(s) : 6 channels
";
        let parsed = parse(dump).expect("valid");
        assert_eq!(parsed.audio.len(), 2);
        assert!(parsed.audio.iter().all(|t| t.language == Language::PtBr));
        assert_eq!(parsed.audio[0].channels, Some(2));
        assert_eq!(parsed.audio[1].channels, Some(6));
    }
}
