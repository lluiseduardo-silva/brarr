-- One-time / on-demand offline maintenance for brarr's SQLite database.
--
-- Prunes `decisions` / `searches` history older than the retention
-- window and physically shrinks the file. Run this with the
-- orchestrator STOPPED (it takes an exclusive lock during VACUUM).
--
-- The default window below is 7 days, matching
-- BRARR_DECISIONS_RETENTION_DAYS. Edit the two `'-7 days'` literals if
-- you want a different cutoff for the one-time cleanup.
--
-- Usage (host with sqlite3):
--     sqlite3 /path/to/brarr.db ".read scripts/db-maintenance.sql"
--
-- Usage (Docker volume, no sqlite3 on host):
--     docker run --rm \
--       -v brarr-data:/data \
--       -v "$PWD/scripts:/s:ro" \
--       keinos/sqlite3 /data/brarr.db ".read /s/db-maintenance.sql"
--
-- Invariants (same as the in-app prune):
--   * decisions referenced by push_history survive regardless of age
--     (keeps the *arr download proxy + already-tried dedup working);
--   * a search is only deleted once it has no decisions left.

.bail on
.timeout 60000

-- Convert the file to incremental auto_vacuum. This is a no-op flag on
-- its own; the VACUUM at the end is what actually rewrites the file in
-- the new mode so future `PRAGMA incremental_vacuum` calls (run by the
-- orchestrator's maintenance task) return free pages to the OS.
PRAGMA auto_vacuum = INCREMENTAL;

-- Show the size before, for a before/after comparison in the log.
SELECT 'before_bytes' AS label,
       (SELECT page_count FROM pragma_page_count) * (SELECT page_size FROM pragma_page_size) AS value;

DELETE FROM decisions
 WHERE decided_at < strftime('%s', 'now', '-7 days')
   AND NOT EXISTS (SELECT 1 FROM push_history ph WHERE ph.decision_id = decisions.id);

DELETE FROM searches
 WHERE submitted_at < strftime('%s', 'now', '-7 days')
   AND NOT EXISTS (SELECT 1 FROM decisions d WHERE d.search_id = searches.id);

-- Rewrite the file: reclaims all freed pages AND applies the
-- auto_vacuum=INCREMENTAL change set above.
VACUUM;

SELECT 'after_bytes' AS label,
       (SELECT page_count FROM pragma_page_count) * (SELECT page_size FROM pragma_page_size) AS value;
