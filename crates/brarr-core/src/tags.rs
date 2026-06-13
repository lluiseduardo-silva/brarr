//! Tokenizador best-effort de títulos de release.
//!
//! Extrai codec de vídeo, grupo de release e flags proper/repack/remux
//! direto do título — que está sempre disponível, ao contrário do
//! `MediaInfo`. Usa métodos de string (sem regex), no mesmo espírito dos
//! `guess_kind` / `lang_hints_from_title` dos converters de tracker.
//!
//! É deliberadamente conservador: na dúvida devolve `None`/`false` em vez
//! de adivinhar. O codec pode ser refinado depois pelo `MediaInfo` (mais
//! preciso) nos converters.

use crate::release::{ReleaseTags, VideoCodec};

/// Sufixos que casam o heurístico de grupo mas são, na verdade, pedaços
/// de termos de fonte (`Blu-Ray` → `Ray`, `WEB-DL` → `DL`, ...).
const GROUP_DENYLIST: &[&str] = &["ray", "dl", "disc", "rip"];

/// Parseia [`ReleaseTags`] de um título de release.
#[must_use]
pub fn parse_release_tags(title: &str) -> ReleaseTags {
    let norm = normalize(title);
    ReleaseTags {
        video_codec: detect_codec(&norm),
        release_group: detect_group(title),
        proper: contains_token(&norm, "proper"),
        repack: contains_token(&norm, "repack"),
        remux: contains_token(&norm, "remux"),
    }
}

/// Lowercase + separadores (`.` `_` `-` `[` `]` `(` `)`, espaços) viram um
/// único espaço, com padding nas pontas pra casar tokens por borda. Ex.:
/// `"Movie.x265-NeX"` → `" movie x265 nex "`.
fn normalize(title: &str) -> String {
    let mut out = String::with_capacity(title.len() + 2);
    out.push(' ');
    let mut prev_space = true;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    if !prev_space {
        out.push(' ');
    }
    out
}

/// `true` quando `token` aparece como token inteiro no título já
/// normalizado (delimitado por espaços).
fn contains_token(norm: &str, token: &str) -> bool {
    norm.contains(&format!(" {token} "))
}

fn detect_codec(norm: &str) -> Option<VideoCodec> {
    const H265: &[&str] = &["x265", "hevc", "h265", "h 265"];
    const H264: &[&str] = &["x264", "avc", "h264", "h 264"];
    if H265.iter().any(|t| contains_token(norm, t)) {
        Some(VideoCodec::H265)
    } else if H264.iter().any(|t| contains_token(norm, t)) {
        Some(VideoCodec::H264)
    } else if contains_token(norm, "av1") {
        Some(VideoCodec::Av1)
    } else {
        None
    }
}

/// Heurístico de grupo: o token após o último `-`, se parecer um grupo de
/// scene (sem espaços, 2..=20 chars alfanuméricos com ao menos uma letra,
/// fora da denylist de termos de fonte).
fn detect_group(title: &str) -> Option<String> {
    let candidate = title.rsplit('-').next()?.trim();
    if candidate.is_empty() || candidate == title.trim() {
        return None; // sem hífen ⇒ sem sufixo de grupo
    }
    let len = candidate.chars().count();
    let alnum_only = candidate.chars().all(|c| c.is_ascii_alphanumeric());
    let has_alpha = candidate.chars().any(|c| c.is_ascii_alphabetic());
    if !(2..=20).contains(&len) || !alnum_only || !has_alpha {
        return None;
    }
    if GROUP_DENYLIST
        .iter()
        .any(|d| d.eq_ignore_ascii_case(candidate))
    {
        return None;
    }
    Some(candidate.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bluray_remux_x265_with_group() {
        let t = parse_release_tags("Movie.Name.2024.1080p.BluRay.REMUX.x265-NeX");
        assert_eq!(t.video_codec, Some(VideoCodec::H265));
        assert_eq!(t.release_group.as_deref(), Some("NeX"));
        assert!(t.remux);
        assert!(!t.proper);
        assert!(!t.repack);
    }

    #[test]
    fn parses_proper_webdl_x264_with_group() {
        let t = parse_release_tags("Show.S01E02.PROPER.WEB-DL.x264-GRP");
        assert_eq!(t.video_codec, Some(VideoCodec::H264));
        assert_eq!(t.release_group.as_deref(), Some("GRP"));
        assert!(t.proper);
        assert!(!t.remux);
    }

    #[test]
    fn detects_av1_and_repack_without_group() {
        let t = parse_release_tags("Some Movie 2024 1080p WEB-DL AV1 REPACK");
        assert_eq!(t.video_codec, Some(VideoCodec::Av1));
        assert!(t.repack);
        assert_eq!(t.release_group, None);
    }

    #[test]
    fn hevc_spelled_out_and_dotted_h265() {
        assert_eq!(
            parse_release_tags("Filme 2160p HEVC Dual").video_codec,
            Some(VideoCodec::H265)
        );
        assert_eq!(
            parse_release_tags("Filme 1080p H.265 10bit").video_codec,
            Some(VideoCodec::H265)
        );
    }

    #[test]
    fn no_codec_when_absent() {
        assert_eq!(parse_release_tags("Movie 1080p WEB-DL").video_codec, None);
    }

    #[test]
    fn group_denylist_rejects_source_fragments() {
        // "Blu-Ray ..." would rsplit to "Ray ..." (spaces) → no group;
        // a bare "...-Ray" / "...-DL" suffix is denylisted.
        assert_eq!(
            parse_release_tags("Movie 1080p Blu-Ray").release_group,
            None
        );
        assert_eq!(parse_release_tags("Movie 1080p WEB-DL").release_group, None);
    }

    #[test]
    fn group_rejects_pure_numeric_suffix() {
        assert_eq!(parse_release_tags("Movie-2024").release_group, None);
    }

    #[test]
    fn empty_title_is_all_default() {
        let t = parse_release_tags("");
        assert_eq!(t, ReleaseTags::default());
    }
}
