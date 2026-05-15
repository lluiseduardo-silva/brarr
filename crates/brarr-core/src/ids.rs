//! Newtypes para IDs externos (TMDB, IMDB, TVDB, MAL).
//!
//! Cada um envolve um `u32` e rejeita `0` na construção, alinhado com
//! a convenção dessas plataformas (IDs começam em 1; `0` na API
//! UNIT3D significa "ID não informado", caso em que o campo na
//! [`Release`](crate::Release) deve ser `None`).

/// TMDB movie/TV/person ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TmdbId(u32);

/// Erro de construção de [`TmdbId`].
#[derive(Debug, thiserror::Error)]
#[error("TMDB ID cannot be zero")]
pub struct TmdbIdError;

impl TmdbId {
    /// Constrói um `TmdbId`, rejeitando zero.
    ///
    /// # Errors
    ///
    /// [`TmdbIdError`] se `value == 0`.
    pub const fn new(value: u32) -> Result<Self, TmdbIdError> {
        if value == 0 {
            Err(TmdbIdError)
        } else {
            Ok(Self(value))
        }
    }
    /// Valor inteiro nu.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// IMDB ID numérico (sem o prefixo `tt`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImdbId(u32);

/// Erro de construção de [`ImdbId`].
#[derive(Debug, thiserror::Error)]
#[error("IMDB ID cannot be zero")]
pub struct ImdbIdError;

impl ImdbId {
    /// Constrói um `ImdbId`, rejeitando zero.
    ///
    /// # Errors
    ///
    /// [`ImdbIdError`] se `value == 0`.
    pub const fn new(value: u32) -> Result<Self, ImdbIdError> {
        if value == 0 {
            Err(ImdbIdError)
        } else {
            Ok(Self(value))
        }
    }
    /// Valor inteiro nu (sem o prefixo `tt`).
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// `TheTVDB` ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TvdbId(u32);

/// Erro de construção de [`TvdbId`].
#[derive(Debug, thiserror::Error)]
#[error("TVDB ID cannot be zero")]
pub struct TvdbIdError;

impl TvdbId {
    /// Constrói um `TvdbId`, rejeitando zero.
    ///
    /// # Errors
    ///
    /// [`TvdbIdError`] se `value == 0`.
    pub const fn new(value: u32) -> Result<Self, TvdbIdError> {
        if value == 0 {
            Err(TvdbIdError)
        } else {
            Ok(Self(value))
        }
    }
    /// Valor inteiro nu.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// `MyAnimeList` ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MalId(u32);

/// Erro de construção de [`MalId`].
#[derive(Debug, thiserror::Error)]
#[error("MAL ID cannot be zero")]
pub struct MalIdError;

impl MalId {
    /// Constrói um `MalId`, rejeitando zero.
    ///
    /// # Errors
    ///
    /// [`MalIdError`] se `value == 0`.
    pub const fn new(value: u32) -> Result<Self, MalIdError> {
        if value == 0 {
            Err(MalIdError)
        } else {
            Ok(Self(value))
        }
    }
    /// Valor inteiro nu.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn tmdb_id_rejects_zero_accepts_nonzero() {
        assert!(TmdbId::new(0).is_err());
        let id = TmdbId::new(603).expect("valid");
        assert_eq!(id.get(), 603);
    }

    #[test]
    fn imdb_id_rejects_zero_accepts_nonzero() {
        assert!(ImdbId::new(0).is_err());
        let id = ImdbId::new(133_093).expect("valid");
        assert_eq!(id.get(), 133_093);
    }

    #[test]
    fn tvdb_id_rejects_zero() {
        assert!(TvdbId::new(0).is_err());
        assert!(TvdbId::new(1).is_ok());
    }

    #[test]
    fn mal_id_rejects_zero() {
        assert!(MalId::new(0).is_err());
        assert!(MalId::new(1).is_ok());
    }

    #[test]
    fn ids_are_copy_and_equality_works() {
        let a = TmdbId::new(603).expect("valid");
        let b = a; // Copy semantics
        assert_eq!(a, b);
    }
}
