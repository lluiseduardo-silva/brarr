//! Carrega [`RuleSet`] de TOML.
//!
//! ## Exemplo de TOML
//!
//! ```toml
//! [[rule]]
//! name = "PT-BR audio"
//! add_score = 100
//! when = { audio = "pt-br" }
//!
//! [[rule]]
//! name = "Jackpot 4K HDR PT-BR"
//! add_score = 500
//! tag = "jackpot"
//! when = { audio = "pt-br", hdr = true, resolution = "min-2160" }
//!
//! [[rule]]
//! name = "Reject sem PT"
//! reject = true
//! tag = "sem PT"
//! when = {}
//! ```

use crate::rule::RuleSet;

/// Erros ao parsear regras a partir de TOML.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// TOML mal-formado ou campos com tipos inesperados.
    #[error("invalid rules TOML: {0}")]
    Toml(#[from] toml::de::Error),
}

/// Faz `parse` do `input` como TOML e devolve um [`RuleSet`].
///
/// # Errors
///
/// - [`LoadError::Toml`] se o input não for TOML válido ou se algum
///   campo não bater com o schema esperado.
pub fn from_toml(input: &str) -> Result<RuleSet, LoadError> {
    let rules: RuleSet = toml::from_str(input)?;
    Ok(rules)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::from_toml;
    use crate::rule::{AudioFilter, ResolutionFilter, SubtitleFilter};

    #[test]
    fn parses_simple_pt_br_rule() {
        let toml_str = r#"
[[rule]]
name = "PT-BR"
add_score = 100
when = { audio = "pt-br" }
"#;
        let rs = from_toml(toml_str).expect("parse");
        assert_eq!(rs.rules.len(), 1);
        let r = &rs.rules[0];
        assert_eq!(r.name.as_deref(), Some("PT-BR"));
        assert_eq!(r.add_score, 100);
        assert_eq!(r.when.audio, Some(AudioFilter::PtBr));
    }

    #[test]
    fn parses_compound_condition() {
        let toml_str = r#"
[[rule]]
name = "Jackpot"
add_score = 500
tag = "jackpot"
when = { audio = "pt-br", hdr = true, resolution = "min-2160", subtitle = "pt-any" }
"#;
        let rs = from_toml(toml_str).expect("parse");
        let r = &rs.rules[0];
        assert_eq!(r.when.audio, Some(AudioFilter::PtBr));
        assert_eq!(r.when.hdr, Some(true));
        assert_eq!(r.when.resolution, Some(ResolutionFilter::At2160));
        assert_eq!(r.when.subtitle, Some(SubtitleFilter::PtAny));
        assert_eq!(r.tag.as_deref(), Some("jackpot"));
    }

    #[test]
    fn parses_exact_resolution_filter() {
        let toml_str = r#"
[[rule]]
when = { resolution = "exact-1080" }
"#;
        let rs = from_toml(toml_str).expect("parse");
        assert_eq!(
            rs.rules[0].when.resolution,
            Some(ResolutionFilter::Exact1080)
        );
    }

    #[test]
    fn parses_reject_rule_with_empty_when() {
        let toml_str = r#"
[[rule]]
reject = true
tag = "blocked"
when = {}
"#;
        let rs = from_toml(toml_str).expect("parse");
        assert!(rs.rules[0].reject);
        assert_eq!(rs.rules[0].tag.as_deref(), Some("blocked"));
    }

    #[test]
    fn rejects_unknown_field_in_when() {
        let toml_str = r#"
[[rule]]
when = { audio = "pt-br", invalid_field = "x" }
"#;
        assert!(from_toml(toml_str).is_err());
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let toml_str = r"
[[rule]]
typo_score = 100
";
        assert!(from_toml(toml_str).is_err());
    }

    #[test]
    fn empty_input_yields_empty_ruleset() {
        let rs = from_toml("").expect("parse");
        assert!(rs.rules.is_empty());
    }

    #[test]
    fn parses_min_seeders_and_tracker_filters() {
        let toml_str = r#"
[[rule]]
add_score = 5
when = { min_seeders = 10, tracker = "capybara" }
"#;
        let rs = from_toml(toml_str).expect("parse");
        let c = &rs.rules[0].when;
        assert_eq!(c.min_seeders, Some(10));
        assert_eq!(c.tracker.as_deref(), Some("capybara"));
    }

    #[test]
    fn parses_max_size_bytes() {
        let toml_str = r"
[[rule]]
when = { max_size_bytes = 10000000000 }
reject = true
";
        let rs = from_toml(toml_str).expect("parse");
        assert_eq!(rs.rules[0].when.max_size_bytes, Some(10_000_000_000));
    }
}
