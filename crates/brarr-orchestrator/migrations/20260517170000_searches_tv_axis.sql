-- Persist the TV search axis in the `searches` audit table.
--
-- Until this migration brarr persisted only `tmdb_id` + `imdb_id` on
-- every search row, even when the search was driven by the Sonarr
-- wanted-episodes poll (which uses TVDB id + season + episode). All
-- per-episode searches landed in the table indistinguishable from
-- one another — `{"tmdb_id":null,"imdb_id":null}`. Operators had no
-- way to ask "did brarr try to grab The Rookie S08E14?" from the DB.
--
-- New columns are nullable; legacy rows stay valid (they were movie
-- searches and never had a TV axis).

ALTER TABLE searches ADD COLUMN tvdb_id INTEGER;
ALTER TABLE searches ADD COLUMN season INTEGER;
ALTER TABLE searches ADD COLUMN episode INTEGER;

-- Help the diagnostic queries the operator runs to investigate why a
-- specific episode never pushed — `WHERE tvdb_id = ? AND season = ?
-- AND episode = ?`.
CREATE INDEX idx_searches_tv_axis ON searches(tvdb_id, season, episode);
