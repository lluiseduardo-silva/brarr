-- Reconciling the catalogue with the disk.
--
-- Until now "do I already have this?" was answered entirely from
-- `grabs`: anything not `failed`/`rejected` meant the item was taken
-- care of. That was a deliberate simplification, and it had one known
-- lie — a file deleted outside brarr left the item out of the sweep
-- forever, because nothing ever looked at the disk.
--
-- `grabs.imported_path` (migration 20260804160000) made looking possible.
-- This column records the answer.
ALTER TABLE grabs ADD COLUMN file_missing_at INTEGER;

-- The barrier indexes have to exclude those rows, and this is the whole
-- reason for a column rather than a status.
--
-- Marking the grab `failed` would let the scanner search again, but the
-- unique key would stay occupied — so the one release the operator
-- actually had could never be re-acquired, which is usually the exact
-- release they want back. Deleting the row would free the key and
-- destroy the acquisition history. A partial index that skips
-- `file_missing_at IS NOT NULL` does both: the key is free, the row
-- stays.
DROP INDEX idx_grabs_unique_episode;
DROP INDEX idx_grabs_unique_item;

CREATE UNIQUE INDEX idx_grabs_unique_episode
    ON grabs(provider_id, release_id_remote, item_id, episode_id)
    WHERE episode_id IS NOT NULL AND file_missing_at IS NULL;

CREATE UNIQUE INDEX idx_grabs_unique_item
    ON grabs(provider_id, release_id_remote, item_id)
    WHERE episode_id IS NULL AND file_missing_at IS NULL;

-- The verification pass reads exactly this set.
CREATE INDEX idx_grabs_imported_present
    ON grabs(status)
    WHERE status = 'imported' AND file_missing_at IS NULL;
