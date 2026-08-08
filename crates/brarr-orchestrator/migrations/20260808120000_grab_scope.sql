-- ## What a grab is *about*, said out loud
--
-- Until now the answer was inferred from two nullable columns:
--
--     episode_id NOT NULL                    → one episode
--     episode_id NULL, season_number NOT NULL → a season pack
--     episode_id NULL, season_number NULL     → the whole item
--
-- The third row is the problem. It is the correct shape for a film, and
-- it is also what a per-episode grab *decays into* when something nulls
-- its `episode_id` — which `library_episodes`' `ON DELETE SET NULL` did
-- on every metadata refresh until 20260808 (see `db::library`). The
-- decayed row then reads as "covers the whole item", so a single file
-- answered for every episode of a series and the library rendered
-- **complete**. A false negative would have shown up as a red card; this
-- showed up as nothing at all.
--
-- Inferring intent from the absence of a value cannot distinguish "this
-- grab is about the item" from "this grab has lost what it was about".
-- Recording it removes the ambiguity by construction: a grab taken for
-- an episode stays scoped to an episode forever, and if its FK is ever
-- nulled again it covers *nothing* — honest, visible, and repairable —
-- instead of covering everything.
--
-- The value is derived at insert, never passed in: it is a function of
-- `(episode_id, season_number)` at reservation time, and letting callers
-- set it would just be a sixth way to get the encoding wrong.

ALTER TABLE grabs ADD COLUMN scope TEXT NOT NULL DEFAULT 'item'
    CHECK (scope IN ('item', 'season', 'episode'));

-- Backfill in the order the encoding is read, most specific first.
UPDATE grabs SET scope = 'episode' WHERE episode_id IS NOT NULL;

UPDATE grabs SET scope = 'season'
 WHERE episode_id IS NULL AND season_number IS NOT NULL;

-- The decayed rows. A grab of a **series** naming neither an episode nor
-- a season is not a whole-series acquisition — nothing in brarr creates
-- one. `scan::take_first_available` passes `season_number: None` and
-- takes the episode from the target, so a series grab always names an
-- episode; the interactive search is the only writer of packs and always
-- names the season. Both-NULL on a series can therefore only be a row
-- that lost its episode, and calling it `episode` is what stops it
-- answering for the other forty.
--
-- They are left with `episode_id IS NULL` deliberately: that is the set
-- `relink::run` looks for, and the repair fills the blank from the file
-- name, or `arr_import` fills it from the pairing Sonarr already did.
-- Marking them `item` would have been the silent option, and it is the
-- one that keeps the lie.
UPDATE grabs SET scope = 'episode'
 WHERE episode_id IS NULL
   AND season_number IS NULL
   AND item_id IN (SELECT id FROM library_items WHERE media_type = 'tv');

CREATE INDEX idx_grabs_scope ON grabs(scope);
