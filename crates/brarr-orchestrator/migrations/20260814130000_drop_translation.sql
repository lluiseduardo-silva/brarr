-- # The translation goes, because it has nothing to translate
--
-- `20260808130000` created `library_episode_numbering` to hold the
-- difference between two numberings of one series: the catalogue's tree
-- came from a source that flattens Dragon Ball Super into one season of
-- 131, and every release names the 14/13/19/30/55 split that another
-- source publishes. `20260813130000` and `20260813140000` then added two
-- ways to derive that difference automatically, because picking an
-- ordering by hand per title is work nobody does.
--
-- All three were the right answer to the wrong question. A series whose
-- tree is built by whoever numbers it the way releases do has **no
-- difference to record** — the stored coordinate is at once the row
-- identity, the query sent to an indexer, the marker matched in a
-- release name, the name written to disk and the key Sonarr is paired
-- on. That is one value, and this table existed to keep two.
--
-- ## Why this is a `DROP COLUMN` and not a table rebuild
--
-- `library_items` has four children — `library_seasons`,
-- `library_episodes`, `library_item_ids` (CASCADE) and `grabs`
-- (SET NULL). `PRAGMA foreign_keys = OFF` is a **no-op
-- inside a transaction**, which is how sqlx runs every migration here,
-- so the 12-step rebuild would fire an implicit DELETE and take the
-- catalogue, the tree and every grab link with it. Nothing below
-- rebuilds anything.
--
-- `search_numbering_source` carries a CHECK, and SQLite refuses
-- `DROP COLUMN` for a column named in a *table*-level constraint. This
-- one is column-level, so the constraint is part of the definition and
-- goes with it — proven on this exact column against a copy of the
-- production database by `20260813140000`, which dropped and renamed it.

DROP TABLE library_episode_numbering;

-- Which ordering was applied, and its name for the screen. Both are
-- answered now by `library_items.structure_source` /
-- `structure_family` / `structure_handle`, which say who owns the tree
-- rather than what it is being translated into.
ALTER TABLE library_items DROP COLUMN search_group_id;
ALTER TABLE library_items DROP COLUMN search_group_name;

-- "May a sweep overwrite this?", asked of a translation that no longer
-- exists. `structure_pinned` is the same question about the tree, and it
-- is a boolean orthogonal to the value — which is what `'off'` had to
-- exist to fake here, and why `'off'` was a one-way door until
-- `reset_to_automatic` appeared.
ALTER TABLE library_items DROP COLUMN search_numbering_source;
