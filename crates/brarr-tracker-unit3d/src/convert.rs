//! Conversão `Unit3dTorrent` → [`brarr_core::Release`].
//!
//! Bridge entre a forma "wire" do UNIT3D e o tipo de domínio. Integra
//! [`brarr_mediainfo::parse`] para popular `Release::enrichment` quando
//! o tracker fornece o dump de `MediaInfo`. Falha no parse de `MediaInfo`
//! é **não-fatal**: a Release é criada com `enrichment: None` em vez
//! de toda a conversão abortar.

use brarr_core::{
    ImdbId, MalId, OffsetDateTime, Release, ReleaseError, ReleaseKind, Resolution, TmdbId,
    TrackerSource, TvdbId,
};
use brarr_mediainfo::parse;
use time::PrimitiveDateTime;
use time::macros::format_description;
use url::Url;

use crate::dto::Unit3dTorrent;

/// Erros possíveis ao converter um `Unit3dTorrent` em [`Release`].
#[derive(Debug, thiserror::Error)]
pub enum ConversionError {
    /// O struct [`Release`] rejeitou os dados (título ou ID vazios).
    #[error("invalid release fields: {0}")]
    Release(#[from] ReleaseError),

    /// URL inválida em algum dos campos `details_link`, `download_link`,
    /// `magnet_link`. Note que magnets caem aqui porque `url::Url` aceita
    /// `magnet:` perfeitamente — só algo estruturalmente malformado falha.
    #[error("invalid URL for {field}: {source}")]
    InvalidUrl {
        /// Nome do campo (e.g., `"details_link"`).
        field: &'static str,
        /// Erro original do parser de URL.
        #[source]
        source: url::ParseError,
    },
}

impl Unit3dTorrent {
    /// Converte o DTO em [`Release`] do domínio.
    ///
    /// `tracker` é injetado pelo cliente que sabe de onde veio a resposta;
    /// o DTO não carrega essa informação.
    ///
    /// # Errors
    ///
    /// - [`ConversionError::Release`] se o título for vazio (o ID do
    ///   torrent vem do campo `id` do envelope, sempre não-vazio em
    ///   respostas UNIT3D válidas).
    /// - [`ConversionError::InvalidUrl`] se algum dos campos de URL for
    ///   uma string presente mas não-parseable como URL.
    pub(crate) fn into_release(self, tracker: TrackerSource) -> Result<Release, ConversionError> {
        let Self {
            id,
            attributes: attr,
            ..
        } = self;

        let kind = attr.release_type.as_deref().map_or_else(
            || ReleaseKind::Other(String::new()),
            ReleaseKind::from_unit3d_type,
        );
        let resolution = attr
            .resolution
            .as_deref()
            .map_or_else(|| Resolution::Other(String::new()), Resolution::from_unit3d);
        let size_bytes = attr.size.unwrap_or(0);

        let mut release = Release::new(id, tracker, attr.name, kind, resolution, size_bytes)?;

        release.year = attr.release_year;
        release.seeders = attr.seeders;
        release.leechers = attr.leechers;
        release.snatches = attr.times_completed;

        release.external_ids.tmdb = attr.tmdb_id.and_then(|n| TmdbId::new(n).ok());
        release.external_ids.imdb = attr.imdb_id.and_then(|n| ImdbId::new(n).ok());
        release.external_ids.tvdb = attr.tvdb_id.and_then(|n| TvdbId::new(n).ok());
        release.external_ids.mal = attr.mal_id.and_then(|n| MalId::new(n).ok());

        release.urls.details = parse_optional_url(attr.details_link.as_deref(), "details_link")?;
        release.urls.download = parse_optional_url(attr.download_link.as_deref(), "download_link")?;
        release.urls.magnet = parse_optional_url(attr.magnet_link.as_deref(), "magnet_link")?;

        // MediaInfo parsing é best-effort: se falhar, deixa enrichment None.
        // Trackers eventualmente devolvem dumps truncados ou em formato
        // inesperado; isso não deve quebrar a busca inteira.
        release.enrichment = attr
            .media_info
            .as_deref()
            .and_then(|raw| parse(raw).ok())
            .map(|parsed| parsed.to_enrichment());

        // `created_at` é o timestamp do upload no tracker — alimenta
        // o `<pubDate>` do feed Torznab pra que Sonarr/Radarr mostrem
        // a idade real do upload em vez de "Age: 0 minutes". Parse
        // best-effort: se o formato variar, deixa `None`.
        release.published_at = attr.created_at.as_deref().and_then(parse_unit3d_timestamp);

        Ok(release)
    }
}

/// Parse uma string ISO 8601 emitida pelo UNIT3D em
/// [`OffsetDateTime`].
///
/// Formas vistas nos fixtures:
/// - `"2024-04-06T23:28:08.000000Z"` (shadow / capybara)
/// - `"2024-01-11T13:33:46.000000Z"` (vnlls / locadora)
///
/// Algumas builds de UNIT3D ainda emitem a forma "space"
/// (`"2024-04-06 23:28:08"`) em endpoints específicos; a função tenta
/// ambos antes de desistir.
fn parse_unit3d_timestamp(raw: &str) -> Option<OffsetDateTime> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // ISO 8601 com microssegundos + sufixo `Z` (UTC). `[offset_hour
    // sign:mandatory]` cobriria `+00:00`, mas UNIT3D ancora em `Z`,
    // então casamos o literal direto.
    let iso_z = format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond]Z");
    if let Ok(dt) = PrimitiveDateTime::parse(trimmed, iso_z) {
        return Some(dt.assume_utc());
    }
    // Variante sem subsegundos: `"2024-04-06T23:28:08Z"`.
    let iso_z_no_sub = format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");
    if let Ok(dt) = PrimitiveDateTime::parse(trimmed, iso_z_no_sub) {
        return Some(dt.assume_utc());
    }
    // Variante com espaço (sem `T` e sem `Z`).
    let space = format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    if let Ok(dt) = PrimitiveDateTime::parse(trimmed, space) {
        return Some(dt.assume_utc());
    }
    None
}

/// Parseia um `Option<&str>` em `Option<Url>`. String vazia vira `None`.
fn parse_optional_url(
    s: Option<&str>,
    field: &'static str,
) -> Result<Option<Url>, ConversionError> {
    s.filter(|x| !x.is_empty())
        .map(|x| Url::parse(x).map_err(|source| ConversionError::InvalidUrl { field, source }))
        .transpose()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::fs;
    use std::path::PathBuf;

    use brarr_core::{Language, ReleaseKind, Resolution, TmdbId, TrackerSource};
    use url::Url;

    use super::Unit3dTorrent;

    fn fixture(name: &str) -> String {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("docs")
            .join("requests-response-examples")
            .join(name);
        fs::read_to_string(&p).unwrap_or_else(|e| panic!("read fixture {}: {e}", p.display()))
    }

    fn capybara() -> TrackerSource {
        TrackerSource::new(
            "capybara",
            Url::parse("https://capybarabr.com/").expect("valid"),
        )
        .expect("non-empty name")
    }

    fn locadora() -> TrackerSource {
        TrackerSource::new(
            "locadora",
            Url::parse("https://locadora.cc/").expect("valid"),
        )
        .expect("non-empty name")
    }

    #[test]
    fn shadow_to_release_has_enrichment_with_pt_br_audio_and_hdr() {
        let raw = fixture("shadow.json");
        let dto: Unit3dTorrent = serde_json::from_str(&raw).expect("deserialize");
        let release = dto.into_release(capybara()).expect("convert");

        assert_eq!(release.tracker_release_id, "125");
        assert_eq!(release.tracker.name, "capybara");
        assert_eq!(
            release.title,
            "Matrix 1999 2160p HMAX WEB-DL DDP5.1 Atmos HDR x265 DUAL-sh4down"
        );
        assert_eq!(release.year, Some(1999));
        assert_eq!(release.kind, ReleaseKind::WebDl);
        assert_eq!(release.resolution, Resolution::P2160);
        assert_eq!(release.size_bytes, 19_381_275_821);
        assert_eq!(release.seeders, 5);
        assert_eq!(release.snatches, 31);

        assert_eq!(release.external_ids.tmdb, TmdbId::new(603).ok());
        assert_eq!(
            release.external_ids.imdb,
            brarr_core::ImdbId::new(133_093).ok()
        );
        // tvdb / mal were null in JSON → None
        assert_eq!(release.external_ids.tvdb, None);
        assert_eq!(release.external_ids.mal, None);

        // URLs parsed
        assert!(release.urls.details.is_some());
        assert!(release.urls.download.is_some());
        assert_eq!(release.urls.magnet, None);

        // Enrichment present and reflects MediaInfo: HDR + PT-BR audio
        let e = release.enrichment.as_ref().expect("enrichment populated");
        assert!(e.has_hdr, "shadow has HDR HEVC");
        assert!(e.has_audio_in(&Language::PtBr));
        assert!(e.has_audio_in(&Language::En));
        assert!(e.has_forced_subs);
    }

    #[test]
    fn vnlls_to_release_handles_string_year_and_zero_external_ids() {
        let raw = fixture("vnlls.json");
        let dto: Unit3dTorrent = serde_json::from_str(&raw).expect("deserialize");
        let release = dto.into_release(locadora()).expect("convert");

        assert_eq!(release.tracker_release_id, "27582");
        assert_eq!(release.tracker.name, "locadora");
        // vnlls fixture has release_year as a string "1999"
        assert_eq!(release.year, Some(1999));
        assert_eq!(release.kind, ReleaseKind::WebDl);
        assert_eq!(release.resolution, Resolution::P1080);
        assert_eq!(release.size_bytes, 9_608_016_733);
        assert_eq!(release.seeders, 1);

        // vnlls has tvdb_id / mal_id / igdb_id as 0 (number) — should be None
        assert_eq!(release.external_ids.tvdb, None);
        assert_eq!(release.external_ids.mal, None);
        // tmdb_id is real
        assert_eq!(release.external_ids.tmdb, TmdbId::new(603).ok());

        // SDR (no HDR field in MediaInfo)
        let e = release.enrichment.as_ref().expect("enrichment populated");
        assert!(!e.has_hdr);
        // PT-BR + PT-PT distinct in subs
        assert_eq!(e.subtitle_count_in(&Language::PtBr), 2);
        assert_eq!(e.subtitle_count_in(&Language::PtPt), 1);
    }

    #[test]
    fn missing_media_info_yields_none_enrichment_but_still_succeeds() {
        let json = r#"
        {
          "type": "torrent",
          "id": "42",
          "attributes": {
            "name": "Some release 1080p WEB-DL",
            "release_year": 2024,
            "type": "WEB-DL",
            "resolution": "1080p",
            "size": 1234,
            "seeders": 0,
            "leechers": 0,
            "times_completed": 0
          }
        }"#;
        let dto: Unit3dTorrent = serde_json::from_str(json).expect("deserialize");
        let release = dto.into_release(capybara()).expect("convert");
        assert_eq!(release.title, "Some release 1080p WEB-DL");
        assert!(release.enrichment.is_none());
    }

    #[test]
    fn unknown_release_kind_falls_through_to_other() {
        let json = r#"
        {
          "type": "torrent",
          "id": "1",
          "attributes": {
            "name": "Funky source release",
            "type": "CAM",
            "resolution": "480p",
            "size": 0,
            "seeders": 0, "leechers": 0, "times_completed": 0
          }
        }"#;
        let dto: Unit3dTorrent = serde_json::from_str(json).expect("deserialize");
        let release = dto.into_release(capybara()).expect("convert");
        assert_eq!(release.kind, ReleaseKind::Other("CAM".to_string()));
        assert_eq!(release.resolution, Resolution::Other("480p".to_string()));
    }

    #[test]
    fn shadow_published_at_parses_iso8601_with_microseconds() {
        // shadow.json fixture: `"created_at": "2024-04-06T23:28:08.000000Z"`.
        let raw = fixture("shadow.json");
        let dto: Unit3dTorrent = serde_json::from_str(&raw).expect("deserialize");
        let release = dto.into_release(capybara()).expect("convert");
        let ts = release.published_at.expect("created_at parsed");
        // 2024-04-06 23:28:08 UTC = 1712446088
        assert_eq!(ts.unix_timestamp(), 1_712_446_088);
    }

    #[test]
    fn missing_created_at_yields_none_published_at() {
        let json = r#"
        {
          "type": "torrent",
          "id": "42",
          "attributes": {
            "name": "x",
            "type": "WEB-DL",
            "resolution": "1080p",
            "size": 1,
            "seeders": 0, "leechers": 0, "times_completed": 0
          }
        }"#;
        let dto: Unit3dTorrent = serde_json::from_str(json).expect("deserialize");
        let release = dto.into_release(capybara()).expect("convert");
        assert!(release.published_at.is_none());
    }

    #[test]
    fn empty_title_rejected() {
        let json = r#"
        {
          "type": "torrent",
          "id": "1",
          "attributes": {
            "name": "",
            "type": "WEB-DL",
            "resolution": "1080p",
            "size": 0,
            "seeders": 0, "leechers": 0, "times_completed": 0
          }
        }"#;
        let dto: Unit3dTorrent = serde_json::from_str(json).expect("deserialize");
        let err = dto.into_release(capybara()).expect_err("empty title");
        assert!(matches!(
            err,
            super::ConversionError::Release(brarr_core::ReleaseError::EmptyTitle),
        ));
    }
}
