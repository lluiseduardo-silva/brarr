-- Media servers — the last thing the *arr did that brarr had not.
--
-- Radarr and Sonarr call it Connect → Plex Media Server and Connect →
-- Emby/Jellyfin: after an import, tell the media server where to look so
-- the file shows up now instead of whenever it next scans on its own.
-- brarr took over searching, grabbing, importing and renaming, and this
-- notification left with the *arr. The three instances on this stack
-- still carry those connections, configured and pointed at a Plex that
-- brarr no longer feeds.
--
-- ## Three kinds, two dialects, and no column for the second one
--
-- Jellyfin and Emby speak one API — Sonarr has no `Notifications/
-- Jellyfin/` directory and `MediaBrowserSettings` has no server-type
-- field. `kind` names all three because the operator has to say what
-- they are pointing at; which dialect that implies is derived by
-- `MediaServerKind::api()`. Same rule as `download_clients`, which has
-- no `protocol` column for the same reason: a stored copy of something
-- derivable is one more thing that can disagree with itself.
--
-- ## One nullable `token`, not one column per kind
--
-- `download_clients` has username/password/api_key because its two
-- kinds genuinely authenticate differently. Here all three send a single
-- opaque string; only the header name changes (`X-Plex-Token` vs
-- `X-MediaBrowser-Token`), and a header name is not configuration.
--
-- Where they do differ is *how the operator gets it*: Jellyfin and Emby
-- print a key in their admin panel, and Plex does not — the credential
-- is an account token obtained by sending a person to plex.tv to approve
-- a PIN. That flow needs one durable value, `plex_client_identifier`,
-- which lives in `settings` rather than here: it identifies this brarr
-- install to plex.tv and must be identical across every Plex row and
-- every call, forever. A per-row copy is precisely the way to orphan a
-- token.
--
-- ## last_notified_at / last_error are on the row on purpose
--
-- The alternative was an audit table in the shape of `push_history` or
-- `webhook_events`. It was refused: a refresh is idempotent and
-- self-healing (the media server scans on its own eventually), so the
-- history has no forensic value the row does not, and `decisions` is the
-- standing reminder of what an unbounded log costs — 2.95 GB of file for
-- 28 MB of data. What the operator actually needs is one answer, on the
-- screen where the server is configured: did the last one work?
--
-- `last_error` is TEXT and nullable, and a successful notification
-- clears it. Both are written best-effort by the notify path: failing to
-- record a notification must never fail an import.
CREATE TABLE media_servers (
    id              TEXT    PRIMARY KEY NOT NULL,
    name            TEXT    NOT NULL UNIQUE,
    kind            TEXT    NOT NULL CHECK (kind IN ('plex', 'jellyfin', 'emby')),
    base_url        TEXT    NOT NULL,

    -- `X-Plex-Token` or `X-MediaBrowser-Token`. Plaintext, same as
    -- `download_clients.api_key` and `arr_instances.api_key` — the file
    -- sits on local disk owned by the service user.
    token           TEXT,

    -- Drain mode, mirroring `download_clients.enabled`: stop notifying
    -- without losing the configuration.
    enabled         INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),

    created_at      INTEGER NOT NULL,

    -- The last outcome, which is the whole audit trail this feature gets.
    last_notified_at INTEGER,
    last_error       TEXT
) STRICT;

CREATE INDEX idx_media_servers_enabled ON media_servers(enabled);

-- The same translation as `path_mappings`, pointed the other way.
--
-- `path_mappings` answers "the download client said /data/torrents/x —
-- where is that for me?". This answers "I wrote /midias/Filmes/X — what
-- does the media server call it?". Measured on this stack, one directory
-- has three names: the host and Plex see /mnt/midias, the *arr see
-- /data, and brarr's container mounts it at /midias. A refresh naming
-- brarr's spelling matches no Plex section at all.
--
-- Column names keep `path_mappings`' meaning exactly — `remote_prefix`
-- is what the *other* side writes, `local_prefix` is brarr's — so the
-- rows feed `remote_path::PrefixRule` without a second vocabulary. The
-- matcher is shared too (`remote_path::to_remote`), because the sharp
-- edges were paid for once: component boundaries, longest-wins, and a
-- backslash being a legal POSIX filename character.
--
-- CASCADE, like `path_mappings` and unlike `grabs.client_id`: a grab
-- without a client is history worth keeping, a mapping without a server
-- is dead configuration.
--
-- No UPDATE path, again like `path_mappings`: a mapping is added or
-- removed, which is what keeps `remote_prefix` canonical by
-- construction. And only `local_prefix` is validated against this
-- filesystem — `remote_prefix` names a directory on another machine and
-- *should* not exist here.
CREATE TABLE media_server_path_mappings (
    id            TEXT    PRIMARY KEY NOT NULL,
    server_id     TEXT    NOT NULL REFERENCES media_servers(id) ON DELETE CASCADE,
    remote_prefix TEXT    NOT NULL,
    local_prefix  TEXT    NOT NULL,
    created_at    INTEGER NOT NULL,
    UNIQUE (server_id, remote_prefix)
) STRICT;

CREATE INDEX idx_media_server_path_mappings_server ON media_server_path_mappings(server_id);
