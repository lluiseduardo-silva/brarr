-- Remote path mapping: what the download client calls a directory, and
-- what brarr calls the same place on disk.
--
-- brarr runs in Docker. Its clients mount the media share at /data and
-- report finished downloads at /data/torrents/…; brarr mounts the same
-- share at /midias, because /data inside its image is already the
-- sqlite volume. The path the client reports simply does not exist for
-- brarr, and five finished downloads were marked failed while sitting
-- intact on disk.
--
-- Telling the operator to remount the container is not a fix: it
-- demands that everyone adopt one layout. The operator has to be free
-- to choose whatever bind they want. This table is the translation.
--
-- ## Why client_id and not a host string
--
-- Radarr/Sonarr key on Settings.Host — the string the operator retyped
-- into the client. localhost, 127.0.0.1, the container name and the IP
-- are four keys for one machine, nothing validates the coincidence, and
-- a silently orphaned mapping is the most reported failure of their
-- version. The upside is that one row serves every client on that host.
--
-- brarr already has download_clients with a stable id, grabs already
-- references it, and the import path already holds the row when it
-- needs the mapping — so the key costs nothing and that failure mode
-- stops existing. The price is one row per client instead of one per
-- machine: at brarr's scale (one qBittorrent, one SABnzbd) that is a
-- better trade than a string nobody can validate.
--
-- ## CASCADE here, SET NULL on grabs.client_id
--
-- A grab without a client is acquisition history worth keeping
-- (migration 20260804130000). A mapping without a client is dead,
-- invisible configuration: for_client is only ever called with a live
-- client_id.
--
-- ## remote_prefix never touches the filesystem
--
-- It names a directory on another machine, possibly under another
-- operating system's rules. /data/torrents *should* not exist here —
-- that is the entire reason the row exists. Only local_prefix is
-- validated (exists, is a directory, is readable), and only at
-- registration, for the same reason root_folders validates at
-- registration (20260804150000): a typo found at import time is a
-- download that already finished with nowhere to go, and the operator
-- learns about it hours later.
--
-- ## UNIQUE(client_id, remote_prefix)
--
-- remote_prefix is stored canonical: trimmed, repeated separators
-- collapsed, no trailing separator, `.`/`..` components resolved. There
-- is no UPDATE path — a mapping is added or removed, like root folders.
-- That structurally removes Radarr's asymmetry, where Add() normalises
-- and Update() does not.
CREATE TABLE path_mappings (
    id            TEXT    PRIMARY KEY NOT NULL,
    client_id     TEXT    NOT NULL REFERENCES download_clients(id) ON DELETE CASCADE,
    remote_prefix TEXT    NOT NULL,
    local_prefix  TEXT    NOT NULL,
    created_at    INTEGER NOT NULL,
    UNIQUE (client_id, remote_prefix)
) STRICT;

-- The import reads exactly this set, once per attempt.
CREATE INDEX idx_path_mappings_client ON path_mappings(client_id);

-- Waiting has to be visible.
--
-- ImportOutcome::Waiting writes nothing to the row (import.rs:246-248).
-- That was acceptable while it meant "the client is down for a minute",
-- and became the whole problem once it meant "you need to configure
-- something": the reason lived only in a debug! log, and five finished
-- downloads sat on disk with nothing in the UI able to say why.
--
-- We do not reuse `error`: that column means "this grab failed"
-- everywhere else in the code, and the entire point of this change is
-- that waiting is not failing. library_detail.html:242 renders g.error
-- in text-danger-soft-fg, and routes.rs:1271 already maps
-- Completed => "ok" — reusing it would produce a green pill beside a
-- red sentence. Same shape as file_missing_at: a column, not a status,
-- because the status is still correct.
ALTER TABLE grabs ADD COLUMN import_wait_reason TEXT;

-- And waiting must not kill the import queue.
--
-- import_pending() takes the first MAX_IMPORTS_PER_PASS (3) rows of
-- awaiting_import(), ordered by updated_at. Waiting does not move
-- updated_at (correctly: nothing changed state), so the sort key of a
-- grab that cannot advance does not move either. Three of those sit at
-- the head of a LIMIT 3 queue forever and every grab behind them is
-- never attempted.
--
-- That was survivable while an invisible path left the set immediately
-- by being marked failed. Now that it correctly waits, the starvation
-- is guaranteed — the incident produced five at once. The importer gets
-- its own clock: when it *looked*, not when the grab *changed*. This
-- also closes a pre-existing hazard: a grab whose blocking task panics
-- returns a hard AppError that aborts the pass (import.rs:198,
-- `import_grab(...)?`), and without the stamp it would be reselected
-- first on every following pass, forever.
ALTER TABLE grabs ADD COLUMN import_attempted_at INTEGER;

-- The importer reads exactly this set, in this order. NULL sorts first
-- in ASC on SQLite, so a freshly completed grab still cuts ahead of the
-- stuck set, and among the never-attempted the oldest still comes
-- first.
CREATE INDEX idx_grabs_awaiting_import
    ON grabs(import_attempted_at, updated_at)
    WHERE status = 'completed';
