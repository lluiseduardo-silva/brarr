# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**brarr** is a Portuguese-language torrent search aggregator across UNIT3D-based trackers (locadora.cc, capybarabr.com, etc.). It scores releases by Portuguese audio/subtitle presence and quality, and will eventually expose a CLI, gRPC service, and Axum + Askama + HTMX + Tailwind admin web UI.

This is a personal Rust learning project. The author values quality, correctness, and architectural clarity over speed of delivery.

## Current State — Phases 1-6c complete

Implemented end-to-end pipeline plus rules engine, orchestrator service, and WASM plugin host:

- `brarr-mediainfo` (Phase 2): textual MediaInfo dump → `ParsedMediaInfo` + `ParseError`. Handles CRLF and LF, lenient to unknown fields.
- `brarr-core` (Phase 3): shared domain types — `Release`, `TrackerSource`, `Language`, `ReleaseEnrichment`, newtype IDs (`TmdbId`, `ImdbId`, `TvdbId`, `MalId`), `DecisionScore` (0..=1000), `ReleaseKind`, `Resolution`. Plus the `TrackerProvider` trait + `ProviderError` (Phase 6c).
- `brarr-tracker-unit3d` (Phase 4): async `reqwest`-based `Unit3dClient` with `search_by_tmdb` + `get_torrent`. Tolerant deserializers for the UNIT3D JSON variance between trackers. Implements `TrackerProvider`.
- `brarr-cli` (Phase 5): binary `brarr` with `search` subcommand, TOML config (`directories` crate for default paths), parallel fan-out via `futures::join_all`, `tracing` logging, `anyhow` at the binary boundary.
- `brarr-decision-service` (Phase 6a): declarative rules engine, TOML schema, `Engine::baseline()` reproduces the legacy Phase 5 scoring exactly.
- `brarr-orchestrator` (Phase 6b + cross-crate integration): tonic gRPC server (`Brarr` service: `Search`, `ListTrackers`, `RecentSearches`) + Axum admin web UI (dashboard, trackers CRUD, releases history, search detail) backed by SQLite via `sqlx`. Server-side rendered with Askama templates + HTMX, Tailwind via CDN, no frontend build pipeline. Search fan-out goes through the `brarr_core::TrackerProvider` trait so UNIT3D direct clients and WASM plugin providers share one pipeline. Trackers table has a `plugin_path` column — when set, the row is served by `WasmTrackerProvider`; otherwise by `Unit3dClient`. A single `wasmtime::Engine` lives in `AppState`. Binary `brarr-orchestrator` launches both servers concurrently.
- `brarr-plugin-host` (Phase 6c): wasmtime-backed sandbox that loads third-party tracker scrapers as core WASM modules. Plugin ABI v1 documented in the crate-level rustdoc (exports: `plugin_alloc/free`, `plugin_abi_version`, `plugin_name`, `plugin_search_by_tmdb`; imports: `env.host_log` gated by `HostCapabilities`). `WasmTrackerProvider` implements `TrackerProvider`.

181 tests pass (`cargo test --workspace --all-targets`). `INITIAL_PROMPT.md` remains the authoritative spec — consult before adding crates, dependencies, or making architectural decisions.

### Running the orchestrator

```powershell
$env:Path = "C:\Users\pc\.cargo\bin;$env:Path"
cargo run -p brarr-orchestrator
# → http://127.0.0.1:3000 (admin UI), gRPC on 127.0.0.1:50051
```

Env vars: `BRARR_DB_PATH` (default `./brarr.db`), `BRARR_HTTP_ADDR`, `BRARR_GRPC_ADDR`, `BRARR_STATIC_DIR`.

## Toolchain notes

- **Deployment target**: Linux (Docker). Windows local dev is supported but not the primary path.
- **Active toolchain on this machine**: `stable-x86_64-pc-windows-msvc` (rustc 1.95.0). Visual Studio 2022 Build Tools with the "Desktop development with C++" workload provides `link.exe` + `cl.exe`; crates with C build scripts (`aws-lc-sys`, `openssl-sys`, etc.) compile cleanly through MSVC.
- We previously used the GNU toolchain + MSYS2 mingw-w64 as a workaround. It worked but hit two sharp edges (rustup's `rust-mingw` self-contained bundle missing internal binutils that `dlltool` spawns; mingw-w64-crt 13.0.0 renaming `nanosleep` → `nanosleep64` while `aws-lc-sys` still calls the old name). MSVC sidesteps both. The GNU toolchain is still installed but no longer the default.
- **Cargo PATH**: `C:\Users\pc\.cargo\bin` may not be on PowerShell's default PATH. Prepend with: `$env:Path = "C:\Users\pc\.cargo\bin;$env:Path"` at the start of each shell session.
- **MSRV** pinned to **1.85** (`edition = "2024"` requires it).
- **Dev container alternative** for Linux-native builds: `Dockerfile.dev` + `.devcontainer/devcontainer.json` + `docker-compose.dev.yml`. Run `docker compose -f docker-compose.dev.yml run --rm dev cargo test --workspace` for a one-shot Linux build, or open the repo in a Dev Container-aware IDE (VS Code, JetBrains Gateway → RustRover).

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
