//! Guards the single door to the season tree.
//!
//! Rebuilding a series' tree re-points every acquisition hanging off it,
//! and `grabs.episode_id` is `ON DELETE SET NULL` — so a write that
//! prunes the wrong row unlinks a file with no error, no log, and a
//! screen that goes on rendering the series *complete*. `structure::apply`
//! is what asks who owns the shape before rewriting it, counts the
//! orphaned acquisitions on both sides of the transaction, and rolls back
//! on a rise.
//!
//! None of that helps if a second writer appears beside it. The pressure
//! is real and structural: `arr_import::sync_tree` calls the writer for
//! **every** series on **every** passive sweep, outside the `if created`
//! gate, every half hour. That is how the v0.13 damage reached a whole TV
//! library, and it is why "anything that rewrites a tree without asking
//! who owns it has a half-life of one cycle" is a design note rather than
//! a slogan.
//!
//! Rust can carry half of this: `db::library::sync_seasons` is
//! `#[cfg(test)]` and `write_tree` is `pub(crate)`, so nothing outside the
//! crate reaches either. It cannot carry the other half — `structure`
//! lives outside `db`, so `pub(in crate::db)` is not available, and a new
//! caller *inside* the crate would compile. This test is that half.
//!
//! Same shape and same reason as `css_coverage.rs`: a check the compiler
//! used to make, moved into the suite when the language stopped making
//! it.

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

/// Every `.rs` file under `src/`, which is the only place production code
/// lives. `migrations/` is deliberately out of scope: the identity
/// migration contains `UPDATE library_episodes`, and prose in `CLAUDE.md`
/// contains the phrases too.
fn production_sources() -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_rust(&crate_dir().join("src"), &mut out);
    assert!(!out.is_empty(), "no sources found under src/");
    out
}

/// The file with its line comments removed.
///
/// A guard that trips on its own explanation is a guard people work
/// around by rewording the explanation, so the doc comment in
/// `structure.rs` that *names* these statements must not read as one.
/// Only whole-line comments are stripped — `//`, `///`, `//!` — which is
/// enough here and cannot swallow a `//` inside a URL in real code.
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

/// The tree tables are written from exactly one module.
///
/// Scoped to `INSERT` and `DELETE`. `UPDATE` is deliberately **not**
/// checked: `park` rewrites `season_number`, and `set_season_monitored` /
/// `set_episode_monitored` write the operator's flags — those are
/// correctly outside the tree writer and always will be.
#[test]
fn only_db_library_writes_the_tree_tables() {
    const FORBIDDEN: [&str; 4] = [
        "INSERT INTO library_seasons",
        "INSERT INTO library_episodes",
        "DELETE FROM library_seasons",
        "DELETE FROM library_episodes",
    ];

    let mut offenders: Vec<String> = Vec::new();
    for path in production_sources() {
        let rel = relative(&path);
        if rel == "src/db/library.rs" {
            continue;
        }
        let source = without_comments(&std::fs::read_to_string(&path).unwrap());
        for needle in FORBIDDEN {
            if source.contains(needle) {
                offenders.push(format!("{rel}: {needle}"));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "the season tree is written outside src/db/library.rs:\n  {}\n\n\
         Every tree write must go through `structure::apply`, which checks the \
         recorded owner and brackets the write with the orphaned-grab count. A \
         second writer bypasses both, and the damage renders as a complete \
         library rather than a missing one.",
        offenders.join("\n  ")
    );
}

/// `write_tree` — the function that opens the transaction — has exactly
/// one caller outside `db::library` itself, and it is `structure::apply`.
#[test]
fn only_structure_apply_writes_the_tree() {
    let mut callers: Vec<String> = Vec::new();
    for path in production_sources() {
        let rel = relative(&path);
        if rel == "src/db/library.rs" {
            // Declaration site, plus the `#[cfg(test)]` seeding door.
            continue;
        }
        let source = without_comments(&std::fs::read_to_string(&path).unwrap());
        if source.contains("write_tree(") {
            callers.push(rel);
        }
    }
    callers.sort();

    assert_eq!(
        callers,
        vec!["src/structure.rs".to_owned()],
        "`db::library::write_tree` must have exactly one caller — `structure::apply`. \
         Found: {callers:?}"
    );
}

/// No production path reaches the test-only seeding door.
///
/// It resolves rows the way the writer did before `structure::pair`
/// existed and stamps no identity, so a production caller would quietly
/// give up the owner check, the orphan report and the air-date gate. The
/// `#[cfg(test)]` attribute already makes this a compile error; the test
/// states the rule so that removing the attribute is a visible decision
/// rather than a silent one.
#[test]
fn no_production_code_calls_the_seeding_door() {
    let mut callers: Vec<String> = Vec::new();
    for path in production_sources() {
        let source = without_comments(&std::fs::read_to_string(&path).unwrap());
        // Skip the file's own test module, which is what the door is for.
        let production = source
            .split("#[cfg(test)]")
            .next()
            .unwrap_or_default()
            .to_owned();
        if production.contains("sync_seasons(") {
            callers.push(relative(&path));
        }
    }

    assert!(
        callers.is_empty(),
        "`sync_seasons` is the test-only seeding door and production reaches it from: {callers:?}"
    );
}
