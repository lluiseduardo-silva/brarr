-- Download clients — the programs that actually move the bytes.
--
-- Second half of taking Radarr/Sonarr out of the loop. The library
-- migration (20260804120000) gave brarr its own answer to "what do I
-- want"; this one gives it somewhere to send a release once it decides.
-- Until now the only exit from the pipeline was `POST /release/push` at
-- an *arr, which is exactly the hand-off that let a second agent grab
-- the same title in parallel.
--
-- Two kinds in the first cut, one per protocol: qBittorrent for torrent
-- and SABnzbd for usenet, covering the UNIT3D and Newznab providers
-- that already exist. Transmission and Deluge are deliberately out.
--
-- There is no `protocol` column. It is a function of `kind`
-- (qbittorrent ⇒ torrent, sabnzbd ⇒ usenet), and a stored copy would be
-- one more thing that can disagree with itself; the mapping lives in
-- `brarr_download_client::DownloadClientKind::protocol`.

CREATE TABLE download_clients (
    id         TEXT    PRIMARY KEY NOT NULL,
    name       TEXT    NOT NULL UNIQUE,
    kind       TEXT    NOT NULL CHECK (kind IN ('qbittorrent', 'sabnzbd')),
    base_url   TEXT    NOT NULL,

    -- One nullable column per authentication scheme rather than a
    -- generic credentials blob: the set is closed (qBittorrent posts a
    -- username/password pair and gets a SID cookie back; SABnzbd takes
    -- an apikey query parameter), and typed columns keep the admin form
    -- honest about which field a given kind actually reads. Plaintext,
    -- same as `providers.api_token` and `arr_instances.api_key` — the
    -- file sits on local disk owned by the service user.
    username   TEXT,
    password   TEXT,
    api_key    TEXT,

    -- Category / label the client files the download under
    -- (qBittorrent "category", SABnzbd "category"). NULL leaves the
    -- client's own default in place.
    category   TEXT,

    -- Drain mode, mirroring `providers.enabled`: stop routing grabs here
    -- without losing the configuration.
    enabled    INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),

    -- Tie-break when more than one client serves the same protocol.
    -- Lowest wins, matching the *arr convention (1 = highest priority).
    priority   INTEGER NOT NULL DEFAULT 1,

    created_at INTEGER NOT NULL
) STRICT;

CREATE INDEX idx_download_clients_enabled ON download_clients(enabled);
CREATE INDEX idx_download_clients_kind    ON download_clients(kind);

-- The FK `grabs` was written without. `20260804120000_library.sql` left
-- this column out on purpose — a REFERENCES clause pointing at a table
-- that does not exist yet is not a constraint, it is a typo waiting to
-- be discovered. Now the table exists.
--
-- ON DELETE SET NULL, not CASCADE: deleting a download client must not
-- erase the acquisition history that went through it.
ALTER TABLE grabs ADD COLUMN client_id TEXT REFERENCES download_clients(id) ON DELETE SET NULL;

CREATE INDEX idx_grabs_client ON grabs(client_id);
