-- ## Numbering, as a translation — never as a rewrite
--
-- TMDB and the scene do not always number a series the same way, and for
-- anime they usually do not. Measured on this operator's catalogue:
-- TMDB models Jujutsu Kaisen as **one** season of 59 episodes, while
-- every release in their own `decisions` table is named `S02E23`. brarr
-- asked for `S01E35`, `title_matches_episode` required that exact
-- marker, and every candidate was refused. 625 of the 1127 missing
-- episodes are on titles with that shape.
--
-- The marker was never the problem — the *numbers* were, and TMDB
-- already publishes the answer. An episode group carries, for each
-- episode, its position in the alternate ordering **and** its canonical
-- season/episode side by side, so the mapping is read, never inferred.
--
-- ## Why a table and not a rewrite of library_episodes
--
-- Those two columns are simultaneously four things: the row's identity
-- (`existing_tree` keys on them, `upsert_episode` conflicts on them),
-- the network coordinate, the file name on disk (`import::destination`),
-- and the pairing key with Sonarr and with `relink`. Renumbering moves
-- all four at once and only one of them is wanted.
--
-- Concretely, renumbering Dragon Ball Super would delete ~117 episode
-- rows and null ~117 `grabs.episode_id` in a single transaction — and
-- *both* repair paths are inert in exactly that case: `relink::run`
-- looks up a canonical key the tree no longer has, and
-- `arr_import::adopt_files` hits `counts.missing += 1; continue` before
-- reaching the repair, while `idx_grabs_unique_local` keeps the path key
-- occupied so re-adoption is refused. It would also not survive: the
-- passive \*arr sweep calls `sync_tree` for every series on every pass
-- and builds the canonical tree, so the rewrite has a half-life of one
-- cycle and each undo costs another round of orphans.
--
-- So the group lives beside the tree. Applying one writes here and sets
-- two columns on `library_items`; it writes **nothing** to
-- `library_episodes` and touches no grab. Reverting is one UPDATE.

CREATE TABLE library_episode_numbering (
    item_id           TEXT    NOT NULL,
    -- TMDB's group id is a hex string, not an integer, unlike every
    -- other id in that API.
    group_id          TEXT    NOT NULL,
    part_order        INTEGER NOT NULL,
    part_name         TEXT,
    -- What the release names it. `group_season` is the block's 1-based
    -- order; `group_episode` is the episode's 0-based position within
    -- its block, plus one. Stored rather than derived so the read path
    -- does no arithmetic and a change upstream cannot silently reshuffle
    -- what is already applied.
    group_season      INTEGER NOT NULL,
    group_episode     INTEGER NOT NULL,
    -- What the catalogue, the disk and Sonarr call it. Unchanged, ever.
    canonical_season  INTEGER NOT NULL,
    canonical_episode INTEGER NOT NULL,
    -- The identity that is stable across orderings. Recorded because it
    -- costs nothing here and is what a future join should use; nothing
    -- reads it yet, and `library_episodes` does not carry it.
    tmdb_episode_id   INTEGER,

    -- Lookup is canonical → group, one mapping per episode, one active
    -- ordering per title. Applying a different group replaces the rows.
    PRIMARY KEY (item_id, canonical_season, canonical_episode),
    FOREIGN KEY (item_id) REFERENCES library_items(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX idx_episode_numbering_item ON library_episode_numbering(item_id);

-- Which ordering is active, and its name for the screen. NULL is "the
-- canonical one", which is what every title starts as and what
-- "voltar à ordem original" returns to.
ALTER TABLE library_items ADD COLUMN search_group_id   TEXT;
ALTER TABLE library_items ADD COLUMN search_group_name TEXT;
