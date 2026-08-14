-- The sweep's own bookkeeping: when each target was last searched.
--
-- `scan::run_once` spends a bounded number of searches per cycle, and it
-- used to spend them from a fixed head — items by `metadata_refreshed_at`,
-- then season and episode. Nothing ever moved, so the same targets were
-- searched every cycle forever, and those are by definition the ones that
-- never find anything: a target is wanted precisely because nothing was
-- found for it. Measured on this operator's catalogue: 294 wanted targets
-- against a ceiling of 25, so 269 of them had never been searched once —
-- including every episode that had just aired.
--
-- This is a table rather than a column on `library_items` /
-- `library_episodes` because it is not catalogue metadata. Those rows
-- already carry two categories that are kept apart on purpose (metadata is
-- a cache, monitoring is state); the sweep's attempt clock is a third, it
-- belongs to the sweep, and putting it there would have meant threading a
-- field through every fixture that constructs an `Episode`.
--
-- Deriving it from `searches` instead would have read plausibly and
-- decayed: that table is pruned on the retention window, so a target would
-- read as never-searched every time its history aged out — and the
-- Torznab pull path writes rows there that no sweep produced.
--
-- No retention sweep prunes this. It is bounded by the catalogue, one row
-- per target at most, and both foreign keys cascade — a deleted title or a
-- season TMDB dropped takes its attempts with it.
CREATE TABLE scan_attempts (
    item_id     TEXT    NOT NULL REFERENCES library_items(id)    ON DELETE CASCADE,
    episode_id  TEXT             REFERENCES library_episodes(id) ON DELETE CASCADE,
    searched_at INTEGER NOT NULL
) STRICT;

-- Two partial indexes rather than one composite, for the third time in
-- this schema (`grabs` did it twice): SQLite treats NULL as distinct from
-- NULL, so `UNIQUE(item_id, episode_id)` would let a movie — whose target
-- is the item, with no episode — accumulate a row per cycle.
CREATE UNIQUE INDEX idx_scan_attempts_episode
    ON scan_attempts(episode_id) WHERE episode_id IS NOT NULL;
CREATE UNIQUE INDEX idx_scan_attempts_item
    ON scan_attempts(item_id) WHERE episode_id IS NULL;
