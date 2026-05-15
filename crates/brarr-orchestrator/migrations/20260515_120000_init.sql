-- Initial schema for brarr-orchestrator.
--
-- Three concerns kept in distinct tables:
--   1. trackers     — configured UNIT3D endpoints (mirrors what brarr-cli
--                     reads from TOML; orchestrator holds them in SQLite so
--                     the admin UI can add/remove without editing files).
--   2. searches     — one row per user-initiated search.
--   3. decisions    — per-release outcome rows produced by the rule engine.
--
-- IDs are TEXT/UUID v4 stringified. Timestamps are Unix epoch seconds
-- (INTEGER) — SQLite has no native time type, and storing seconds keeps
-- arithmetic trivial. `sqlx`'s `time::OffsetDateTime` integration handles
-- the conversion for typed bindings.

CREATE TABLE trackers (
    id              TEXT PRIMARY KEY NOT NULL,
    name            TEXT NOT NULL UNIQUE,
    base_url        TEXT NOT NULL,
    api_token       TEXT NOT NULL,
    kind            TEXT NOT NULL DEFAULT 'unit3d',
    created_at      INTEGER NOT NULL
) STRICT;

CREATE TABLE searches (
    id              TEXT PRIMARY KEY NOT NULL,
    tmdb_id         INTEGER,
    imdb_id         TEXT,
    submitted_at    INTEGER NOT NULL,
    result_count    INTEGER NOT NULL DEFAULT 0,
    -- Free-form JSON blob of the raw query — keeps the schema flexible
    -- while we figure out which fields belong as proper columns.
    request_json    TEXT NOT NULL
) STRICT;

CREATE INDEX idx_searches_submitted_at ON searches(submitted_at DESC);

CREATE TABLE decisions (
    id              TEXT PRIMARY KEY NOT NULL,
    search_id       TEXT NOT NULL REFERENCES searches(id) ON DELETE CASCADE,
    tracker_id      TEXT REFERENCES trackers(id) ON DELETE SET NULL,
    tracker_name    TEXT NOT NULL,
    release_name    TEXT NOT NULL,
    release_id_remote INTEGER NOT NULL,
    score           INTEGER NOT NULL,
    rejected        INTEGER NOT NULL DEFAULT 0,
    tags_json       TEXT NOT NULL DEFAULT '[]',
    matched_json    TEXT NOT NULL DEFAULT '[]',
    seeders         INTEGER NOT NULL DEFAULT 0,
    leechers        INTEGER NOT NULL DEFAULT 0,
    size_bytes      INTEGER NOT NULL DEFAULT 0,
    resolution      TEXT NOT NULL DEFAULT 'unknown',
    kind            TEXT NOT NULL DEFAULT 'unknown',
    decided_at      INTEGER NOT NULL
) STRICT;

CREATE INDEX idx_decisions_search_id ON decisions(search_id);
CREATE INDEX idx_decisions_decided_at ON decisions(decided_at DESC);
