-- Root folders — where the library actually lives on disk.
--
-- First half of the import block, and the only half that touches no
-- files: this is the destination the importer will link into, declared
-- and validated up front rather than discovered when a download has
-- already finished.
--
-- `media_type` is nullable on purpose. Radarr and Sonarr are separate
-- programs, so each has its own root folders; brarr owns both kinds in
-- one catalogue, and an operator may well keep `/data/media/filmes` and
-- `/data/media/series` apart *or* point everything at one `/data/media`.
-- NULL means "serves either kind", and the selection rule prefers an
-- exact match over it.
CREATE TABLE root_folders (
    id         TEXT    PRIMARY KEY NOT NULL,
    -- Absolute path, stored as the operator typed it (minus a trailing
    -- separator). UNIQUE so the same folder cannot be registered twice
    -- with different kinds.
    path       TEXT    NOT NULL UNIQUE,
    media_type TEXT             CHECK (media_type IS NULL OR media_type IN ('movie', 'tv')),
    created_at INTEGER NOT NULL
) STRICT;

CREATE INDEX idx_root_folders_media_type ON root_folders(media_type);
