-- Sonarr / Radarr instances brarr can push releases to (autobrr-style
-- inversion). Today brarr is a pull target for *arr; this table flips
-- the relationship so brarr can notify the matching *arr instance via
-- `POST /api/v3/release/push` when its rules engine accepts a release.
--
-- Each row is one configured *arr (one Sonarr, one Radarr, or multiple
-- of either if the operator runs split stacks). `kind` discriminates
-- the flavour. `push_threshold` is the minimum [`DecisionScore`] (0..=1000)
-- that warrants an auto-push — releases scoring below it are persisted
-- to `decisions` as usual but never pushed. `enabled=0` short-circuits
-- the push without deleting the row, useful for "drain mode".
--
-- `api_key` stored as plaintext to match the existing `providers` row
-- pattern. Encryption-at-rest is a future hardening.

CREATE TABLE arr_instances (
    id              TEXT PRIMARY KEY NOT NULL,
    name            TEXT NOT NULL UNIQUE,
    kind            TEXT NOT NULL CHECK (kind IN ('sonarr', 'radarr')),
    base_url        TEXT NOT NULL,
    api_key         TEXT NOT NULL,
    push_threshold  INTEGER NOT NULL DEFAULT 700 CHECK (push_threshold BETWEEN 0 AND 1000),
    enabled         INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at      INTEGER NOT NULL
) STRICT;

CREATE INDEX idx_arr_instances_kind ON arr_instances(kind);
CREATE INDEX idx_arr_instances_enabled ON arr_instances(enabled);
