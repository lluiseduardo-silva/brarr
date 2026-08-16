//! Guards the two boundaries that make adding a metadata provider a
//! finite, enumerable job.
//!
//! `CLAUDE.md` states the rule — "strict boundaries, never collapse
//! layers" — and until now nothing enforced it. There were three live
//! violations when the metadata refactor started: `brarr_tmdb::
//! EpisodeGroup` inside `db::episode_numbering`, `EpisodeGroupKind`
//! inside `web/routes.rs`, and `brarr_tmdb::image_url` at four call
//! sites there. Each one meant a provider's own vocabulary decided the
//! shape of a table or a template, so a second provider could not be
//! added without rewriting both.
//!
//! The other half is the enum. `MetadataSource` is deliberately closed
//! and deliberately not `#[non_exhaustive]`, because exhaustive `match`
//! is this repository's dominant safety mechanism and the whole cost of
//! adding a provider should be a list of compiler errors. A single
//! `_ =>` arm anywhere converts that list into silence — and silence is
//! how the last two defects in this area shipped green: a `Source`
//! variant valid in Rust and inert in a `SQLite` `CHECK`, and a CSS
//! class valid in a template and absent from the stylesheet.
//!
//! Same shape and same reason as `css_coverage.rs` and
//! `tree_writer_coverage.rs`: a check the compiler used to make, moved
//! into the suite when the language stopped making it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on happy paths"
)]

use std::path::{Path, PathBuf};

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn collect_rust(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rust(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn sources_under(relative_dir: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_rust(&crate_dir().join(relative_dir), &mut out);
    assert!(!out.is_empty(), "no sources found under {relative_dir}");
    out
}

/// The file with its line comments removed.
///
/// A guard that trips on its own explanation is a guard people work
/// around by rewording the explanation — and the module docs in this
/// crate name these types constantly, on purpose, because naming what
/// is forbidden is how the rule survives.
fn without_comments(source: &str) -> String {
    source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn relative(path: &Path) -> String {
    path.strip_prefix(crate_dir())
        .unwrap_or(path)
        .display()
        .to_string()
        .replace('\\', "/")
}

/// No provider crate's types reach the data layer or the web layer.
///
/// The one permitted crossing is the attribution text, and it is
/// permitted for a reason worth stating: both metadata licences are
/// conditioned on displaying an exact phrase, the phrase belongs to the
/// provider, and it must appear whether or not that provider's
/// credential happens to be configured. So the constant is imported into
/// `web/mod.rs`'s attribution `match` — in the orchestrator, from the
/// provider's crate — rather than copied into `brarr-core`, which would
/// put one vendor's licence text in the foundational crate every other
/// crate depends on.
#[test]
fn no_provider_crate_type_crosses_into_db_or_web() {
    const FORBIDDEN: [&str; 2] = ["brarr_tmdb::", "brarr_tvdb::"];
    // Where the licence text legitimately lands.
    const ATTRIBUTION_SITE: &str = "src/web/mod.rs";

    let mut offenders: Vec<String> = Vec::new();
    for path in sources_under("src/db")
        .into_iter()
        .chain(sources_under("src/web"))
    {
        let rel = relative(&path);
        let source = without_comments(&std::fs::read_to_string(&path).unwrap());
        for needle in FORBIDDEN {
            for line in source.lines().filter(|l| l.contains(needle)) {
                let is_attribution = rel == ATTRIBUTION_SITE
                    && (line.contains("ATTRIBUTION") || line.contains("ATTRIBUTION_URL"));
                if !is_attribution {
                    offenders.push(format!("{rel}: {}", line.trim()));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "a provider crate's types reach the data or web layer:\n  {}\n\n\
         A provider-shaped value in a table or a template is a second provider's \
         migration. Route it through `brarr_core::metadata` — the neutral \
         vocabulary exists for exactly this — or through `metadata::art` / \
         `metadata::registry`, which are where the per-provider `match` lives.",
        offenders.join("\n  ")
    );
}

/// Nothing matches `MetadataSource` with a catch-all arm.
///
/// This is what makes `cargo build` enumerate the work of adding a
/// provider, and it is the reason the enum is closed rather than a
/// validated newtype. A `_ =>` arm is not a style preference to lose:
/// it is the difference between a compiler error list and a provider
/// that is configured, dispatched to, and silently absent from an
/// attribution footer, an image URL builder or a credential form.
///
/// Deliberately conservative — it flags a wildcard anywhere in a `match`
/// whose scrutinee or arms name the type, and the fix is always to write
/// the arms out. A genuine need for a default would be a signal that the
/// enum should not be closed, which is a decision worth making loudly.
#[test]
fn no_wildcard_arm_over_metadata_source() {
    let mut offenders: Vec<String> = Vec::new();
    let mut files = sources_under("src");
    files.extend(collect_core());

    for path in files {
        let rel = relative(&path);
        let source = without_comments(&std::fs::read_to_string(&path).unwrap());
        for block in match_blocks(&source) {
            // Only this match's own arms — a nested `match` inside an arm
            // body is its own question, and reading it here is how the
            // first draft of this guard accused two innocent handlers.
            let arms = top_level_arms(&block);
            if arms.contains("MetadataSource::") && has_wildcard_arm(&arms) {
                offenders.push(format!("{rel}: {}", first_line(&block)));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "a `match` over MetadataSource has a catch-all arm:\n  {}\n\n\
         Write the arms out. The enum is closed so that adding a provider is a \
         list of compiler errors; a wildcard turns that list into a provider \
         that is configured and silently missing from whatever this decides.",
        offenders.join("\n  ")
    );
}

/// `brarr-core`'s metadata module, which declares the enum and is where
/// a wildcard would do the most damage.
fn collect_core() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let core = crate_dir()
        .parent()
        .map(|p| p.join("brarr-core").join("src").join("metadata"));
    if let Some(dir) = core {
        collect_rust(&dir, &mut out);
    }
    out
}

/// Every `match … { … }` in the file, as source text.
///
/// Brace-counted rather than parsed: a real parser is the right tool and
/// a dependency this suite does not have, and the failure mode of
/// counting is a block that ends early — which can only produce a false
/// *pass*, never a false accusation.
///
/// **Every offset here is a byte offset**, and that has to stay true.
/// The first version walked `source.chars().collect::<Vec<char>>()` by
/// element index while `open` and `start` came from `str::find`, which
/// answers in bytes. The two agree only while the file is pure ASCII, so
/// the guard quietly read a shifted, truncated block out of any file
/// containing an accent — and this repository writes its operator-facing
/// strings in Portuguese. It surfaced as a panic on a char boundary,
/// which was lucky: the same mismatch one byte the other way is a block
/// that ends early, and this guard's own docs note that ending early can
/// only produce a false pass.
fn match_blocks(source: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut idx = 0;
    while let Some(found) = source[idx..].find("match ") {
        let start = idx + found;
        let Some(open) = source[start..].find('{').map(|o| start + o) else {
            break;
        };
        let mut depth = 0_i32;
        let mut end = open;
        for (offset, ch) in source[open..].char_indices().map(|(o, c)| (open + o, c)) {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = offset;
                        break;
                    }
                }
                _ => {}
            }
        }
        if end > open {
            blocks.push(source[start..=end].to_owned());
        }
        idx = start + "match ".len();
    }
    blocks
}

/// The arm patterns of one `match`, with every nested block removed.
///
/// A nested `match` inside an arm body is a separate question, and its
/// wildcard says nothing about this one. Reading the whole block is what
/// made the first draft of this guard accuse two handlers that were
/// matching on a media kind two levels down.
fn top_level_arms(block: &str) -> String {
    let mut out = String::new();
    let mut depth = 0_i32;
    for ch in block.chars() {
        match ch {
            '{' => depth += 1,
            '}' => depth -= 1,
            _ if depth == 1 => out.push(ch),
            _ => {}
        }
    }
    out
}

/// A `_` arm at the top level of a match.
///
/// `..` and a binding like `other =>` are deliberately not flagged: the
/// first is a struct pattern, and the second names what it ignores,
/// which the compiler still checks for exhaustiveness against the arms
/// around it only when the enum is matched by value — so it is a weaker
/// smell than a bare `_` and not one this guard should decide.
fn has_wildcard_arm(arms: &str) -> bool {
    arms.split("=>").any(|segment| {
        segment
            .rsplit(&[',', '\n'][..])
            .next()
            .is_some_and(|last| last.trim() == "_")
    })
}

fn first_line(block: &str) -> String {
    block.lines().next().unwrap_or_default().trim().to_owned()
}
