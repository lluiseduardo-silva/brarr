//! [`Release`], [`ReleaseKind`], [`Resolution`] e tipos auxiliares
//! ([`ExternalIds`], [`ReleaseUrls`]) — a representação de domínio de
//! um torrent encontrado num tracker.

use time::OffsetDateTime;
use url::Url;

use crate::enrichment::ReleaseEnrichment;
use crate::ids::{ImdbId, MalId, TmdbId, TvdbId};
use crate::tracker::TrackerSource;

/// Um release (torrent) encontrado num tracker.
///
/// Modelo de domínio — não é um DTO de API. O cliente UNIT3D
/// (`brarr-tracker-unit3d`, Fase 4) tem seus próprios structs de
/// desserialização que convertem para `Release` via `From`/`TryFrom`.
///
/// Invariantes garantidos via [`Release::new`]:
/// - `title` não vazio
/// - `tracker_release_id` não vazio
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Release {
    /// ID opaco fornecido pelo tracker (e.g., `"125"` no capybara,
    /// `"27582"` no locadora). Único dentro de um [`TrackerSource`].
    pub tracker_release_id: String,
    /// De onde veio o release.
    pub tracker: TrackerSource,
    /// Nome completo do release como o tracker reporta.
    pub title: String,
    /// Ano de lançamento da obra (não do upload).
    pub year: Option<u16>,
    /// Tipo da fonte (WEB-DL, `BluRay`, encode, etc.).
    pub kind: ReleaseKind,
    /// Resolução nominal.
    pub resolution: Resolution,
    /// Tamanho total em bytes.
    pub size_bytes: u64,
    /// Seeders no momento da captura.
    pub seeders: u32,
    /// Leechers no momento da captura.
    pub leechers: u32,
    /// Quantidade de downloads completos (`times_completed` no UNIT3D).
    pub snatches: u32,
    /// IDs externos (TMDB, IMDB, etc.) quando disponíveis.
    pub external_ids: ExternalIds,
    /// URLs associadas (detalhes, download, magnet) quando o tracker as expõe.
    pub urls: ReleaseUrls,
    /// Resumo do conteúdo de mídia, derivado do `MediaInfo` quando o
    /// tracker o fornece. `None` quando o tracker não inclui ou o
    /// parser falhou.
    pub enrichment: Option<ReleaseEnrichment>,
    /// Quando o tracker publicou o release (não a data da obra — esse
    /// fica em [`Release::year`]). Mapeia de `usenetdate`/`pubDate` no
    /// Newznab e `created_at` no UNIT3D. `None` quando o tracker não
    /// expôs o campo. Usado pelo feed Torznab outbound como `<pubDate>`
    /// para que clientes *arr enxerguem a idade real do upload em vez
    /// de "Age: 0 minutes".
    pub published_at: Option<OffsetDateTime>,
    /// Tags derivadas do título (e refinadas pelo `MediaInfo` quando
    /// disponível): codec de vídeo, grupo de release e flags
    /// proper/repack/remux. Preenchidas pelos converters via
    /// [`crate::parse_release_tags`].
    pub tags: ReleaseTags,
}

/// Erros de construção de [`Release`].
#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    /// Título vazio (incluindo só whitespace).
    #[error("release title cannot be empty")]
    EmptyTitle,
    /// ID do tracker vazio.
    #[error("tracker_release_id cannot be empty")]
    EmptyTrackerReleaseId,
}

impl Release {
    /// Constrói um `Release` com os campos obrigatórios; demais
    /// (year, contadores, IDs externos, URLs, enrichment) começam
    /// com valor "vazio" e devem ser preenchidos por struct update.
    ///
    /// # Errors
    ///
    /// - [`ReleaseError::EmptyTitle`] se `title` for vazio após trim.
    /// - [`ReleaseError::EmptyTrackerReleaseId`] se o ID for vazio.
    pub fn new(
        tracker_release_id: impl Into<String>,
        tracker: TrackerSource,
        title: impl Into<String>,
        kind: ReleaseKind,
        resolution: Resolution,
        size_bytes: u64,
    ) -> Result<Self, ReleaseError> {
        let tracker_release_id = tracker_release_id.into();
        let title = title.into();
        if tracker_release_id.trim().is_empty() {
            return Err(ReleaseError::EmptyTrackerReleaseId);
        }
        if title.trim().is_empty() {
            return Err(ReleaseError::EmptyTitle);
        }
        Ok(Self {
            tracker_release_id,
            tracker,
            title,
            year: None,
            kind,
            resolution,
            size_bytes,
            seeders: 0,
            leechers: 0,
            snatches: 0,
            external_ids: ExternalIds::default(),
            urls: ReleaseUrls::default(),
            enrichment: None,
            published_at: None,
            tags: ReleaseTags::default(),
        })
    }
}

/// Codec de vídeo do release, detectado do título ou do `MediaInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    /// H.264 / AVC (também conhecido como x264).
    H264,
    /// H.265 / HEVC (também conhecido como x265).
    H265,
    /// AV1.
    Av1,
    /// Outro/desconhecido — preserva a string crua (e.g. `"VP9"`).
    Other(String),
}

impl VideoCodec {
    /// Normaliza o `Video format` do `MediaInfo` (e.g. `"HEVC"`, `"AVC"`,
    /// `"AV1"`) num [`VideoCodec`]. Desconhecido cai em
    /// [`VideoCodec::Other`] com a string `trim`ada.
    #[must_use]
    pub fn from_mediainfo_format(raw: &str) -> Self {
        let norm = raw.trim();
        match norm.to_ascii_uppercase().as_str() {
            "HEVC" | "H265" | "H.265" | "X265" => Self::H265,
            "AVC" | "H264" | "H.264" | "X264" => Self::H264,
            "AV1" => Self::Av1,
            _ => Self::Other(norm.to_string()),
        }
    }
}

/// Tags derivadas do título (e refinadas pelo `MediaInfo`): codec de
/// vídeo, grupo de release e flags proper/repack/remux. Tudo
/// best-effort — campos ausentes quando não detectados.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ReleaseTags {
    /// Codec de vídeo, quando detectado.
    pub video_codec: Option<VideoCodec>,
    /// Grupo de release (sufixo `-GRUPO`), quando detectado.
    pub release_group: Option<String>,
    /// `true` se o título marca `PROPER`.
    pub proper: bool,
    /// `true` se o título marca `REPACK`.
    pub repack: bool,
    /// `true` se o título marca `REMUX` (refina `ReleaseKind::BluRay`).
    pub remux: bool,
}

/// Tipo de fonte do release.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReleaseKind {
    /// `WEB-DL` — captura direta de plataforma de streaming.
    WebDl,
    /// `BluRay` (remux ou full disc).
    BluRay,
    /// Encode (x264/x265) derivado de outra fonte.
    Encode,
    /// `HDTV` (broadcast).
    HdTv,
    /// `DVD`.
    Dvd,
    /// Valor desconhecido — preserva a string crua do tracker.
    Other(String),
}

impl ReleaseKind {
    /// Parse do campo `type` da API UNIT3D.
    ///
    /// Valores conhecidos:
    /// - `"WEB-DL"`
    /// - `"BluRay"`
    /// - `"Encode"`
    /// - `"HDTV"`
    /// - `"DVD"`
    ///
    /// Qualquer outro valor cai em [`ReleaseKind::Other`] preservando
    /// o original `trim`ado.
    #[must_use]
    pub fn from_unit3d_type(raw: &str) -> Self {
        match raw.trim() {
            "WEB-DL" => Self::WebDl,
            "BluRay" => Self::BluRay,
            "Encode" => Self::Encode,
            "HDTV" => Self::HdTv,
            "DVD" => Self::Dvd,
            other => Self::Other(other.to_string()),
        }
    }
}

/// Resolução nominal do release.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Resolution {
    /// SD (qualquer coisa abaixo de 720p).
    Sd,
    /// 720p.
    P720,
    /// 1080p.
    P1080,
    /// 2160p / 4K.
    P2160,
    /// Valor desconhecido (e.g., `"480p"`, `"1440p"`) — preserva o original.
    Other(String),
}

impl Resolution {
    /// Parse do campo `resolution` da API UNIT3D.
    #[must_use]
    pub fn from_unit3d(raw: &str) -> Self {
        match raw.trim() {
            "SD" => Self::Sd,
            "720p" => Self::P720,
            "1080p" => Self::P1080,
            "2160p" => Self::P2160,
            other => Self::Other(other.to_string()),
        }
    }
}

/// IDs externos de uma obra.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct ExternalIds {
    /// TMDB ID.
    pub tmdb: Option<TmdbId>,
    /// IMDB ID (numérico, sem `tt`).
    pub imdb: Option<ImdbId>,
    /// `TheTVDB` ID.
    pub tvdb: Option<TvdbId>,
    /// `MyAnimeList` ID.
    pub mal: Option<MalId>,
}

/// URLs associadas a um release.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct ReleaseUrls {
    /// Página de detalhes no tracker.
    pub details: Option<Url>,
    /// Download direto do `.torrent` (geralmente requer token).
    pub download: Option<Url>,
    /// Magnet link.
    pub magnet: Option<Url>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{Release, ReleaseError, ReleaseKind, Resolution};
    use crate::tracker::TrackerSource;
    use url::Url;

    fn tracker() -> TrackerSource {
        TrackerSource::new(
            "test",
            Url::parse("https://example.com/api").expect("valid"),
        )
        .expect("valid")
    }

    #[test]
    fn release_new_accepts_valid() {
        let r = Release::new(
            "27582",
            tracker(),
            "The Matrix 1999 1080p",
            ReleaseKind::WebDl,
            Resolution::P1080,
            9_608_016_733,
        )
        .expect("valid");
        assert_eq!(r.title, "The Matrix 1999 1080p");
        assert_eq!(r.tracker_release_id, "27582");
        assert_eq!(r.size_bytes, 9_608_016_733);
        assert_eq!(r.seeders, 0);
        assert!(r.enrichment.is_none());
    }

    #[test]
    fn release_new_rejects_empty_title() {
        assert!(matches!(
            Release::new("1", tracker(), "", ReleaseKind::WebDl, Resolution::P1080, 0),
            Err(ReleaseError::EmptyTitle),
        ));
        assert!(matches!(
            Release::new(
                "1",
                tracker(),
                "   ",
                ReleaseKind::WebDl,
                Resolution::P1080,
                0
            ),
            Err(ReleaseError::EmptyTitle),
        ));
    }

    #[test]
    fn release_new_rejects_empty_tracker_id() {
        assert!(matches!(
            Release::new(
                "",
                tracker(),
                "Some title",
                ReleaseKind::WebDl,
                Resolution::P1080,
                0,
            ),
            Err(ReleaseError::EmptyTrackerReleaseId),
        ));
    }

    #[test]
    fn release_kind_parses_known_values() {
        assert_eq!(ReleaseKind::from_unit3d_type("WEB-DL"), ReleaseKind::WebDl);
        assert_eq!(ReleaseKind::from_unit3d_type("BluRay"), ReleaseKind::BluRay);
        assert_eq!(ReleaseKind::from_unit3d_type("Encode"), ReleaseKind::Encode);
        assert_eq!(ReleaseKind::from_unit3d_type("HDTV"), ReleaseKind::HdTv);
        assert_eq!(ReleaseKind::from_unit3d_type("DVD"), ReleaseKind::Dvd);
    }

    #[test]
    fn release_kind_falls_back_to_other_for_unknown() {
        assert_eq!(
            ReleaseKind::from_unit3d_type("CAM"),
            ReleaseKind::Other("CAM".to_string()),
        );
        // trim aplicado antes do match
        assert_eq!(
            ReleaseKind::from_unit3d_type("  WEB-DL  "),
            ReleaseKind::WebDl,
        );
    }

    #[test]
    fn resolution_parses_known_values() {
        assert_eq!(Resolution::from_unit3d("SD"), Resolution::Sd);
        assert_eq!(Resolution::from_unit3d("720p"), Resolution::P720);
        assert_eq!(Resolution::from_unit3d("1080p"), Resolution::P1080);
        assert_eq!(Resolution::from_unit3d("2160p"), Resolution::P2160);
    }

    #[test]
    fn resolution_falls_back_to_other_for_unknown() {
        assert_eq!(
            Resolution::from_unit3d("1440p"),
            Resolution::Other("1440p".to_string()),
        );
    }
}
