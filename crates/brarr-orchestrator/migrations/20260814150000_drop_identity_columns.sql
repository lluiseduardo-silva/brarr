-- # Identity is a set, and the columns that were not one come out
--
-- `20260814120000` created `library_item_ids` and backfilled it from the
-- three columns below, then `library::upsert` wrote both forms while the
-- readers moved over. Both forms is the state this repository's defects
-- hide in, so it came with a date to close. This is the date.
--
-- ## What the columns made impossible
--
-- `tmdb_id INTEGER NOT NULL` with `UNIQUE(media_type, tmdb_id)` makes a
-- title only TheTVDB knows **unrepresentable** — and worse, because
-- `NewLibraryItem` derives `Default` such a title would arrive with
-- `tmdb_id = 0` and exactly ONE of them would fit in the whole table.
--
-- It also cost two live things. "Already in the library?" was a question
-- about one axis, so a series added through TMDB and met again on the
-- \*arr's TVDB axis read as absent and was catalogued a second time. And
-- `arr_import::resolve_tmdb_id` still calls `find_by_tvdb` per title to
-- get onto the TMDB axis before cataloguing, because until now there was
-- no other axis to catalogue on.
--
-- ## `status` is created here, not dropped
--
-- The design called for a neutral status column with a CHECK, and
-- `20260814120000` did not create it — the column was specified, the
-- backfill was written into the plan, and neither reached the migration.
-- Nothing caught it because nothing read the column: an omission is only
-- visible where a value is consumed, which is the same shape as a CSS
-- class that never matched a rule and a `Source` variant valid in Rust
-- and inert in a CHECK. So it arrives with its first reader.
--
-- What it replaces is `tmdb_status`: free text carrying ONE provider's
-- words — `Returning Series`, `Ended`, `Released`. TheTVDB says
-- `Continuing` for the first, and with no CHECK both dialects enter the
-- same column and every comparison starts depending on who wrote the row.
--
-- ## What stays, and why
--
-- `metadata_refreshed_at` is NOT identity — it is the TTL clock the
-- staleness sweep reads, and the only clock anything writes for the
-- descriptive facet. (`20260814120000` added `descriptive_refreshed_at`
-- and `structure_refreshed_at` as columns rather than the
-- `library_item_facets` table the plan drew; only the structure one is
-- written today.) Retiring it is a question about those three, not about
-- identity, and folding it in here would answer it by accident.
--
-- ## Mechanics
--
-- `DROP INDEX` before each `DROP COLUMN`: SQLite refuses to drop an
-- indexed column, and dropping the index removes the refusal. No table
-- is rebuilt — `library_items` has four children and
-- `PRAGMA foreign_keys = OFF` is a no-op inside the transaction sqlx
-- opens, so the 12-step rebuild would fire an implicit DELETE and take
-- the catalogue, the tree and every grab link with it.

-- Status in brarr's vocabulary, closed so that two providers' dialects
-- cannot both enter it.
ALTER TABLE library_items ADD COLUMN status TEXT
    CHECK (status IS NULL OR status IN
           ('returning','ended','cancelled','in-production','released','announced'));

UPDATE library_items SET status = CASE tmdb_status
    WHEN 'Returning Series' THEN 'returning'
    WHEN 'Ended'            THEN 'ended'
    WHEN 'Canceled'         THEN 'cancelled'
    WHEN 'Cancelled'        THEN 'cancelled'
    WHEN 'In Production'    THEN 'in-production'
    WHEN 'Post Production'  THEN 'in-production'
    WHEN 'Released'         THEN 'released'
    WHEN 'Planned'          THEN 'announced'
    WHEN 'Rumored'          THEN 'announced'
END;

DROP INDEX idx_library_items_tmdb;
DROP INDEX idx_library_items_imdb;
DROP INDEX idx_library_items_tvdb;

ALTER TABLE library_items DROP COLUMN tmdb_id;
ALTER TABLE library_items DROP COLUMN imdb_id;
ALTER TABLE library_items DROP COLUMN tvdb_id;
ALTER TABLE library_items DROP COLUMN tmdb_status;
