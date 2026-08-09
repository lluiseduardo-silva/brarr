-- ## The identity that survives a re-numbering
--
-- `library_episodes` has had two candidate keys and **neither survives
-- both changes an episode can go through**. The local UUID survives a
-- re-numbering but not a tree rebuild; the `(season_number,
-- episode_number)` pair survives a UUID churn but *is* what a
-- re-numbering changes. So an episode moving from `(1, 15)` to `(2, 1)`
-- was a delete plus an insert — and `grabs.episode_id` is
-- `ON DELETE SET NULL`, so the file it holds came unlinked.
--
-- TMDB's own episode id is stable across orderings. It already arrives
-- in the payload brarr fetches for every season and `EpisodeDto` simply
-- discarded it; recording it turns "this episode moved" into an UPDATE
-- of two integers on a row that lives, which is the whole reason a
-- series can now be re-numbered without losing what it holds.
--
-- Nullable, and the index is partial, because the column fills in over a
-- refresh cycle rather than at once: SQLite treats NULL as distinct from
-- NULL, so an unqualified UNIQUE would not constrain the un-backfilled
-- rows anyway, and NOT NULL would abort the upsert of every row that has
-- not been refreshed yet. Matching falls back to the number pair while
-- the value is missing, which is exactly the behaviour that shipped
-- before this column existed.

ALTER TABLE library_episodes ADD COLUMN tmdb_episode_id INTEGER;

CREATE UNIQUE INDEX idx_library_episodes_tmdb
    ON library_episodes(item_id, tmdb_episode_id)
    WHERE tmdb_episode_id IS NOT NULL;
