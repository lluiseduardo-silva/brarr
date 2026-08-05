-- Two things `grabs` could not express, both needed at once.
--
-- 1. **A file that was never downloaded.** Adopting an existing library
--    means recording files brarr did not fetch, and those are neither
--    `torrent` nor `usenet`. Writing one of those anyway would poison
--    the column that `/queue` and `deliver` branch on. SQLite cannot
--    ALTER a CHECK constraint, so the table is rebuilt.
--
-- 2. **A season pack.** `season_number` has been there since the
--    library migration and nothing ever filled it, because nothing
--    could grab a pack. The interactive search can, and that exposes a
--    real defect in the barrier: `blocking_for` treated any grab with
--    `episode_id IS NULL` as covering *every* episode of the item, so a
--    pack of season 4 would have made season 5 look acquired. The fix
--    is in the query (see `db::grabs::blocking_for`), and the index
--    below is what keeps it fast.
--
-- The rebuild is the standard SQLite procedure. It is safe inside the
-- migration's transaction because nothing references `grabs` — every
-- foreign key here points outward.

CREATE TABLE grabs_new (
    id                TEXT    PRIMARY KEY NOT NULL,
    item_id           TEXT    NOT NULL,
    episode_id        TEXT,
    season_number     INTEGER,

    decision_id       TEXT,
    provider_id       TEXT,
    provider_name     TEXT    NOT NULL,

    release_id_remote TEXT    NOT NULL,
    release_name      TEXT    NOT NULL,
    download_url      TEXT,
    -- `local` = a file that was already on disk when brarr met it.
    protocol          TEXT    NOT NULL CHECK (protocol IN ('torrent', 'usenet', 'local')),

    client_id         TEXT,
    client_item_id    TEXT,

    status            TEXT    NOT NULL CHECK (status IN
                          ('reserved', 'sent', 'downloading', 'completed',
                           'imported', 'failed', 'rejected')),
    error             TEXT,
    imported_path     TEXT,
    file_missing_at   INTEGER,

    grabbed_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,

    FOREIGN KEY (item_id)     REFERENCES library_items(id)    ON DELETE CASCADE,
    FOREIGN KEY (episode_id)  REFERENCES library_episodes(id) ON DELETE SET NULL,
    FOREIGN KEY (decision_id) REFERENCES decisions(id)        ON DELETE SET NULL,
    FOREIGN KEY (provider_id) REFERENCES providers(id)        ON DELETE SET NULL,
    FOREIGN KEY (client_id)   REFERENCES download_clients(id) ON DELETE SET NULL
) STRICT;

INSERT INTO grabs_new (
    id, item_id, episode_id, season_number, decision_id, provider_id, provider_name,
    release_id_remote, release_name, download_url, protocol, client_id, client_item_id,
    status, error, imported_path, file_missing_at, grabbed_at, updated_at
)
SELECT
    id, item_id, episode_id, season_number, decision_id, provider_id, provider_name,
    release_id_remote, release_name, download_url, protocol, client_id, client_item_id,
    status, error, imported_path, file_missing_at, grabbed_at, updated_at
FROM grabs;

DROP TABLE grabs;
ALTER TABLE grabs_new RENAME TO grabs;

-- Same partial indexes as before the rebuild: a row whose file went
-- missing frees its barrier key without losing its history.
CREATE UNIQUE INDEX idx_grabs_unique_episode
    ON grabs(provider_id, release_id_remote, item_id, episode_id)
    WHERE episode_id IS NOT NULL AND file_missing_at IS NULL;

CREATE UNIQUE INDEX idx_grabs_unique_item
    ON grabs(provider_id, release_id_remote, item_id)
    WHERE episode_id IS NULL AND file_missing_at IS NULL;

CREATE INDEX idx_grabs_status     ON grabs(status);
CREATE INDEX idx_grabs_item       ON grabs(item_id);
CREATE INDEX idx_grabs_grabbed_at ON grabs(grabbed_at DESC);
CREATE INDEX idx_grabs_client     ON grabs(client_id);
CREATE INDEX idx_grabs_season     ON grabs(item_id, season_number);

CREATE INDEX idx_grabs_imported_present
    ON grabs(status)
    WHERE status = 'imported' AND file_missing_at IS NULL;
