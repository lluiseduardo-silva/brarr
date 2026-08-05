# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**brarr** is a Portuguese-language torrent search aggregator across UNIT3D-based trackers (locadora.cc, capybarabr.com, etc.). It scores releases by Portuguese audio/subtitle presence and quality, and will eventually expose a CLI, gRPC service, and Axum + Askama + HTMX admin web UI.

This is a personal Rust learning project. The author values quality, correctness, and architectural clarity over speed of delivery.

## Current State — Phases 1-6c complete

Implemented end-to-end pipeline plus rules engine, orchestrator service, and WASM plugin host:

- `brarr-mediainfo` (Phase 2): textual MediaInfo dump → `ParsedMediaInfo` + `ParseError`. Handles CRLF and LF, lenient to unknown fields.
- `brarr-core` (Phase 3): shared domain types — `Release`, `TrackerSource`, `Language`, `ReleaseEnrichment`, newtype IDs (`TmdbId`, `ImdbId`, `TvdbId`, `MalId`), `DecisionScore` (0..=1000), `ReleaseKind`, `Resolution`. Plus the `TrackerProvider` trait + `ProviderError` (Phase 6c).
- `brarr-tracker-unit3d` (Phase 4): async `reqwest`-based `Unit3dClient` with `search_by_tmdb` + `get_torrent`. Tolerant deserializers for the UNIT3D JSON variance between trackers. Implements `TrackerProvider`.
- `brarr-tracker-newznab` (Newznab/Torznab): async `reqwest` client for Newznab indexers (NZBGeek, DrunkenSlug, NZB.su, etc.) and Torznab Usenet-shape trackers. Hand-rolled `quick-xml` parser that collects `<newznab:attr>` entries (including repeated `audio`/`subs`) into a `HashMap` and maps `language`/`audio`/`subs` — plus the `language-audio`/`language-subs`/`lang` attrs that BR forks like Curupira emit — to `ReleaseEnrichment`. `Language::from_mediainfo` accepts English names, region names (`Portuguese (Brazil)`), and ISO codes (`pt-BR`, `pob`, `pt`, `eng`), so PT detection works across indexer dialects. Implements `TrackerProvider` via `search_by_imdb` (movie-search axis).
- `brarr-cli` (Phase 5 + remote subcommand): binary `brarr` with two subcommands. `brarr search` fans out locally from a TOML provider config. `brarr remote --addr host:port --token <t> {--tmdb <id> | --imdb <ttN>}` dispatches the search to a running `brarr-orchestrator` over gRPC (no local tracker config needed). Both render results through the same text/JSON formatter.
- `brarr-decision-service` (Phase 6a): declarative rules engine, TOML schema, `Engine::baseline()` reproduces the legacy Phase 5 scoring exactly. `Condition` is a **boolean tree** — beyond the flat AND of leaf fields it carries `all`/`any`/`not` combinators (recursive). Leaf predicates: `audio`/`subtitle`/`hdr`/`resolution`/`min_seeders`/`min_size_bytes`/`max_size_bytes`/`tracker`/`kind` (`web-dl`/`bluray`/`remux`/`encode`/`hdtv`/`dvd`)/`video_codec` (`x264`/`x265`/`av1`, accepts `h264`/`h265`/`avc`/`hevc` aliases)/`release_group`/`proper`/`repack`/`title_contains`/`title_matches` (regex). `kind`/`video_codec`/`release_group`/`proper`/`repack` read `Release.tags` (populated by `brarr_core::parse_release_tags` from the title; codec refined by MediaInfo in the UNIT3D converter). The `Engine` pre-compiles `title_matches` regexes; `RuleSet::validate()` reports invalid patterns to the profile editor. Adding new leaves was backward-compatible (all `Option`, no migration) — stored `rules_json` and the 5 presets are unchanged.
- `brarr-orchestrator` (Phase 6b + cross-crate integration): tonic gRPC server (`Brarr` service: `Search`, `ListProviders`, `RecentSearches`) + Axum admin web UI (dashboard, providers CRUD, releases history, search detail) backed by SQLite via `sqlx`. Server-side rendered with Askama templates + HTMX. **CSS is hand-authored and committed at `crates/brarr-orchestrator/static/app.css`** — there is no Tailwind and no frontend build step; the design tokens are CSS custom properties mirroring the Figma file, and dark mode swaps them via `:root[data-theme="dark"]`. Search fan-out goes through the `brarr_core::TrackerProvider` trait so UNIT3D direct clients, Newznab/Torznab indexers, and WASM plugins share one pipeline. The `providers` table has a `kind` column (`unit3d` / `newznab` / `torznab` / `plugin`) and a nullable `plugin_path`. Dispatch matrix: `plugin_path=Some` → `WasmTrackerProvider`; `kind=newznab|torznab` → `NewznabClient` (shared XML protocol); otherwise `Unit3dClient`. The gRPC `Search` RPC accepts `tmdb_id` and/or `imdb_id`; orchestrator dispatch prefers IMDb for Newznab/Torznab rows and TMDb everywhere else, falling back to the other axis when missing. A single `wasmtime::Engine` lives in `AppState`. Binary `brarr-orchestrator` launches both servers concurrently. **Terminology note**: pre-`a0b63a8` the configuration table was called `trackers`; migration `20260516120000_rename_to_providers.sql` renames it in-place along with FK + denorm columns in `decisions`.
- `brarr-plugin-host` (Phase 6c): wasmtime-backed sandbox that loads third-party tracker scrapers as core WASM modules. Runs in async mode (`Config::async_support`) with epoch interruption (`Config::epoch_interruption`) so host imports can `.await` and runaway plugins trap on a per-call deadline. Plugin ABI v1 documented in the crate-level rustdoc (exports: `plugin_alloc/free`, `plugin_abi_version`, `plugin_name`, `plugin_search_by_tmdb`; imports: `env.host_log` and `env.host_fetch`). `host_fetch` is async, capability-gated by `HostCapabilities::fetch` plus an `allowed_hosts` allowlist, with a per-request timeout. `HostCapabilities::call_deadline` (default 5s) caps wall-clock CPU per call via a background epoch ticker (`WasmEpochTicker`); `max_memory_pages` (default 1024 = 64 MiB) caps linear memory via a `ResourceLimiter`. `WasmTrackerProvider` implements `TrackerProvider`.
- `brarr-orchestrator` (inbound webhooks, `src/web/webhooks.rs`): receives Sonarr/Radarr Connect → Webhook events at `POST /webhooks/{radarr|sonarr}/{arr_instance_id}` (auth via `?apikey=` / bearer / trusted-peer bypass), persists an audit row in `webhook_events`, and runs the search + optional auto-push in a background `tokio::spawn`. **Discoverability UI**: `/arr-instances` shows the ready-to-paste webhook URL per instance (copy button via `static/copy.js`); a `/webhooks` nav page renders the `webhook_events` audit log (`webhook_events::recent`). **Poll reduction**: `arr_instances.webhook_driven` (migration `20260613120000`, toggled from `/arr-instances`) makes the scheduled poller skip that instance — the manual "rodar agora" button still works. (autobrr support is deferred.)
- `brarr-orchestrator` (Torznab indexer endpoint, `src/web/torznab.rs`): exposes `/torznab/api` so Sonarr/Radarr can use brarr as a single virtual Newznab/Torznab indexer that fans out across every configured provider. Dispatch on `?t=`: `caps` returns an XML capability advert (movie-search: yes; tv-search: yes); `movie&{tmdbid|imdbid}` and `tvsearch&tvdbid[&season&ep]` run the full search pipeline and render the kept decisions as an RSS feed with `torznab:attr` blocks (category, size, seeders, peers, leechers, grabs, tmdb, imdb); `search` (free-text) returns a valid empty feed (axis not built); a missing/blank `t=` defaults to `caps` (so the bare base URL is testable in a browser; *arr always sends an explicit `t=`); unknown `t=` values return `400` with a Newznab `<error code="202">` payload. **Profile pre-filter**: `movie`/`tvsearch` accept `?profile=<uuid|name>` — when set, brarr keeps only releases whose score under that quality profile clears its `push_threshold` (so brarr's add_score rules apply on the pull path, not just *arr's own quality profile); absent ⇒ every found release. Unknown profile ⇒ `400` Newznab error. The `/settings` page surfaces the indexer URL + a profile picker (`crate::web::torznab::resolve_profile_filter`/`score_passes`). Auth accepts either `?apikey=<BRARR_AUTH_TOKEN>` (the *arr-native way) or `Authorization: Bearer <token>`. Categories are inferred from `ReleaseKind` + `Resolution`: movies vs TV via `S##E##` regex heuristic on the title, then `2030/5030` SD, `2040/5040` HD, `2045/5045` UHD, plus `2050` BluRay as a secondary subcat for movie BluRay rips. Per-item download URL falls back to a synthetic `brarr:///download/<release_id>` placeholder when the persisted decision row doesn't carry an upstream URL; an explicit `/torznab/download/{tracker}/{release}` proxy route remains future work. Hand-rolled XML via `quick-xml`'s element builder — no Askama templates touch this surface.

- `brarr-orchestrator` (pull-path performance, `src/search.rs` + `src/ttl_cache.rs` + `src/provider_cache.rs`): the Torznab/Newznab feed handlers call `run_search_cached` instead of `run_search`. Sonarr/Radarr blow one Interactive Search up into many near-identical indexer requests (per season/episode, once per configured feed), so without dedup each one re-fans-out and re-persists a `searches`+`decisions` set, stalling the *arr UI for minutes. Four guards: (1) **protocol-scoped fan-out** — `run_search_scoped(state, keys, ProviderScope)` filters the enabled providers before dispatch: `/torznab/api` only hits torrent-side providers (`unit3d`/`torznab`/plugin), `/newznab/api` only `newznab` rows (`ProviderScope::{Torrent,Usenet}`; admin UI / poller / gRPC / webhooks use `All` via the `run_search` wrapper). The scope mirrors `Protocol::matches_kind` on the render side (a test asserts they agree), so a feed no longer waits on providers whose results it would drop; (2) a short-TTL `TtlCache<(SearchKeys, ProviderScope), SearchRunOutcome>` in `AppState` (`SEARCH_CACHE_TTL`, 60s; `ttl=0` disables) collapses the duplicate burst onto a single fan-out per scope — `run_search_cached` returns `(outcome, cache_hit)`; the admin UI and poller still call `run_search` directly for fresh data; (3) `fan_out` caps each provider's dispatch with `tokio::time::timeout` (`PER_PROVIDER_BUDGET`, 15s) so one slow/timing-out tracker can't hold the whole search for the underlying client's 30s×retry window — it's reported as a timeout failure and the rest proceed; (4) a `ProviderClientCache` keyed by provider id + a config fingerprint reuses the built UNIT3D/Newznab HTTP client (warm `reqwest` connection pool + TLS) across requests; plugins are still rebuilt per call.

- `brarr-orchestrator` (observability, `src/db/metrics.rs` + `/health` page): two STRICT tables (migration `20260721120000`) answer "is the slowness brarr, the cache, or an upstream tracker". `provider_metrics` gets one row per provider per fan-out — duration, `ok`/`error`/`timeout` outcome, release count — written best-effort by `run_search_scoped` (`record_provider_metric`; a metrics failure never fails the search). `endpoint_metrics` gets one row per `/torznab/*`/`/newznab/*` request — `t=` function, HTTP status, latency as the *arr client saw it, and cache `hit`/`miss` (the search handlers attach a `SearchCacheMarker` response extension; the outermost `metrics_middleware` in `web/torznab.rs` reads it, so 401s and 400s are measured too). The `/health` admin page ("Saúde" nav) aggregates both over 24h in Rust (`provider_stats`/`endpoint_stats`: count, ok/erro/timeout, avg/p50/p95/max — nearest-rank ceiling percentile so small-sample p95 exposes the worst case — cache hit rate, last error) plus a recent-requests table. Metric rows are pruned with the same retention window (`db::metrics::prune` inside `run_prune`; `MaintenanceOutcome.metrics_deleted`).

- `brarr-orchestrator` (own library, `src/db/library.rs` + `src/db/grabs.rs`, migration `20260804120000_library.sql`): brarr's own catalogue, replacing "whatever the \*arr answers on `/wanted/missing`" as the wanted-list. `library_items` (UNIQUE `media_type,tmdb_id`; `imdb_id` stored canonical **with** the `tt` prefix — note `searches.imdb_id` stores the bare number, the two conventions are reconciled in code) → `library_seasons` → `library_episodes`. **Metadata is a cache, monitoring is state**: `library::upsert` refreshes only TMDB-owned columns on conflict, never `monitored` / `profile_id` / `root_folder` / `added_at`; `sync_seasons` rebuilds the tree while preserving per-season and per-episode monitoring flags. `grabs` is the durable acquisition record (deliberately *not* hung off `decisions`, which the retention sweep prunes after 7 days). **`grabs::reserve` is the idempotency barrier**: it inserts a `reserved` row with `ON CONFLICT DO NOTHING` *before* any network call and returns `Ok(None)` to the losers, so concurrent tasks can't both grab a release. Two partial unique indexes rather than one composite, because SQLite treats NULL as distinct from NULL and `episode_id` is nullable (movies and season packs would otherwise be unconstrained). `release_id_remote` is TEXT — the old push path ran `parse::<u64>().unwrap_or(0)` over non-numeric Newznab guids, collapsing every such release onto key 0.

- `brarr-tmdb` (metadata source): async `reqwest` client for the TMDB v3 API, modelled on `brarr-tracker-unit3d` (thiserror, tolerant deserialisation, same retry shape). Auth is the **v4 read access token** as `Authorization: Bearer` — the v3 API key is a different string sent as a query param, and passing it as a bearer yields 401. Surface: `search_movies` / `search_tv` / `find_by_imdb` / `find_by_tvdb` / `movie` / `tv` / `season` / `verify_token`. Two upstream behaviours are handled in `model.rs` rather than leaked: (1) **TMDB has no automatic language fallback** — `language=pt-BR` returns an *empty* `overview` when no translation exists, so the client appends `translations` and walks pt-BR → pt-PT → en-US in code; (2) **release dates are per country and per type** — digital is `type == 4`, physical `5`, theatrical `3`, resolved for BR first then US then any. `tvdb_id` exists for series only. `ATTRIBUTION` holds the exact ToU phrase (the shorter FAQ wording is *not* the one the terms require) and `image_url()` builds CDN URLs — brarr stores only `poster_path`, never image bytes, because the ToU forbid using TMDB as an image host and forbid caching metadata beyond six months. **Fixtures in `tests/fixtures/` are derived from the documented schema, not captured live** (every endpoint needs a token); see the README there — re-capture them once a token exists.

- `brarr-orchestrator` (TMDB bridge, `src/tmdb_sync.rs`): wires `brarr-tmdb` into the library tables. Pure conversions (`movie_to_item` / `tv_to_item` / `season_to_new`) are separated from the fetch-and-persist side (`add_movie` / `add_series` / `refresh`) so the mapping is unit-testable without a pool or a network. Config comes from `settings` first and env second (`BRARR_TMDB_TOKEN`), with keys `tmdb_token` / `tmdb_language` / `tmdb_country` / `tmdb_ttl_days`; a blanked setting reads as "use the default" rather than as an empty value. `canonical_imdb` reconciles the two IMDb conventions in the codebase — `searches.imdb_id` holds the bare number, `library_items.imdb_id` the `ttNNNNNNN` form. `AppError::Tmdb` is deliberately not `#[from]`: the conversion peels off 401 into an `InvalidInput` that names the read-access-token mix-up, and 404 into `NotFound`.

- `brarr-orchestrator` (library UI, `templates/library*.html` + the `/library*` routes): `/library` renders the catalogue as a poster grid or a dense list — both server-rendered, switched by `?view=`, with `?filter=` chips for movie/tv/unmonitored. `/library/add` is the TMDB search; adding a series also walks every season so the episode tree lands with the item. Monitor toggle and delete answer with `HX-Refresh: true` instead of a row fragment, because the row markup differs between the two layouts and a partial would mean maintaining it twice. **Season 0 is TMDB's specials bucket and is excluded from both halves of the tree summary** — The Boys carries 76 specials against 40 real episodes, so counting them would render "5 temporadas · 116 episódios". `/settings` gained a TMDB section; the credential field is write-only (never echoed back, blank means "keep the current one").
- `brarr-orchestrator` (library detail, `templates/library_detail.html` + `partials/library_season.html`): `/library/{id}` renders the TMDB hero (poster w342, status/runtime/release-date pills, external-id chips, synopsis), the season accordion for series, and the grab history. **Seasons are collapsed and their episodes load on demand** (`hx-trigger="click once"` → `GET /library/{id}/season/{season_id}`) — a long-running series carries a hundred-plus episodes and rendering them all up front costs a query and a lot of DOM for rows nobody opened. Season toggle cascades to its episodes and answers `HX-Refresh` (the open accordion body is stale after the cascade); episode toggle swaps just its own row, reusing `LibrarySeasonPartial` with a one-element vec. The theatrical-window alert fires only while `digital_release_at` is still in the future — verified discriminating on live data: of six titles, it fired on exactly the one whose digital date had not passed.
- **`tests/css_coverage.rs` restores what Tailwind used to catch at build time.** With `--color-*: initial`, an unknown utility simply failed to generate and the gap was visible; hand-authored CSS makes it silently inert instead. The test parses `static/app.css` for class selectors, strips Askama tags from every template (before locating attributes — conditional classes carry their own quotes) and fails on any class with no rule. It has already caught a real regression: `border-danger-solid/30` and `border-success-solid/30`, opacity variants Tailwind generated on demand and the migration dropped.

648 tests pass (`cargo test --workspace --all-targets`). `INITIAL_PROMPT.md` remains the authoritative spec — consult before adding crates, dependencies, or making architectural decisions.

### Running the orchestrator

```powershell
$env:Path = "C:\Users\pc\.cargo\bin;$env:Path"
$env:BRARR_AUTH_TOKEN = "$(openssl rand -hex 32)"  # optional but recommended
cargo run -p brarr-orchestrator
# → http://127.0.0.1:3000 (admin UI), gRPC on 127.0.0.1:50051
```

Env vars: `BRARR_DB_PATH` (default `./brarr.db`), `BRARR_HTTP_ADDR`, `BRARR_GRPC_ADDR`, `BRARR_STATIC_DIR`, `BRARR_AUTH_TOKEN`, `BRARR_DECISIONS_RETENTION_DAYS` (default `7`, `0` = keep forever).

**History retention / DB maintenance**: the poller persists a `decisions` row per evaluated release every cycle, so the table grows unbounded without a retention policy. A background task (`src/maintenance.rs`, mirrors `poll::spawn`) wakes every 6h and prunes `decisions`/`searches` older than `BRARR_DECISIONS_RETENTION_DAYS` (hot-reloadable via `/settings`), preserving any decision referenced by `push_history` (pushed releases) and any search that still has decisions. SQL lives in `db::maintenance` (`run_prune`/`prune_decisions`/`prune_searches`/`checkpoint_wal`/`incremental_vacuum`/`full_vacuum`); `db::open` sets `auto_vacuum=INCREMENTAL`. On-demand triggers: `/settings` "Manutenção do banco" buttons (prune / VACUUM), the gRPC `RunMaintenance` RPC, and `brarr maintenance --addr <a> [--token <t>] [--vacuum]`. The one-time offline reduction for a large pre-existing DB is `scripts/db-maintenance.sql` — see `docs/DB-MAINTENANCE.md`.

When `BRARR_AUTH_TOKEN` is set, the UI requires a login at `/login` (sets a `brarr_session` HttpOnly cookie) and gRPC calls must present `authorization: Bearer <token>` metadata. When unset, the orchestrator logs a `warn!` once at startup and lets every request through (dev mode).

### Production deploy (Docker)

```bash
docker build -t brarr:latest .
docker run --rm \
  -p 127.0.0.1:3000:3000 -p 127.0.0.1:50051:50051 \
  -v brarr-data:/data -v "$PWD/plugins:/plugins:ro" \
  -e BRARR_AUTH_TOKEN="$(openssl rand -hex 32)" \
  brarr:latest
```

Or via compose: `docker compose -f docker-compose.prod.yml --env-file .env.prod up -d` (after writing `BRARR_AUTH_TOKEN=...` into `.env.prod`).

Image layout: multi-stage `rust:1.95-slim-bookworm` builder → `debian:bookworm-slim` runtime, non-root user (`uid 10001`), `/data` volume for sqlite + `/plugins` for `.wasm` modules, `tini` as PID 1, `wget`-based HEALTHCHECK against `/healthz`.

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
| `brarr-orchestrator` | deferred | gRPC service + Axum/Askama/HTMX web UI (CSS artesanal) |
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
