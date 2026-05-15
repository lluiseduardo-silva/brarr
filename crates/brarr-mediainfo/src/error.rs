//! Erros estruturais retornados por [`crate::parse`].

/// Erros possíveis em [`parse`](crate::parse).
///
/// O parser é deliberadamente tolerante: campos com valor inválido viram
/// `None`/`false` no resultado e chaves desconhecidas são ignoradas.
/// `ParseError` cobre só os casos em que não dá pra produzir nada útil.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// Entrada estritamente vazia ou contendo só whitespace.
    #[error("MediaInfo input is empty")]
    Empty,

    /// Nenhuma seção reconhecida (`General`, `Video`, `Audio`, `Text`).
    /// Indica entrada que não parece um dump de `MediaInfo`.
    #[error(
        "no recognized sections in input — expected at least one of: General, Video, Audio, Text"
    )]
    NoSections,
}
