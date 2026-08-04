-- Own media library. Until now brarr had no catalogue of its own: the
-- "what do I want" list *was* whatever Radarr/Sonarr answered on
-- `/wanted/missing`. That is what makes the double-grab possible — the
-- *arr fires a webhook at brarr *and* runs its own indexer search, so two
-- independent pipelines reach the same download client. Owning the
-- catalogue is what lets brarr be the only agent.
--
-- Four tables:
--
--   library_items     one row per monitored movie or series
--   library_seasons   one row per season (tv only)
--   library_episodes  one row per episode — replaces `/wanted/missing`
--   grabs             one row per acquisition attempt, durable
--
-- Metadata comes from TMDB and is a *cache*, not the truth: their terms
-- forbid holding it longer than six months, so identifiers (perennial)
-- are kept apart from title/overview/poster/dates (expirable, tracked by
-- `metadata_refreshed_at`). Only `poster_path` is stored, never image
-- bytes — the terms also forbid using TMDB as an image host.

CREATE TABLE library_items (
    id                    TEXT    PRIMARY KEY NOT NULL,
    media_type            TEXT    NOT NULL CHECK (media_type IN ('movie', 'tv')),

    -- TMDB is the library's canonical axis. `tmdb_id` alone is not
    -- unique across kinds (a movie and a series can share an id), hence
    -- the composite unique index below.
    tmdb_id               INTEGER NOT NULL,
    -- Canonical `ttNNNNNNN` form, WITH the prefix. Note `searches.imdb_id`
    -- stores the bare number instead ('133093'); do not copy that here —
    -- the two conventions are reconciled in code, not in the schema.
    imdb_id               TEXT,
    -- TMDB only exposes an external tvdb_id for series, never for movies.
    tvdb_id               INTEGER,

    -- TMDB metadata cache.
    title                 TEXT    NOT NULL,
    original_title        TEXT,
    year                  INTEGER,
    -- May legitimately be '' — TMDB has no automatic language fallback,
    -- so a title with no pt-BR translation returns an empty overview.
    overview              TEXT,
    poster_path           TEXT,
    backdrop_path         TEXT,
    -- 'Returning Series' | 'Ended' | 'Released' | … (tv mostly).
    tmdb_status           TEXT,
    runtime_minutes       INTEGER,

    -- Scheduling inputs. `next_air_date` drives the series poller;
    -- the release-date pair keeps brarr from burning searches on a movie
    -- that is still only in cinemas.
    next_air_date         INTEGER,
    digital_release_at    INTEGER,
    physical_release_at   INTEGER,

    -- Monitoring.
    monitored             INTEGER NOT NULL DEFAULT 1 CHECK (monitored IN (0, 1)),
    profile_id            TEXT,
    root_folder           TEXT,

    added_at              INTEGER NOT NULL,
    metadata_refreshed_at INTEGER NOT NULL,

    FOREIGN KEY (profile_id) REFERENCES quality_profiles(id) ON DELETE SET NULL
) STRICT;

CREATE UNIQUE INDEX idx_library_items_tmdb      ON library_items(media_type, tmdb_id);
CREATE INDEX        idx_library_items_monitored ON library_items(monitored);
CREATE INDEX        idx_library_items_imdb      ON library_items(imdb_id);
CREATE INDEX        idx_library_items_tvdb      ON library_items(tvdb_id);
CREATE INDEX        idx_library_items_refreshed ON library_items(metadata_refreshed_at);

CREATE TABLE library_seasons (
    id            TEXT    PRIMARY KEY NOT NULL,
    item_id       TEXT    NOT NULL,
    season_number INTEGER NOT NULL,
    episode_count INTEGER NOT NULL DEFAULT 0,
    air_date      INTEGER,
    -- Partial monitoring: an operator can follow season 4 and ignore the
    -- back catalogue. The poller reads this, not the parent flag alone.
    monitored     INTEGER NOT NULL DEFAULT 1 CHECK (monitored IN (0, 1)),

    FOREIGN KEY (item_id) REFERENCES library_items(id) ON DELETE CASCADE
) STRICT;

CREATE UNIQUE INDEX idx_library_seasons_item ON library_seasons(item_id, season_number);

CREATE TABLE library_episodes (
    id             TEXT    PRIMARY KEY NOT NULL,
    item_id        TEXT    NOT NULL,
    season_id      TEXT    NOT NULL,
    -- Denormalised from the parent season so the poller can filter and
    -- sort without a join; kept in sync by the TMDB refresh.
    season_number  INTEGER NOT NULL,
    episode_number INTEGER NOT NULL,
    title          TEXT,
    air_date       INTEGER,
    monitored      INTEGER NOT NULL DEFAULT 1 CHECK (monitored IN (0, 1)),

    FOREIGN KEY (item_id)   REFERENCES library_items(id)   ON DELETE CASCADE,
    FOREIGN KEY (season_id) REFERENCES library_seasons(id) ON DELETE CASCADE
) STRICT;

CREATE UNIQUE INDEX idx_library_episodes_number ON library_episodes(item_id, season_number, episode_number);
CREATE INDEX        idx_library_episodes_air    ON library_episodes(air_date);

-- Acquisition attempts. Durable on purpose: today the only handle on a
-- download is `decisions.id`, and `decisions` is pruned after
-- BRARR_DECISIONS_RETENTION_DAYS (7 by default). An autonomous brarr
-- cannot hang acquisition state off a row that disappears.
--
-- `release_id_remote` is TEXT, not INTEGER. The old push path parsed it
-- with `parse::<u64>().unwrap_or(0)` while Newznab providers return a
-- non-numeric guid, so every Newznab release collapsed onto key 0 and
-- the dedup check silently matched unrelated releases.
CREATE TABLE grabs (
    id                TEXT    PRIMARY KEY NOT NULL,
    item_id           TEXT    NOT NULL,
    -- Set for a per-episode grab; NULL for a movie or a season pack.
    episode_id        TEXT,
    -- Set for a season pack (episode_id NULL, season_number present).
    season_number     INTEGER,

    -- Snapshot of the scoring decision. Nullable + ON DELETE SET NULL
    -- because `decisions` is pruned on a retention window while the grab
    -- has to outlive it.
    decision_id       TEXT,
    provider_id       TEXT,
    provider_name     TEXT    NOT NULL,

    release_id_remote TEXT    NOT NULL,
    release_name      TEXT    NOT NULL,
    download_url      TEXT,
    protocol          TEXT    NOT NULL CHECK (protocol IN ('torrent', 'usenet')),

    status            TEXT    NOT NULL CHECK (status IN
                          ('reserved', 'sent', 'downloading', 'completed',
                           'imported', 'failed', 'rejected')),
    error             TEXT,

    grabbed_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,

    FOREIGN KEY (item_id)     REFERENCES library_items(id)    ON DELETE CASCADE,
    FOREIGN KEY (episode_id)  REFERENCES library_episodes(id) ON DELETE SET NULL,
    FOREIGN KEY (decision_id) REFERENCES decisions(id)        ON DELETE SET NULL,
    FOREIGN KEY (provider_id) REFERENCES providers(id)        ON DELETE SET NULL
) STRICT;

-- The idempotency barrier. Nothing like it existed before: `push_history`
-- had no UNIQUE at all and inserted an unconditional `Uuid::new_v4()`, so
-- the read-then-write dedup was a plain SELECT COUNT(*) whose window
-- spanned the whole HTTP round-trip. Two concurrent webhook tasks
-- converging on the same release both passed it.
--
-- Two partial indexes rather than one composite: SQLite treats NULL as
-- distinct from NULL, so a single index over a nullable `episode_id`
-- would not constrain movies or season packs at all.
CREATE UNIQUE INDEX idx_grabs_unique_episode
    ON grabs(provider_id, release_id_remote, item_id, episode_id)
    WHERE episode_id IS NOT NULL;

CREATE UNIQUE INDEX idx_grabs_unique_item
    ON grabs(provider_id, release_id_remote, item_id)
    WHERE episode_id IS NULL;

CREATE INDEX idx_grabs_status     ON grabs(status);
CREATE INDEX idx_grabs_item       ON grabs(item_id);
CREATE INDEX idx_grabs_grabbed_at ON grabs(grabbed_at DESC);

-- Ties a search back to the catalogue entry that triggered it. NULL for
-- ad-hoc searches submitted straight from the admin UI or the CLI.
ALTER TABLE searches ADD COLUMN library_item_id TEXT REFERENCES library_items(id) ON DELETE SET NULL;

CREATE INDEX idx_searches_library_item ON searches(library_item_id);
