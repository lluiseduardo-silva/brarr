//! [`DecisionScore`] — pontuação tipada de um release pelas regras de
//! ranqueamento, com invariante de faixa `0..=1000`.

/// Score numérico de um release dentro de um conjunto de regras.
///
/// Faixa garantida: `0..=1000`. Construtores rejeitam valores acima
/// do máximo ([`DecisionScore::new`]) ou saturam ([`DecisionScore::saturating`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DecisionScore(u32);

/// Erro de construção de [`DecisionScore`].
#[derive(Debug, thiserror::Error)]
#[error("decision score {value} exceeds maximum {max}")]
pub struct ScoreOutOfRange {
    /// Valor recebido.
    pub value: u32,
    /// Limite máximo aceito ([`DecisionScore::MAX`]).
    pub max: u32,
}

impl DecisionScore {
    /// Score mínimo (`0`).
    pub const MIN: u32 = 0;
    /// Score máximo (`1000`).
    pub const MAX: u32 = 1000;

    /// Score zero — release sem matches.
    #[must_use]
    pub const fn zero() -> Self {
        Self(0)
    }

    /// Constrói um score, falhando se exceder [`DecisionScore::MAX`].
    ///
    /// # Errors
    ///
    /// [`ScoreOutOfRange`] se `value > MAX`.
    pub const fn new(value: u32) -> Result<Self, ScoreOutOfRange> {
        if value > Self::MAX {
            Err(ScoreOutOfRange {
                value,
                max: Self::MAX,
            })
        } else {
            Ok(Self(value))
        }
    }

    /// Constrói um score saturando em [`DecisionScore::MAX`] —
    /// útil para somas defensivas em regras de scoring que podem
    /// estourar acidentalmente.
    #[must_use]
    pub const fn saturating(value: u32) -> Self {
        if value > Self::MAX {
            Self(Self::MAX)
        } else {
            Self(value)
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

    use super::DecisionScore;

    #[test]
    fn zero_constructor() {
        assert_eq!(DecisionScore::zero().get(), 0);
    }

    #[test]
    fn new_accepts_min_max_and_between() {
        assert_eq!(DecisionScore::new(0).expect("min").get(), 0);
        assert_eq!(DecisionScore::new(500).expect("mid").get(), 500);
        assert_eq!(DecisionScore::new(1000).expect("max").get(), 1000);
    }

    #[test]
    fn new_rejects_above_max() {
        let err = DecisionScore::new(1001).expect_err("should reject");
        assert_eq!(err.value, 1001);
        assert_eq!(err.max, DecisionScore::MAX);

        assert!(DecisionScore::new(u32::MAX).is_err());
    }

    #[test]
    fn saturating_clamps_overflow() {
        assert_eq!(DecisionScore::saturating(0).get(), 0);
        assert_eq!(DecisionScore::saturating(500).get(), 500);
        assert_eq!(DecisionScore::saturating(1000).get(), 1000);
        assert_eq!(DecisionScore::saturating(1001).get(), 1000);
        assert_eq!(DecisionScore::saturating(u32::MAX).get(), 1000);
    }

    #[test]
    fn ordering_is_total_and_monotonic() {
        let a = DecisionScore::new(100).expect("valid");
        let b = DecisionScore::new(200).expect("valid");
        let c = DecisionScore::new(200).expect("valid");
        assert!(a < b);
        assert_eq!(b, c);
        assert!(b <= c);
    }
}
