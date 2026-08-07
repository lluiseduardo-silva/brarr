//! Guards against a class that no CSS rule backs.
//!
//! Tailwind used to catch this at build time: the `@theme` block reset
//! `--color-*` to `initial`, so a typo like `bg-slat-500` simply failed
//! to generate and the missing style was obvious. Hand-authored CSS has
//! no such step — an unknown class is silently inert, which is exactly
//! how `text-white` and `text-on-accent-solid` went unnoticed on 21
//! gradient buttons.
//!
//! This test is that step, moved into the suite.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on happy paths"
)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every class selector defined in `static/app.css`, unescaped.
fn defined_classes() -> BTreeSet<String> {
    let css = std::fs::read_to_string(crate_dir().join("static/app.css"))
        .expect("static/app.css must exist — it is the source of truth, not a build artifact");

    let mut out = BTreeSet::new();
    let bytes: Vec<char> = css.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != '.' {
            i += 1;
            continue;
        }
        // A class selector starts with `.` followed by a letter, `_` or
        // `-`; `.5rem` inside a declaration does not qualify.
        let Some(&next) = bytes.get(i + 1) else { break };
        if !(next.is_ascii_alphabetic() || next == '_' || next == '-' || next == '\\') {
            i += 1;
            continue;
        }
        let mut name = String::new();
        let mut j = i + 1;
        while j < bytes.len() {
            let c = bytes[j];
            if c == '\\' {
                // Escaped character — take the next one literally.
                if let Some(&escaped) = bytes.get(j + 1) {
                    name.push(escaped);
                    j += 2;
                    continue;
                }
                break;
            }
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                name.push(c);
                j += 1;
            } else {
                break;
            }
        }
        if !name.is_empty() {
            out.insert(name);
        }
        i = j.max(i + 1);
    }
    out
}

/// Remove every Askama tag (`{% … %}`, `{{ … }}`, `{# … #}`) from the
/// source.
///
/// This has to happen **before** attributes are located, not after. A
/// conditional class carries its own quotes — `class="{% if filter ==
/// "movie" %}…"` — so scanning for the closing quote of the attribute
/// first would stop inside the tag and truncate the class list.
fn strip_template_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(open) = rest.find('{') {
        let Some(kind) = rest[open + 1..].chars().next() else {
            break;
        };
        let close = match kind {
            '%' => "%}",
            '{' => "}}",
            '#' => "#}",
            _ => {
                // A lone brace: emit it and move on.
                out.push_str(&rest[..=open]);
                rest = &rest[open + 1..];
                continue;
            }
        };
        out.push_str(&rest[..open]);
        // Whitespace keeps two adjacent literals from fusing into one
        // bogus token when a tag sits between them.
        out.push(' ');
        if let Some(end) = rest[open..].find(close) {
            rest = &rest[open + end + close.len()..];
        } else {
            // Unterminated tag — nothing sane left to scan.
            rest = "";
            break;
        }
    }
    out.push_str(rest);
    out
}

/// Pull the tokens out of every `class="…"` / `class='…'` attribute of
/// already tag-stripped HTML.
fn used_classes(html: &str) -> BTreeSet<String> {
    let stripped = strip_template_tags(html);
    let mut out = BTreeSet::new();
    for (marker, quote) in [("class=\"", '"'), ("class='", '\'')] {
        let mut rest = stripped.as_str();
        while let Some(start) = rest.find(marker) {
            let after = &rest[start + marker.len()..];
            let Some(end) = after.find(quote) else { break };
            for token in after[..end].split_whitespace() {
                if is_utility_token(token) {
                    out.insert(token.to_owned());
                }
            }
            rest = &after[end..];
        }
    }
    out
}

/// Last-resort shape check for a token that survived tag stripping.
fn is_utility_token(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    if token.contains(['{', '}', '%', '(', ')', '<', '>', '=', '|', '"', '\'']) {
        return false;
    }
    let first = token.chars().next().unwrap_or(' ');
    if !(first.is_ascii_alphabetic() || first == '!' || first == '-') {
        return false;
    }
    // `it.monitored` vs `gap-0.5`: a dot is only legitimate before a digit.
    let chars: Vec<char> = token.chars().collect();
    for (idx, c) in chars.iter().enumerate() {
        if *c == '.' {
            match chars.get(idx + 1) {
                Some(next) if next.is_ascii_digit() => {}
                _ => return false,
            }
        }
    }
    true
}

/// Strip the leading `!` of an important-modified class and the variant
/// prefix (`hover:`, `md:`, `dark:`, …) is *not* stripped: those are
/// separate selectors in the stylesheet and must be defined in full.
fn normalise(token: &str) -> String {
    token.trim_start_matches('!').to_owned()
}

/// Whether `token` is the literal half of a class whose suffix comes from
/// Rust — `class="lib-status-{{ it.tone }}"` leaves `lib-status-` behind
/// once the tag is stripped.
///
/// Such a token is satisfied by *any* rule beginning with it. That is
/// weaker than an exact match, and deliberately so: it still catches a
/// typo in the stable half (`lib-statuss-` matches nothing), while the
/// variable half is checked where it is actually produced — see
/// `every_status_tone_has_a_rule`, which enumerates the real enums rather
/// than a list somebody has to remember to update.
fn is_interpolated_prefix(token: &str, defined: &BTreeSet<String>) -> bool {
    token.ends_with('-') && defined.iter().any(|class| class.starts_with(token))
}

fn collect_files(dir: &Path, extension: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, extension, out);
        } else if path.extension().is_some_and(|e| e == extension) {
            out.push(path);
        }
    }
}

fn collect_templates(dir: &Path, out: &mut Vec<PathBuf>) {
    collect_files(dir, "html", out);
}

#[test]
fn every_class_used_by_a_template_has_a_rule() {
    let defined = defined_classes();
    assert!(
        defined.len() > 100,
        "parsed only {} selectors out of app.css — the parser is broken, not the stylesheet",
        defined.len()
    );

    let mut templates = Vec::new();
    collect_templates(&crate_dir().join("templates"), &mut templates);
    assert!(!templates.is_empty(), "no templates found");

    let mut missing: Vec<String> = Vec::new();
    for path in &templates {
        let html = std::fs::read_to_string(path).unwrap();
        for token in used_classes(&html) {
            let class = normalise(&token);
            // `!` important variants resolve to the same rule name in
            // this stylesheet (`.\!bg-danger-soft`), so check both forms.
            if !defined.contains(&class)
                && !defined.contains(&token)
                && !is_interpolated_prefix(&class, &defined)
            {
                missing.push(format!(
                    "{}: {token}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ));
            }
        }
    }
    missing.sort();
    missing.dedup();

    assert!(
        missing.is_empty(),
        "templates use {} class(es) with no rule in static/app.css — they render as no-ops:\n  {}",
        missing.len(),
        missing.join("\n  ")
    );
}

/// The half `every_class_used_by_a_template_has_a_rule` cannot see.
///
/// `class="lib-status-{{ it.tone }}"` is checked there only up to the
/// prefix; the suffix comes from `coverage::ItemStatus::tone` and
/// `coverage::EpisodeState::tone`. This walks the real enums, so a new
/// variant with no rule fails here rather than rendering colourless in
/// production — which is exactly how the emerald badges shipped.
#[test]
fn every_status_tone_has_a_rule() {
    use brarr_orchestrator::coverage::{EpisodeState, ItemStatus};

    let defined = defined_classes();
    let mut missing = Vec::new();

    for status in [
        ItemStatus::Paused,
        ItemStatus::Nothing,
        ItemStatus::Upcoming,
        ItemStatus::Missing,
        ItemStatus::UpToDate,
        ItemStatus::Complete,
    ] {
        // Every place a tone is rendered — miss one and that surface
        // silently loses its colour while the others keep theirs.
        for prefix in ["lib-status-", "lib-bar-", "lib-spine-"] {
            let class = format!("{prefix}{}", status.tone());
            if !defined.contains(&class) {
                missing.push(class);
            }
        }
    }

    for state in [
        EpisodeState::Downloaded,
        EpisodeState::Downloading,
        EpisodeState::Missing,
        EpisodeState::Unaired,
        EpisodeState::Gone,
    ] {
        let class = format!("ep-mark-{}", state.tone());
        if !defined.contains(&class) {
            missing.push(class);
        }
    }

    assert!(
        missing.is_empty(),
        "these status classes are produced by Rust and have no rule in static/app.css:\n  {}",
        missing.join("\n  ")
    );
}

/// The templates are not the only source of markup. `web/routes.rs`
/// builds HTMX fragments as strings — the connectivity badges, the
/// profile-preview cards — and those classes went unchecked until now.
///
/// That gap was not hypothetical: every badge those handlers emitted
/// carried `bg-emerald-100 text-emerald-800`, a Tailwind palette scale
/// the hand-authored stylesheet never defined, so the pill rendered with
/// no colour at all from the migration onward.
#[test]
fn every_class_emitted_from_rust_has_a_rule() {
    let defined = defined_classes();

    let mut sources = Vec::new();
    collect_files(&crate_dir().join("src"), "rs", &mut sources);
    assert!(!sources.is_empty(), "no Rust sources found");

    let mut missing: Vec<String> = Vec::new();
    let mut seen = 0usize;
    for path in &sources {
        let source = std::fs::read_to_string(path).unwrap();
        for token in used_classes(&source) {
            seen += 1;
            let class = normalise(&token);
            if !defined.contains(&class) && !defined.contains(&token) {
                missing.push(format!(
                    "{}: {token}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ));
            }
        }
    }
    missing.sort();
    missing.dedup();

    // Without this the test passes by finding nothing at all, which is
    // exactly the failure mode it exists to prevent.
    assert!(
        seen > 20,
        "only {seen} classes extracted from Rust sources — the scanner is broken, not the code"
    );
    assert!(
        missing.is_empty(),
        "Rust-generated fragments use {} class(es) with no rule in static/app.css:\n  {}",
        missing.len(),
        missing.join("\n  ")
    );
}

#[test]
fn a_format_placeholder_is_not_mistaken_for_a_class() {
    // The exact shape of the badge fragments in `web/routes.rs`.
    let source = r##"format!(r#"<span class="badge {bg} {fg}" title="x">{label}</span>"#)"##;
    let found = used_classes(source);
    assert!(found.contains("badge"));
    assert_eq!(found.len(), 1, "got {found:?}");
}

#[test]
fn conditional_classes_yield_only_the_literal_branches() {
    // The exact shape used across the templates, quotes and all.
    let html = r#"<a class="px-3 {% if filter == "movie" %}bg-accent-solid{% else %}bg-bg-surface border{% endif %} h-7">x</a>"#;
    let found = used_classes(html);

    let mut expected = BTreeSet::new();
    for c in ["px-3", "bg-accent-solid", "bg-bg-surface", "border", "h-7"] {
        expected.insert(c.to_owned());
    }
    assert_eq!(
        found, expected,
        "both branches count, and no Askama keyword leaks through"
    );
}

#[test]
fn interpolations_and_comments_do_not_become_classes() {
    let html = r#"<div class="card {{ extra }} {# a note #} p-4"></div>"#;
    let found = used_classes(html);
    assert!(found.contains("card") && found.contains("p-4"));
    assert!(
        !found.contains("extra") && !found.contains("note"),
        "got {found:?}"
    );
}

#[test]
fn the_token_filter_keeps_decimals_and_variants() {
    for good in [
        "gap-0.5",
        "p-1.5",
        "md:col-span-2",
        "hover:bg-bg-muted",
        "!bg-danger-soft",
    ] {
        assert!(is_utility_token(good), "{good} should count as a class");
    }
    for bad in ["it.monitored", "filter.is_empty()", "=="] {
        assert!(!is_utility_token(bad), "{bad} should not count as a class");
    }
}
