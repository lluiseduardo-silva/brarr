# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**brarr** is a Portuguese-language torrent search aggregator across UNIT3D-based trackers (locadora.cc, capybarabr.com, etc.). It scores releases by Portuguese audio/subtitle presence and quality, and will eventually expose a CLI, gRPC service, and Axum + Askama + HTMX + Tailwind admin web UI.

This is a personal Rust learning project. The author values quality, correctness, and architectural clarity over speed of delivery.

## Current State — Phase 1 complete (workspace + stubs)

Repository structure:
- Root `Cargo.toml` — virtual workspace (`resolver = "3"`, global `[workspace.lints.rust]` + `[workspace.lints.clippy]`, `[workspace.package]` for shared metadata).
- `crates/brarr-*` — 7 stub crates (5 libs + 2 bins). Each `lib.rs` / `main.rs` is a module-level doc comment describing intent + phase number.
- `rustfmt.toml`, `clippy.toml`, `.gitignore` — configured.
- `README.md` (PT) and `docs/ARCHITECTURE.md` — architecture documented.
- `INITIAL_PROMPT.md` — **the authoritative project spec (9.6 KB)**. Always consult before adding crates, dependencies, or making architectural decisions.

Next phase: **Phase 2 — `brarr-mediainfo` parser** (see `INITIAL_PROMPT.md`).

## Toolchain notes

- Active toolchain on this machine: `stable-x86_64-pc-windows-gnu` (rustc 1.95.0). Switched away from MSVC because `link.exe` was not installed; the GNU toolchain ships mingw-w64 ld.
- Cargo is at `C:\Users\pc\.cargo\bin\cargo.exe` and may not be on PowerShell's default PATH. Prepend with: `$env:Path = "C:\Users\pc\.cargo\bin;$env:Path"` at the start of each shell session.
- MSRV pinned to **1.85** (`edition = "2024"` requires it).

## Build / Test / Lint

```
cargo build --workspace
cargo test --workspace --all-targets
cargo test -p <crate> <test_name>          # single test
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

All four currently pass on `master` with zero warnings.

## Architecture (Planned crate layout)

Strict boundaries between concerns. Each crate has a single responsibility:

| Crate | Phase | Responsibility |
|-------|-------|---------------|
| `brarr-mediainfo` | 2 | Parse MediaInfo text dumps into structured types |
| `brarr-core` | 3 | Shared domain types (`Release`, `TrackerSource`, language enums) |
| `brarr-tracker-unit3d` | 4 | UNIT3D API client (HTTP + JSON deserialization) |
| `brarr-cli` | 5 | Command-line search tool |
| `brarr-orchestrator` | deferred | gRPC service + Axum/Askama/HTMX/Tailwind web UI |
| `brarr-decision-service` | deferred | Rules engine for release selection |
| `brarr-plugin-host` | deferred | WASM sandbox for custom scraper plugins |

Boundary rules:
- Parser ≠ HTTP client ≠ Rules engine ≠ Tracker impls. Never collapse layers.
- Traits (e.g., `TrackerProvider`) only when ≥2 implementations exist. No single-impl traits.
- Web UI is server-side rendered (Askama templates + HTMX). No SPA framework.

## Engineering Rules (non-negotiable, from spec)

- **TDD**: failing test first, using real fixtures in `tests/fixtures/`. Sample UNIT3D JSON responses (Matrix 1999 from two trackers) and MediaInfo dumps are drafted in `INITIAL_PROMPT.md` — use them, do not synthesize fake data.
- **No `unwrap()` / `expect()`** outside `#[cfg(test)]`. Propagate `Result`.
- **Errors**: `thiserror` in library crates, `anyhow` in binaries. No `Box<dyn Error>`.
- **Logging**: `tracing` only. No `println!` / `eprintln!` except CLI user output.
- **Types as documentation**: newtypes for IDs, enums for closed sets, `Option<T>` for optional. Avoid stringly-typed APIs.
- **No defensive `.clone()`** — prefer borrows; clone only when ownership genuinely required.
- **Language**: English identifiers and `///` docs. Portuguese only for user-facing strings.
- **Clippy**: `pedantic` lints enabled via `clippy.toml`.
- **Commits**: Conventional Commits (`feat:`, `fix:`, `refactor:`, `test:`, `docs:`, `chore:`).

## Workflow per task

1. Read the relevant section of `INITIAL_PROMPT.md`.
2. Confirm understanding before writing code.
3. Write a failing test with a real fixture.
4. Implement the minimum to pass.
5. Run `cargo test`, `cargo clippy`, `cargo fmt`.
6. Commit with a conventional message.

## Out of scope (per spec)

- Premature abstractions, single-implementation traits
- `Box<dyn Error>` in any production code
- Trading correctness for speed
- SPA / client-side framework for the admin UI
