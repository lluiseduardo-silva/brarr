-- Adopting files that were already on disk.
--
-- ## The barrier key for a file that has no provider
--
-- `NewGrab::provider_id` is a required `Uuid` because both tracker
-- indexes lead with that column and SQLite treats NULL as distinct from
-- NULL: a NULL there does not weaken the barrier, it removes it — the
-- same file could be adopted without limit, every adoption covering the
-- same episode.
--
-- A genuinely adopted file has no provider, and inventing a sentinel row
-- in `providers` would put a fake tracker on the providers screen, in
-- the search fan-out and in every provider metric. An invented UUID does
-- not work either: the foreign key is enforced (`db::open` turns on
-- `PRAGMA foreign_keys`). Local rows carry `provider_id = NULL`, which
-- leaves them outside both tracker indexes — harmless, since NULL
-- collides with nothing, including itself — and constrained here.
--
-- The key is (item, absolute path). Note what is *not* in it:
-- `episode_id`. One file must not be adoptable twice for the same item
-- under two different episodes, and including the episode would allow
-- exactly that — the mistake of an operator who fixes a wrong match by
-- adopting again instead of undoing first. The cost is that a genuine
-- multi-episode file cannot cover two episodes; brarr refuses that file
-- anyway.
--
-- `file_missing_at IS NULL` mirrors the other two indexes, for the same
-- reason as 20260804170000_file_check.sql: a file deleted outside brarr
-- frees the key and keeps the row.
--
-- Nothing in production has ever written `protocol = 'local'`, so this
-- index cannot fail over existing data. One statement, no table rebuild
-- and no new column: 20260805120000 prepared exactly this.

CREATE UNIQUE INDEX idx_grabs_unique_local
    ON grabs(item_id, release_id_remote)
    WHERE protocol = 'local' AND file_missing_at IS NULL;

-- ## Paths the operator told brarr to stop offering
--
-- Sonarr and Radarr forget an ignored file the moment the dialog closes.
-- That is fine for a one-off import of a finished download; it is wrong
-- for a torrents folder that only grows, where the same junk — samples,
-- extras, a rip nobody will ever catalogue — comes back on every single
-- import and has to be skipped by hand again.
--
-- The path is the key because the path is what the scan finds. It is
-- deliberately not tied to an item: at the moment the operator ignores a
-- file, the whole point is that it belongs to no item.
--
-- Ignoring is not deleting and not a decision about content, so this
-- carries no status and no reason — a row here means "do not offer this
-- again", and removing the row undoes it. The importer surfaces them
-- behind an `Ignorados (N)` filter, which is the way back.
CREATE TABLE ignored_paths (
    -- Absolute path, exactly as the scan reported it.
    path       TEXT    PRIMARY KEY NOT NULL,
    -- Unix seconds, like every other timestamp in this schema.
    ignored_at INTEGER NOT NULL
) STRICT;
