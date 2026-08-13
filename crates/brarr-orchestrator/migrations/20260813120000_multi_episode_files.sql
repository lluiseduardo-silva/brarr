-- # One file, two episodes
--
-- A 40-minute release covering two episodes is normal, and both Sonarr
-- and Plex read `S05E33E34` as exactly what it says: episodes 33 and 34.
-- brarr could not. `idx_grabs_unique_local` was keyed on
-- `(item_id, release_id_remote)` — the file path — with the episode
-- outside the key, so the first reservation took the path and the second
-- was refused. `arr_import::join_episode_files` emits the pairing twice
-- on purpose and a test named the consequence out loud: the file "ends up
-- recorded against one episode".
--
-- The other episode is then uncovered forever. It does not read as
-- damage: the library says one episode is missing, the scanner goes
-- looking, and every release it finds is refused by the same barrier
-- because the path is already spoken for. The operator sees a permanent
-- gap over a file they have.
--
-- Two rows for one path is the honest record. The file is on disk once;
-- what it *covers* is two episodes, and coverage is the question
-- `grabs::blocking_for` and `coverage` both ask.
--
-- ## Why two partial indexes and not one composite
--
-- SQLite treats NULL as distinct from NULL, so a plain
-- `(item_id, release_id_remote, episode_id)` unique index would leave
-- every movie and every whole-item adoption unconstrained — the same
-- reason `20260804120000_library.sql` split its own barrier in two.
-- Splitting on `episode_id IS NULL` keeps both halves total.
--
-- Both keep `file_missing_at IS NULL`, so a file deleted outside brarr
-- still frees its key without losing its row (`20260804170000`).
--
-- Widening a unique index cannot fail over existing data: everything the
-- old index permitted, these permit.

DROP INDEX IF EXISTS idx_grabs_unique_local;

CREATE UNIQUE INDEX idx_grabs_unique_local_episode
    ON grabs(item_id, release_id_remote, episode_id)
    WHERE protocol = 'local' AND file_missing_at IS NULL AND episode_id IS NOT NULL;

CREATE UNIQUE INDEX idx_grabs_unique_local_whole
    ON grabs(item_id, release_id_remote)
    WHERE protocol = 'local' AND file_missing_at IS NULL AND episode_id IS NULL;
