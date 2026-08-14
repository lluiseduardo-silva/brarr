-- # One episode identity, and it names who issued it
--
-- `20260809120000` added `tmdb_episode_id` because the tree was being
-- rebuilt from scratch on every metadata refresh — `DELETE` plus a
-- reinsert with fresh UUIDs — and `grabs.episode_id` is
-- `ON DELETE SET NULL`, so a TV library came unstuck from its files
-- every half hour. **The damage rendered as *complete*, not as
-- missing**, which is why a stable episode id was the fix.
--
-- `20260814120000` then replaced it with `(source, external_id)`: the
-- same identity, qualified by the provider that issued it. That
-- qualification is not decoration. A TheTVDB-owned tree has nowhere to
-- write its own episode id in a column named after TMDB, and two
-- providers' episode ids colliding numerically means nothing at all —
-- `structure::pair` had to carry an explicit `if source == Tmdb` guard
-- around the legacy column precisely to say so.
--
-- Both have been written on every accepted tree write since. Keeping the
-- older one is keeping a second source of truth for the exact fact whose
-- ambiguity caused the incident, so it goes.
--
-- `DROP INDEX` first: SQLite refuses `DROP COLUMN` for an indexed
-- column, and dropping the index removes the refusal. No table is
-- rebuilt — see `20260814130000` for why that matters here.

DROP INDEX idx_library_episodes_tmdb;
ALTER TABLE library_episodes DROP COLUMN tmdb_episode_id;
