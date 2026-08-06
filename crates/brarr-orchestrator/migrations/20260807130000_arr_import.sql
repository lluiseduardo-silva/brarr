-- Importing the library from Sonarr/Radarr, and keeping them as a
-- passive source afterwards.
--
-- ## Why a root mapping and not a path mapping
--
-- An *arr reports every path in its own namespace. Measured on the
-- operator's stack: Sonarr answers `/data/Series/9-1-1/Season 1/…mkv`
-- while brarr mounts the same share at `/midias/Series`. This is the
-- same class of defect that once marked five finished downloads as
-- `failed` with the files intact on disk.
--
-- The fix is deliberately *not* another per-path rule table. An *arr has
-- one or two root folders and thousands of files under them, so the
-- operator maps the root — two or three choices for the whole migration
-- — and every path below it follows. Matching by folder name instead
-- (`/data/Series` looks like `/midias/Series`) is exactly how the bug
-- gets reintroduced: it is right until the day two roots share a name.
--
-- `ON DELETE CASCADE` on both sides: a mapping without its instance, or
-- without the brarr root it points at, is dead configuration, not
-- history worth keeping. That is the opposite of `grabs.client_id`,
-- which is `SET NULL` because a grab without a client is still a record
-- of something that happened.
CREATE TABLE arr_root_mappings (
    id              TEXT    PRIMARY KEY NOT NULL,
    arr_instance_id TEXT    NOT NULL,
    -- Absolute path as the *arr reports it, e.g. `/data/Series`.
    -- Deliberately not validated against the local disk: it names a
    -- directory on another machine and *should* not exist here.
    arr_path        TEXT    NOT NULL,
    -- Which brarr root folder it corresponds to. Its path is validated
    -- at registration, as `root_folders` already guarantees.
    root_folder_id  TEXT    NOT NULL,
    created_at      INTEGER NOT NULL,

    UNIQUE (arr_instance_id, arr_path),
    FOREIGN KEY (arr_instance_id) REFERENCES arr_instances(id) ON DELETE CASCADE,
    FOREIGN KEY (root_folder_id)  REFERENCES root_folders(id)  ON DELETE CASCADE
) STRICT;

CREATE INDEX idx_arr_root_mappings_instance ON arr_root_mappings(arr_instance_id);

-- ## Passive synchronisation
--
-- The operator's colleagues request titles through Seerr, which adds
-- them to Sonarr/Radarr. Until brarr speaks to Seerr directly, the *arr
-- stay on as a second data source: brarr reads their catalogues and
-- records the *wish*, paused.
--
-- This is a different axis from `enabled`, which governs the deprecated
-- push/poll path. An instance can be a sync source while being disabled
-- for everything else — which is exactly the state all three of the
-- operator's instances are in.
--
-- **The passive path never enables monitoring**, and that is not a
-- default: all three *arr still have indexers and download clients
-- pointed at the same qBittorrent and SABnzbd brarr uses, so a synced
-- title that arrived monitored would put two agents on the same 468
-- titles — the exact double agency that motivated removing the *arr from
-- the loop. The code expresses it by not accepting a monitoring
-- parameter on that path at all.
ALTER TABLE arr_instances ADD COLUMN sync_source INTEGER NOT NULL DEFAULT 0;

-- When the passive sweep last read this instance, so the UI can say
-- whether the wish list is fresh without inferring it from row
-- timestamps scattered across `library_items`.
ALTER TABLE arr_instances ADD COLUMN synced_at INTEGER;
