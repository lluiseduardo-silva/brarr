//! Resultado da avaliação de um [`Release`] contra um [`RuleSet`].
//!
//! [`Release`]: brarr_core::Release
//! [`RuleSet`]: crate::rule::RuleSet

use brarr_core::DecisionScore;

/// Resultado da avaliação. Carrega score acumulado, tags coletadas e
/// flag de rejeição.
///
/// Construído internamente pelo [`Engine::evaluate`](crate::Engine::evaluate);
/// consumidores leem os campos públicos. Não-`#[non_exhaustive]` de
/// propósito — `brarr-cli` constrói instâncias diretamente em testes,
/// e o conjunto de campos é estável dentro da Fase 6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionOutcome {
    /// Score final saturado em `DecisionScore::MAX`.
    pub score: DecisionScore,
    /// Tags anexadas pelas regras que casaram (ordem de avaliação).
    pub tags: Vec<String>,
    /// `true` se alguma regra com `reject = true` casou. Caller decide
    /// se filtra ou ainda exibe o release (com indicação visual).
    pub rejected: bool,
    /// Nomes das regras que casaram (úteis pra `-vv` debug).
    pub matched_rules: Vec<String>,
}

impl DecisionOutcome {
    /// Outcome neutro: score 0, sem tags, não rejeitado.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            score: DecisionScore::saturating(0),
            tags: Vec::new(),
            rejected: false,
            matched_rules: Vec::new(),
        }
    }
}

impl Default for DecisionOutcome {
    fn default() -> Self {
        Self::empty()
    }
}
