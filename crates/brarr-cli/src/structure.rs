//! Rendering for `brarr structure --dry-run`.
//!
//! Pure `&[RemoteStructureTitle] -> String`, no I/O, which is what makes
//! it testable — the same split [`crate::search::format_outcome`] uses.
//!
//! ## What the text form leaves out, and why
//!
//! Every title the server looked at comes back, including the ones with
//! no numbering to move — on this catalogue roughly 165 of 180. Printing
//! them would bury the dozen rows the report exists for under a page of
//! titles about which the answer is "nothing happens". They are counted
//! in the header instead, so the arithmetic still closes against the
//! catalogue and nothing is silently dropped.
//!
//! The JSON form filters nothing. It is what a second pass reads.

use std::fmt::Write as _;

use serde::Serialize;

use crate::remote::RemoteStructureTitle;

/// The report, for a terminal.
#[must_use]
pub fn format_structure(titles: &[RemoteStructureTitle]) -> String {
    let ready: Vec<&RemoteStructureTitle> =
        titles.iter().filter(|t| t.outcome == "ready").collect();
    let blocked: Vec<&RemoteStructureTitle> =
        titles.iter().filter(|t| t.outcome == "blocked").collect();
    let untouched = titles.len() - ready.len() - blocked.len();

    let mut out = String::new();
    let _ = writeln!(
        out,
        "Simulação de estrutura — {} título(s): {} pronto(s), {} bloqueado(s), \
         {} sem numeração a mover.",
        titles.len(),
        ready.len(),
        blocked.len(),
        untouched
    );
    let _ = writeln!(out, "Nada foi escrito.");

    if !ready.is_empty() {
        let width = column(&ready);
        let _ = writeln!(out, "\nPRONTOS");
        for t in &ready {
            write_ready(&mut out, t, width);
        }
    }

    if !blocked.is_empty() {
        let width = column(&blocked);
        let _ = writeln!(out, "\nBLOQUEADOS");
        for t in &blocked {
            let _ = writeln!(out, "  {:<width$}  {}", t.title, t.reason);
        }
    }

    // The one number an operator acts on before anything else: a title
    // whose plan would be refused is a title that needs a decision, not
    // a retry.
    let refused = ready.iter().filter(|t| !t.would_commit).count();
    if refused > 0 {
        let _ = writeln!(
            out,
            "\n{refused} título(s) pronto(s) seriam RECUSADOS pelos portões — \
             veja `órfãos` e `datas` acima."
        );
    }

    out
}

/// The width the title column needs, capped so one very long name does
/// not push every number off an 80-column terminal.
fn column(rows: &[&RemoteStructureTitle]) -> usize {
    rows.iter()
        .map(|t| t.title.chars().count())
        .max()
        .unwrap_or(0)
        .min(44)
}

fn write_ready(out: &mut String, t: &RemoteStructureTitle, width: usize) {
    let (source, family, handle) = t
        .destination
        .clone()
        .unwrap_or_else(|| ("?".to_owned(), "?".to_owned(), String::new()));
    let ordering = if handle.is_empty() {
        family
    } else {
        format!("{family}:{handle}")
    };
    let (stored, incoming) = t.air_date_coverage;

    let _ = writeln!(
        out,
        "  {:<width$}  {:<22}  {} pareados · {} órfão(s) · {} novo(s) · datas {:.0}%/{:.0}%{}{}",
        t.title,
        format!("{source}/{ordering}"),
        t.paired,
        t.orphans,
        t.added,
        stored * 100.0,
        incoming * 100.0,
        if t.pinned { " · fixado" } else { "" },
        if t.would_commit { "" } else { "  ← RECUSADO" },
    );

    if t.grabs_at_risk > 0 {
        let _ = writeln!(
            out,
            "  {:<width$}  {} aquisição(ões) perderiam o episódio",
            "", t.grabs_at_risk
        );
    }
    for (season, was, now, grabs) in &t.packs {
        let _ = writeln!(
            out,
            "  {:<width$}  pacote da temporada {season}: {was} → {now} episódio(s) \
             ({grabs} aquisição/ões)",
            ""
        );
    }
}

/// The report, for a pipe. One line, every title, nothing filtered.
///
/// # Errors
///
/// Returns the `serde_json` error if serialisation fails, which is
/// unreachable for this tree of owned scalars.
pub fn format_structure_json(titles: &[RemoteStructureTitle]) -> Result<String, serde_json::Error> {
    let doc = ReportJson {
        total: titles.len(),
        ready: titles.iter().filter(|t| t.outcome == "ready").count(),
        blocked: titles.iter().filter(|t| t.outcome == "blocked").count(),
        titles: titles.iter().map(TitleJson::from).collect(),
    };
    serde_json::to_string(&doc)
}

#[derive(Debug, Serialize)]
struct ReportJson {
    total: usize,
    ready: usize,
    blocked: usize,
    titles: Vec<TitleJson>,
}

/// Deliberately flat, like `ReleaseJson`: a report read by a script
/// should not need to walk a tree to answer "which titles are refused".
#[derive(Debug, Serialize)]
struct TitleJson {
    item_id: String,
    title: String,
    outcome: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    destination_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    destination_family: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    destination_handle: Option<String>,
    pinned: bool,
    paired: u32,
    orphans: u32,
    added: u32,
    grabs_at_risk: i64,
    stored_air_date_coverage: f32,
    incoming_air_date_coverage: f32,
    would_commit: bool,
    packs: Vec<PackJson>,
}

#[derive(Debug, Serialize)]
struct PackJson {
    season: i32,
    was: u32,
    now: u32,
    grabs: i64,
}

impl From<&RemoteStructureTitle> for TitleJson {
    fn from(t: &RemoteStructureTitle) -> Self {
        let (source, family, handle) = match &t.destination {
            Some((s, f, h)) => (
                Some(s.clone()),
                Some(f.clone()),
                (!h.is_empty()).then(|| h.clone()),
            ),
            None => (None, None, None),
        };
        Self {
            item_id: t.item_id.clone(),
            title: t.title.clone(),
            outcome: t.outcome.clone(),
            reason: t.reason.clone(),
            destination_source: source,
            destination_family: family,
            destination_handle: handle,
            pinned: t.pinned,
            paired: t.paired,
            orphans: t.orphans,
            added: t.added,
            grabs_at_risk: t.grabs_at_risk,
            stored_air_date_coverage: t.air_date_coverage.0,
            incoming_air_date_coverage: t.air_date_coverage.1,
            would_commit: t.would_commit,
            packs: t
                .packs
                .iter()
                .map(|(season, was, now, grabs)| PackJson {
                    season: *season,
                    was: *was,
                    now: *now,
                    grabs: *grabs,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert on happy paths"
)]
mod tests {
    use super::*;

    fn title(name: &str, outcome: &str) -> RemoteStructureTitle {
        RemoteStructureTitle {
            item_id: "00000000-0000-0000-0000-000000000001".to_owned(),
            title: name.to_owned(),
            outcome: outcome.to_owned(),
            reason: String::new(),
            destination: Some(("tvdb".to_owned(), "default".to_owned(), String::new())),
            pinned: false,
            paired: 59,
            orphans: 0,
            added: 0,
            grabs_at_risk: 0,
            air_date_coverage: (1.0, 1.0),
            would_commit: true,
            packs: Vec::new(),
        }
    }

    /// The 165 titles with nothing to move are counted, never listed —
    /// but the arithmetic has to close, or the report quietly loses
    /// titles and reads as a smaller catalogue.
    #[test]
    fn untouched_titles_are_counted_not_listed() {
        let rows = vec![
            title("Jujutsu Kaisen", "ready"),
            title("Os Simpsons", "untouched"),
            title("Breaking Bad", "untouched"),
        ];
        let text = format_structure(&rows);

        assert!(text.contains("3 título(s): 1 pronto(s), 0 bloqueado(s), 2 sem numeração"));
        assert!(text.contains("Jujutsu Kaisen"));
        assert!(!text.contains("Os Simpsons"));
        assert!(text.contains("Nada foi escrito."));
    }

    /// A plan the gates would refuse must be impossible to miss in a
    /// page of a hundred and eighty rows — it is the one line that needs
    /// a decision rather than a retry.
    #[test]
    fn a_refused_plan_is_called_out_twice() {
        let mut kaiju = title("Kaiju No. 8", "ready");
        kaiju.orphans = 2;
        kaiju.grabs_at_risk = 2;
        kaiju.would_commit = false;
        kaiju.air_date_coverage = (0.4, 1.0);

        let text = format_structure(&[kaiju]);
        assert!(text.contains("← RECUSADO"), "{text}");
        assert!(
            text.contains("1 título(s) pronto(s) seriam RECUSADOS"),
            "{text}"
        );
        assert!(
            text.contains("2 aquisição(ões) perderiam o episódio"),
            "{text}"
        );
        assert!(text.contains("datas 40%/100%"), "{text}");
    }

    /// Dragon Ball Super's season-1 pack narrowing from 131 to 14 is the
    /// correction, and the report is the only place it is ever said.
    #[test]
    fn a_pack_that_changes_meaning_is_printed_under_its_title() {
        let mut dbs = title("Dragon Ball Super", "ready");
        dbs.paired = 131;
        dbs.packs = vec![(1, 131, 14, 1)];

        let text = format_structure(&[dbs]);
        assert!(
            text.contains("pacote da temporada 1: 131 → 14 episódio(s)"),
            "{text}"
        );
    }

    /// A blocked title reports the reason, because the fix is never a
    /// retry — it is an id the title does not carry or a credential
    /// nobody configured.
    #[test]
    fn a_blocked_title_prints_its_reason() {
        let mut blocked = title("Alguma Série", "blocked");
        blocked.reason = "o título não guarda um id da TheTVDB".to_owned();

        let text = format_structure(&[blocked]);
        assert!(text.contains("BLOQUEADOS"), "{text}");
        assert!(text.contains("não guarda um id da TheTVDB"), "{text}");
    }

    /// One line, and nothing filtered out — the text form drops the
    /// untouched rows, and a script reading this must not have to guess
    /// which ones.
    #[test]
    fn the_json_form_is_one_line_and_keeps_every_title() {
        let rows = vec![
            title("Jujutsu Kaisen", "ready"),
            title("Os Simpsons", "untouched"),
        ];
        let json = format_structure_json(&rows).unwrap();

        assert!(!json.contains('\n'));
        assert!(json.contains("\"total\":2"));
        assert!(json.contains("Os Simpsons"));
    }
}
