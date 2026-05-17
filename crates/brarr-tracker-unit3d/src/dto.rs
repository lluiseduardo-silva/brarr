//! DTOs internos (`pub(crate)`) que espelham a forma das respostas
//! JSON do UNIT3D antes da conversão para [`brarr_core::Release`].
//!
//! Manter os DTOs separados do tipo de domínio dá liberdade pra usar
//! desserializers tolerantes (anos como int OU string, booleans como
//! 0/1 OU true/false, IDs externos como número, null ou 0) sem
//! poluir o domínio.

use serde::Deserialize;

/// Envelope `{ "data": ... }` que o UNIT3D usa nas suas respostas.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Envelope<T> {
    pub data: T,
}

/// Um torrent retornado pelos endpoints `/api/torrents/filter` e
/// `/api/torrents/{id}`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Unit3dTorrent {
    /// Sempre `"torrent"` na prática — preservado pra futura inspeção.
    #[serde(rename = "type", default)]
    pub _kind: String,
    /// ID textual do torrent (opaco; UNIT3D usa string).
    pub id: String,
    /// Atributos detalhados.
    pub attributes: Unit3dAttributes,
}

/// Subconjunto dos campos de `attributes` que o brarr efetivamente
/// consome. Campos não listados são silenciosamente ignorados
/// (`#[serde(deny_unknown_fields)]` **não** está ativado, propositalmente
/// — diferentes builds de UNIT3D adicionam campos sem aviso).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Unit3dAttributes {
    /// Nome completo do release.
    pub name: String,

    /// Ano da obra. Capybara emite como número (`1999`), Locadora como
    /// string (`"1999"`); o desserializer aceita ambos e `null`.
    #[serde(default, deserialize_with = "deserialize_year_flexible")]
    pub release_year: Option<u16>,

    /// `type` do release (`"WEB-DL"`, `"BluRay"`, etc.). Renomeado
    /// porque `type` é palavra reservada.
    #[serde(rename = "type", default)]
    pub release_type: Option<String>,

    /// Resolução nominal (`"1080p"`, `"2160p"`, `"SD"`).
    #[serde(default)]
    pub resolution: Option<String>,

    /// Dump textual do `MediaInfo`. `None` quando o tracker não fornece.
    #[serde(default)]
    pub media_info: Option<String>,

    /// Tamanho total em bytes.
    #[serde(default)]
    pub size: Option<u64>,

    /// Seeders no momento da resposta.
    #[serde(default)]
    pub seeders: u32,
    /// Leechers no momento da resposta.
    #[serde(default)]
    pub leechers: u32,
    /// Downloads completos (campo `times_completed` do UNIT3D).
    #[serde(default)]
    pub times_completed: u32,

    /// TMDB ID. `0`, `null` ou string vazia viram `None`.
    #[serde(default, deserialize_with = "deserialize_optional_id")]
    pub tmdb_id: Option<u32>,
    /// IMDB ID numérico. `0`, `null` ou string vazia viram `None`.
    #[serde(default, deserialize_with = "deserialize_optional_id")]
    pub imdb_id: Option<u32>,
    /// `TheTVDB` ID. `0`, `null` ou string vazia viram `None`.
    #[serde(default, deserialize_with = "deserialize_optional_id")]
    pub tvdb_id: Option<u32>,
    /// `MyAnimeList` ID. `0`, `null` ou string vazia viram `None`.
    #[serde(default, deserialize_with = "deserialize_optional_id")]
    pub mal_id: Option<u32>,

    /// Página de detalhes no tracker.
    #[serde(default)]
    pub details_link: Option<String>,
    /// Link de download direto do `.torrent`.
    #[serde(default)]
    pub download_link: Option<String>,
    /// Link magnet.
    #[serde(default)]
    pub magnet_link: Option<String>,

    /// Timestamp de upload no tracker. UNIT3D serializa em ISO 8601
    /// (`"2024-01-15T12:34:56.000000Z"` ou `"2024-01-15 12:34:56"`).
    /// Mapeado para [`brarr_core::Release::published_at`] e usado pelo
    /// feed Torznab como `<pubDate>` para que Sonarr/Radarr mostrem a
    /// idade real do upload.
    #[serde(default)]
    pub created_at: Option<String>,
}

/// Desserializador para `release_year` que aceita número, string ou null.
fn deserialize_year_flexible<'de, D>(deserializer: D) -> Result<Option<u16>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    use serde_json::Value;

    match Value::deserialize(deserializer)? {
        Value::Null => Ok(None),
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                trimmed.parse::<u16>().map(Some).map_err(D::Error::custom)
            }
        }
        Value::Number(n) => n
            .as_u64()
            .and_then(|x| u16::try_from(x).ok())
            .map(Some)
            .ok_or_else(|| D::Error::custom("release_year out of u16 range")),
        other => Err(D::Error::custom(format!(
            "expected string, number, or null for release_year; got {other:?}"
        ))),
    }
}

/// Desserializador para IDs externos opcionais que aceita número,
/// string numérica, ou null. `0` é tratado como ausência (convenção
/// UNIT3D), virando `None`.
fn deserialize_optional_id<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    use serde_json::Value;

    match Value::deserialize(deserializer)? {
        Value::Null => Ok(None),
        Value::Number(n) => match n.as_u64() {
            Some(0) => Ok(None),
            Some(x) => u32::try_from(x)
                .map(Some)
                .map_err(|_| D::Error::custom("external id out of u32 range")),
            None => Err(D::Error::custom("expected non-negative integer for id")),
        },
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            match trimmed.parse::<u32>() {
                Ok(0) => Ok(None),
                Ok(x) => Ok(Some(x)),
                Err(e) => Err(D::Error::custom(e)),
            }
        }
        other => Err(D::Error::custom(format!(
            "expected number, string, or null for id; got {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::fs;
    use std::path::PathBuf;

    use super::Unit3dTorrent;

    fn fixture_path(name: &str) -> PathBuf {
        // tests/fixtures/<name> está colado ao crate raiz — caminho
        // relativo ao CARGO_MANIFEST_DIR.
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("docs")
            .join("requests-response-examples")
            .join(name)
    }

    fn load(name: &str) -> String {
        let path = fixture_path(name);
        fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("could not read fixture {}: {e}", path.display()))
    }

    #[test]
    fn deserialize_shadow_fixture() {
        let raw = load("shadow.json");
        let t: Unit3dTorrent = serde_json::from_str(&raw).expect("shadow should deserialize");
        assert_eq!(t.id, "125");
        let a = &t.attributes;
        assert_eq!(
            a.name,
            "Matrix 1999 2160p HMAX WEB-DL DDP5.1 Atmos HDR x265 DUAL-sh4down"
        );
        // shadow: release_year is a JSON number
        assert_eq!(a.release_year, Some(1999));
        assert_eq!(a.release_type.as_deref(), Some("WEB-DL"));
        assert_eq!(a.resolution.as_deref(), Some("2160p"));
        assert_eq!(a.size, Some(19_381_275_821));
        assert_eq!(a.seeders, 5);
        assert_eq!(a.leechers, 0);
        assert_eq!(a.times_completed, 31);
        assert_eq!(a.tmdb_id, Some(603));
        assert_eq!(a.imdb_id, Some(133_093));
        // shadow has tvdb/mal/igdb as null
        assert_eq!(a.tvdb_id, None);
        assert_eq!(a.mal_id, None);
        // media_info present and non-empty
        assert!(
            a.media_info
                .as_deref()
                .is_some_and(|s| s.starts_with("General"))
        );
    }

    #[test]
    fn deserialize_vnlls_fixture() {
        let raw = load("vnlls.json");
        let t: Unit3dTorrent = serde_json::from_str(&raw).expect("vnlls should deserialize");
        assert_eq!(t.id, "27582");
        let a = &t.attributes;
        // vnlls: release_year is a JSON string ("1999")
        assert_eq!(a.release_year, Some(1999));
        assert_eq!(a.resolution.as_deref(), Some("1080p"));
        assert_eq!(a.size, Some(9_608_016_733));
        assert_eq!(a.tmdb_id, Some(603));
        // vnlls has tvdb/mal/igdb as 0 (number) — desserializer coerces to None
        assert_eq!(a.tvdb_id, None);
        assert_eq!(a.mal_id, None);
    }

    #[test]
    fn year_deserializer_handles_all_shapes() {
        // null
        let v: Unit3dAttrsOnly =
            serde_json::from_str(r#"{"name":"x","release_year":null}"#).unwrap();
        assert_eq!(v.release_year, None);

        // number
        let v: Unit3dAttrsOnly =
            serde_json::from_str(r#"{"name":"x","release_year":2024}"#).unwrap();
        assert_eq!(v.release_year, Some(2024));

        // string
        let v: Unit3dAttrsOnly =
            serde_json::from_str(r#"{"name":"x","release_year":"2024"}"#).unwrap();
        assert_eq!(v.release_year, Some(2024));

        // missing field (#[serde(default)])
        let v: Unit3dAttrsOnly = serde_json::from_str(r#"{"name":"x"}"#).unwrap();
        assert_eq!(v.release_year, None);

        // empty string
        let v: Unit3dAttrsOnly = serde_json::from_str(r#"{"name":"x","release_year":""}"#).unwrap();
        assert_eq!(v.release_year, None);
    }

    #[test]
    fn id_deserializer_treats_zero_and_null_as_absent() {
        // 0 → None
        let v: Unit3dAttrsOnly = serde_json::from_str(r#"{"name":"x","tmdb_id":0}"#).unwrap();
        assert_eq!(v.tmdb_id, None);

        // null → None
        let v: Unit3dAttrsOnly = serde_json::from_str(r#"{"name":"x","tmdb_id":null}"#).unwrap();
        assert_eq!(v.tmdb_id, None);

        // missing → None
        let v: Unit3dAttrsOnly = serde_json::from_str(r#"{"name":"x"}"#).unwrap();
        assert_eq!(v.tmdb_id, None);

        // 603 → Some(603)
        let v: Unit3dAttrsOnly = serde_json::from_str(r#"{"name":"x","tmdb_id":603}"#).unwrap();
        assert_eq!(v.tmdb_id, Some(603));

        // "603" → Some(603)
        let v: Unit3dAttrsOnly = serde_json::from_str(r#"{"name":"x","tmdb_id":"603"}"#).unwrap();
        assert_eq!(v.tmdb_id, Some(603));
    }

    /// Atalho para testar só o struct de atributos sem montar o envelope.
    type Unit3dAttrsOnly = super::Unit3dAttributes;
}
