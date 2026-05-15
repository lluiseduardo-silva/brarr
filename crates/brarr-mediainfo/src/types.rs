//! Estruturas de saída produzidas por [`crate::parse`].

use std::time::Duration;

use crate::language::Language;

/// Resultado completo do parsing — todas as seções relevantes de um
/// dump de `MediaInfo`, distribuídas por tipo.
///
/// O parser preserva a **ordem de aparição** das faixas (`Audio #1`
/// antes de `Audio #2`, etc.), porque essa ordem é semanticamente
/// significativa: ela indica qual faixa é a padrão de fato no container
/// quando não há outro sinal explícito.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedMediaInfo {
    /// Seção `General` do dump. Se ausente, vem com todos os campos `None`.
    pub general: GeneralInfo,
    /// Faixas de vídeo, na ordem de aparição.
    pub video: Vec<VideoTrack>,
    /// Faixas de áudio, na ordem de aparição (`Audio` ou `Audio #N`).
    pub audio: Vec<AudioTrack>,
    /// Legendas, na ordem de aparição (`Text` ou `Text #N`).
    pub subtitles: Vec<SubtitleTrack>,
}

/// Informações do container/arquivo como um todo.
///
/// Marcado como `#[non_exhaustive]` porque novos campos serão
/// adicionados em fases seguintes (bitrate, framerate, container CRC,
/// etc.) sem quebrar consumidores.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct GeneralInfo {
    /// Container (`Matroska`, `MPEG-4`, etc.).
    pub container_format: Option<String>,
    /// Caminho/nome completo do arquivo conforme reportado.
    pub complete_name: Option<String>,
    /// Duração total. `None` se ausente ou inválida.
    pub duration: Option<Duration>,
    /// Tamanho do arquivo conforme reportado (e.g., `"8.95 GiB"`).
    /// O parse para bytes fica deliberadamente para o consumidor — o
    /// `MediaInfo` usa unidades arredondadas que perdem precisão.
    pub file_size_raw: Option<String>,
}

/// Uma faixa de vídeo.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct VideoTrack {
    /// Campo `ID` do `MediaInfo`.
    pub id: Option<u32>,
    /// Codec (`HEVC`, `AVC`, ...).
    pub format: Option<String>,
    /// Largura em pixels.
    pub width: Option<u32>,
    /// Altura em pixels.
    pub height: Option<u32>,
    /// Profundidade de cor (e.g., 8, 10).
    pub bit_depth: Option<u8>,
    /// Descritor de HDR cru (e.g., `"SMPTE ST 2086, HDR10 compatible"`).
    /// `None` indica SDR / ausência do campo.
    pub hdr_format: Option<String>,
    /// `Default: Yes` na faixa.
    pub default: bool,
    /// `Forced: Yes` na faixa.
    pub forced: bool,
}

/// Uma faixa de áudio.
///
/// Não implementa `Default` porque [`Language`] não tem um default
/// significativo — construa explicitamente pelos campos.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct AudioTrack {
    /// Campo `ID` do `MediaInfo`.
    pub id: Option<u32>,
    /// Codec (`E-AC-3`, `E-AC-3 JOC`, `AC-3`, `DTS`, ...).
    pub format: Option<String>,
    /// Nome comercial (e.g., `"Dolby Digital Plus with Dolby Atmos"`).
    pub commercial_name: Option<String>,
    /// Número de canais (e.g., 2 para estéreo, 6 para 5.1).
    pub channels: Option<u8>,
    /// Idioma normalizado a partir do par `(Language, Title)`.
    pub language: Language,
    /// Título cru (e.g., `"Brazilian Portuguese"`, `"English"`). Útil
    /// para preservar nuance perdida na normalização do idioma.
    pub title: Option<String>,
    /// `Default: Yes` na faixa.
    pub default: bool,
    /// `Forced: Yes` na faixa.
    pub forced: bool,
}

/// Uma faixa de legenda.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct SubtitleTrack {
    /// Campo `ID` do `MediaInfo`.
    pub id: Option<u32>,
    /// Formato (`UTF-8`, `PGS`, ...).
    pub format: Option<String>,
    /// Idioma normalizado.
    pub language: Language,
    /// Título cru (e.g., `"Forced"`, `"Brazilian (Forced)"`, `"SDH"`).
    pub title: Option<String>,
    /// `Default: Yes` na faixa.
    pub default: bool,
    /// `Forced: Yes` na faixa.
    pub forced: bool,
}
