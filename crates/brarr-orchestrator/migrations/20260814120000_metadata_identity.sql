-- # Identity as a set, and structure as a declared choice
--
-- Additive. Every existing column stays and stays populated, so nothing
-- in the application changes behaviour on this migration alone — the
-- readers move in the next one, and the old columns come out in the one
-- after that, together with the library wipe that precedes the re-import.
--
-- ## Why a table of sources instead of another CHECK
--
-- `20260813140000` had to add one value to one CHECK, and SQLite has no
-- `ALTER TABLE ... DROP CONSTRAINT`: the repair was ADD COLUMN + UPDATE +
-- DROP COLUMN + RENAME. The defect it repaired was silent — the Rust enum
-- matched, clippy passed, 1068 tests passed, and every write of a
-- TheTVDB-derived numbering died in the database.
--
-- So the rule this schema follows from here on: **enumerate what brarr
-- means, never who told brarr.** `media_type`, `structure_family` and
-- `link_method` are brarr's own vocabulary and stay CHECKs, because they
-- do not grow when a provider is added. A provider's name is a row with
-- a foreign key: adding one is an INSERT, and an unregistered one is a
-- constraint violation on first write rather than a value that is valid
-- in Rust and inert here.

CREATE TABLE metadata_sources (
    label        TEXT PRIMARY KEY NOT NULL,
    display_name TEXT NOT NULL,
    --   provider  — a crate talks to it and it answers questions
    --   namespace — it issues ids brarr stores and never queries (IMDb)
    kind         TEXT NOT NULL CHECK (kind IN ('provider', 'namespace'))
) STRICT;

INSERT INTO metadata_sources (label, display_name, kind) VALUES
    ('tmdb',   'TMDB',    'provider'),
    ('tvdb',   'TheTVDB', 'provider'),
    ('imdb',   'IMDb',    'namespace');


-- ## Identity as a set, not as three named columns
--
-- `library_items.tmdb_id INTEGER NOT NULL` plus `UNIQUE(media_type,
-- tmdb_id)` makes a title only TheTVDB knows unrepresentable — and
-- `NewLibraryItem` derives `Default`, so such a title would carry
-- `tmdb_id = 0` and exactly *one* of them would fit in the whole table.
--
-- It also fixes two live costs. "Already in the library?" is
-- `(media_type, tmdb_id)` today, so adding by TMDB and syncing by the
-- \*arr's TVDB axis produces two rows for one series. And
-- `arr_import::resolve_tmdb_id` calls `find_by_tvdb` per title on every
-- pass over the catalogue, purely to convert onto the TMDB axis before
-- cataloguing.

CREATE TABLE library_item_ids (
    item_id     TEXT    NOT NULL,
    source      TEXT    NOT NULL,
    -- TEXT because the convention belongs to the source: 'tt0133093',
    -- '603', '70726'. An INTEGER here would be a bet that no future
    -- provider uses a slug, and this repository has already paid for
    -- that bet once — `parse::<u64>().unwrap_or(0)` over non-numeric
    -- Newznab guids collapsed every one of them onto key 0.
    external_id TEXT    NOT NULL,
    -- Denormalised from the parent so the natural key constrains without
    -- a join: a movie and a series may legitimately share a TMDB id,
    -- which is what `idx_library_items_tmdb(media_type, tmdb_id)` was
    -- guarding.
    media_type  TEXT    NOT NULL CHECK (media_type IN ('movie', 'tv')),
    -- When a cross-resolution vouched for this pairing. NULL means the
    -- id was merely asserted — an operator typed it, an \*arr reported
    -- it. Reading this is what removes the per-title `find_by_tvdb`.
    verified_at INTEGER,

    PRIMARY KEY (item_id, source),
    FOREIGN KEY (item_id) REFERENCES library_items(id)       ON DELETE CASCADE,
    FOREIGN KEY (source)  REFERENCES metadata_sources(label)
) STRICT;

CREATE UNIQUE INDEX idx_library_item_ids_natural
    ON library_item_ids(source, media_type, external_id);

-- Backfill from the three columns. `tmdb_id` is NOT NULL today so every
-- row contributes one; the other two contribute where they are set.
INSERT INTO library_item_ids (item_id, source, external_id, media_type)
    SELECT id, 'tmdb', CAST(tmdb_id AS TEXT), media_type
      FROM library_items
     WHERE tmdb_id > 0;

INSERT INTO library_item_ids (item_id, source, external_id, media_type)
    SELECT id, 'imdb', imdb_id, media_type
      FROM library_items
     WHERE imdb_id IS NOT NULL AND imdb_id <> '';

INSERT INTO library_item_ids (item_id, source, external_id, media_type)
    SELECT id, 'tvdb', CAST(tvdb_id AS TEXT), media_type
      FROM library_items
     WHERE tvdb_id IS NOT NULL AND tvdb_id > 0;


-- ## Facets: who owns which half of a title
--
-- The line falls where two providers can disagree *without contradicting
-- each other*. Title, year and overview describe one cut of a work;
-- taking the title from one and the year from another produces a record
-- that describes nothing. So there are classes, not fifteen per-field
-- owners.
--
-- **Description** has a single owner and a policy that wins immediately:
-- rewriting an overview is cheap and reversible.
--
-- **Structure** is the opposite, and the asymmetry is the heart of it:
-- rebuilding a tree re-points every acquisition hanging off it, so the
-- stored owner beats the policy and a change is an act per title. A
-- sweep that moved it silently would be the v0.13 damage with a new
-- cause.
--
-- **Art** is a fallback with provenance rather than an owner: TMDB stores
-- a path relative to its CDN and TheTVDB returns an absolute URL, so the
-- column exists to *build the URL*, not to arbitrate.

ALTER TABLE library_items ADD COLUMN descriptive_source TEXT
    REFERENCES metadata_sources(label);

-- NULL for a movie: a film has no tree, and a source recorded for one is
-- a claim nothing can honour.
ALTER TABLE library_items ADD COLUMN structure_source TEXT
    REFERENCES metadata_sources(label);

-- brarr's OWN word for an ordering, so "is this series in absolute
-- order?" is answerable without comparing provider strings. `other` is
-- the escape that keeps this CHECK from becoming the defect at the top
-- of this file one level down: an ordering no brarr word covers is
-- `other` plus a handle, and costs no migration.
ALTER TABLE library_items ADD COLUMN structure_family TEXT
    CHECK (structure_family IS NULL OR structure_family IN
           ('default', 'aired', 'dvd', 'absolute', 'alternate',
            'production', 'manual', 'other'));

-- Opaque to brarr, interpreted only by the owning provider: a season-type
-- segment for TheTVDB, an episode group's hex id for TMDB.
ALTER TABLE library_items ADD COLUMN structure_handle TEXT;

-- The blocks an operator declared, as sizes — `"12, 13"`. Kept as the
-- *recipe* and not only as its result, because a refresh has to be able
-- to re-apply it to a tree that grew an episode.
ALTER TABLE library_items ADD COLUMN structure_recipe TEXT;

-- No sweep writes structure while this is on.
--
-- A boolean orthogonal to the value, rather than a fifth enum member.
-- `'off'` had to exist because NULL would be undone by the next sweep,
-- and then `'off'` became a one-way door until an undo was written for
-- it. A flag has no unreachable state: un-pinning is `UPDATE ... = 0`.
ALTER TABLE library_items ADD COLUMN structure_pinned INTEGER NOT NULL DEFAULT 0
    CHECK (structure_pinned IN (0, 1));

ALTER TABLE library_items ADD COLUMN poster_source   TEXT REFERENCES metadata_sources(label);
ALTER TABLE library_items ADD COLUMN backdrop_source TEXT REFERENCES metadata_sources(label);

-- Two timestamps rather than a facets table.
--
-- The facets table was carried in the design to distinguish "the source
-- said there is nothing" from "the source did not answer" — the
-- ambiguity that made `apply_derived` write NULL for both and re-derive
-- every cycle. That ambiguity does not survive into this schema:
-- "nobody looked" is `structure_source IS NULL` and "looked, and this is
-- the answer" is the source itself, so a failed attempt simply leaves
-- the column alone and the next cycle retries, which is correct rather
-- than a defect. Two columns, and the table earns its keep the day
-- something needs the note.
ALTER TABLE library_items ADD COLUMN descriptive_refreshed_at INTEGER;
ALTER TABLE library_items ADD COLUMN structure_refreshed_at   INTEGER;

UPDATE library_items
   SET descriptive_source       = 'tmdb',
       descriptive_refreshed_at = metadata_refreshed_at,
       poster_source            = CASE WHEN poster_path   IS NOT NULL THEN 'tmdb' END,
       backdrop_source          = CASE WHEN backdrop_path IS NOT NULL THEN 'tmdb' END;

-- Every existing tree was built from TMDB's `/tv/{id}/season/{n}` walk,
-- under its default ordering. Recording that is what lets the next
-- migration's writer refuse a tree from a source the item does not own.
UPDATE library_items
   SET structure_source         = 'tmdb',
       structure_family         = 'default',
       structure_refreshed_at   = metadata_refreshed_at
 WHERE media_type = 'tv';


-- ## An episode's identity belongs to whoever numbered it
--
-- `tmdb_episode_id` is the right idea with a provider's name on it. The
-- pair is what a UNIQUE index can hold once two providers can each own a
-- tree: TheTVDB's episode 5345648 and TMDB's episode 5345648 are not the
-- same row, and without the source they would collide.
--
-- Nullable here only because this migration is additive. The next one
-- rebuilds the table with both NOT NULL — which is exactly what
-- `TreeEpisode.external_id` being a `String` rather than an `Option`
-- buys: a provider that cannot name its episodes cannot own a tree, so
-- there is no legitimate row without one.

ALTER TABLE library_episodes ADD COLUMN source          TEXT
    REFERENCES metadata_sources(label);
ALTER TABLE library_episodes ADD COLUMN external_id     TEXT;
-- Advisory, never a join key: TheTVDB gives absolute 13 to a Kaiju No. 8
-- special, so its S02E01 carries absolute 14 and an absolute-first
-- pairing shifts a whole season by one.
ALTER TABLE library_episodes ADD COLUMN absolute_number INTEGER;

UPDATE library_episodes
   SET source      = 'tmdb',
       external_id = CAST(tmdb_episode_id AS TEXT)
 WHERE tmdb_episode_id IS NOT NULL;

-- Partial, because the column fills in over a refresh cycle rather than
-- at once, and SQLite treats NULL as distinct from NULL — an unqualified
-- UNIQUE would not constrain the un-backfilled rows anyway.
CREATE UNIQUE INDEX idx_library_episodes_external
    ON library_episodes(item_id, source, external_id)
    WHERE external_id IS NOT NULL;


-- ## What the wipe must not take with it
--
-- The library tables are emptied in the migration that precedes the
-- re-import, and the \*arr restores the catalogue, the tree and every
-- file→episode pairing it made. Four things it cannot restore, because
-- they were never the \*arr's: which quality profile brarr scores this
-- title under, which root folder it writes to, how much of it the
-- operator wants chased, and when it entered the library.
--
-- Snapshotted here rather than in that migration so the rows exist for
-- the whole development of the phases in between, and can be inspected
-- long before anything is deleted.
--
-- **Per-season and per-episode flags are deliberately not snapshotted.**
-- They are keyed by number, and a number is exactly what changes when
-- the tree changes owner — reapplying them would land the anime series'
-- flags on the wrong episodes, which is the failure mode this whole
-- direction exists to end. `MonitorChoice::Mirror` restores them from
-- the \*arr instead, which is numbered the way the new tree will be.
CREATE TABLE library_restore_hints (
    media_type    TEXT    NOT NULL CHECK (media_type IN ('movie', 'tv')),
    -- The identity as it stood before the wipe. TMDB, because that is
    -- what every existing row is keyed on and what the re-import will
    -- catalogue under.
    tmdb_id       INTEGER NOT NULL,
    title         TEXT    NOT NULL,

    monitored     INTEGER NOT NULL CHECK (monitored IN (0, 1)),
    profile_id    TEXT,
    root_folder   TEXT,
    monitor_scope TEXT    NOT NULL,
    added_at      INTEGER NOT NULL,

    -- So reapplying is idempotent and a half-finished pass can resume.
    applied_at    INTEGER,

    PRIMARY KEY (media_type, tmdb_id)
    -- No FK to `quality_profiles`: the point of these rows is to outlive
    -- a table being emptied, and a profile deleted in the meantime should
    -- leave the hint readable rather than delete it.
) STRICT;

INSERT INTO library_restore_hints
        (media_type, tmdb_id, title, monitored, profile_id, root_folder,
         monitor_scope, added_at)
    SELECT media_type, tmdb_id, title, monitored, profile_id, root_folder,
           monitor_scope, added_at
      FROM library_items;
