//! Erros do cliente HTTP UNIT3D.

use crate::convert::ConversionError;

/// Erros possíveis em chamadas de [`Unit3dClient`](crate::Unit3dClient).
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Token continha caracteres não-ASCII e não pôde virar header `Authorization`.
    #[error("invalid token: must be ASCII")]
    InvalidToken,

    /// `reqwest::Client::builder()` falhou (config TLS quebrada, etc.).
    #[error("HTTP client builder failed: {0}")]
    ClientBuild(#[source] reqwest::Error),

    /// Falha ao montar a URL absoluta a partir de `base_url` + path do endpoint.
    #[error("could not build endpoint URL: {0}")]
    BadUrl(#[from] url::ParseError),

    /// Erro de transporte ou status `4xx`/`5xx` (via `error_for_status`).
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// JSON inválido ou DTO incompatível com a forma esperada.
    #[error("response did not match expected JSON shape: {0}")]
    BadJson(#[source] serde_json::Error),

    /// Conversão DTO → [`brarr_core::Release`] falhou.
    #[error("DTO conversion failed: {0}")]
    Conversion(#[from] ConversionError),
}
