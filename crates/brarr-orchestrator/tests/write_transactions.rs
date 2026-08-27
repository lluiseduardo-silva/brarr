//! Every write transaction has to take `SQLite`'s write lock at `BEGIN`.
//!
//! `Pool::begin` emits a bare `BEGIN`, which acquires nothing until the
//! transaction's first statement. brarr's write transactions all read
//! before they write, so under a deferred `BEGIN` they open as readers
//! and then ask to be promoted — and `SQLite` refuses a promotion whose
//! snapshot went stale **without invoking the busy handler**, so the
//! configured `busy_timeout` never applies and the call fails instantly
//! with `database is locked`. See `crate::db::begin_write`.
//!
//! This is a guard for the same reason `css_coverage` and
//! `metadata_boundary` are: the mistake compiles, passes every test, and
//! only shows up as intermittent lost work under concurrency in
//! production. Nothing about `pool.begin()` looks wrong at the call site.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on happy paths"
)]

use std::path::Path;

/// No `.begin()` anywhere in the orchestrator's sources.
///
/// Deliberately the whole method rather than `pool.begin()` in
/// particular: the receiver is usually a variable, so matching on its
/// name would be matching on a spelling. Nothing in the crate wants a
/// deferred transaction today, and a savepoint would be a decision worth
/// stopping on rather than a line that slips through.
#[test]
fn no_write_transaction_starts_deferred() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();

    for file in rust_files(&src) {
        let text = std::fs::read_to_string(&file).expect("read source");
        for (index, line) in text.lines().enumerate() {
            // Prose may name the method it is warning about.
            if line.trim_start().starts_with("//") {
                continue;
            }
            if line.contains(".begin()") {
                let shown = file
                    .strip_prefix(&src)
                    .unwrap_or(&file)
                    .display()
                    .to_string()
                    .replace(std::path::MAIN_SEPARATOR, "/");
                offenders.push(format!("  src/{shown}:{}: {}", index + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these open a deferred transaction, which fails its promotion to a \
         writer instead of waiting — use `crate::db::begin_write` (BEGIN \
         IMMEDIATE):\n{}",
        offenders.join("\n")
    );
}

/// Every `.rs` under `dir`, recursively.
fn rust_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(rust_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    out
}
