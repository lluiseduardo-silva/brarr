-- Audit log of every release brarr pushed (or attempted to push) to
-- an *arr instance. One row per push attempt — both successes and
-- failures get persisted so the admin UI can show "what brarr asked
-- *arr to grab and how *arr responded".
--
-- `decision_id` FK to `decisions` so we can deep-link from the
-- decision detail page to its push history. `ON DELETE CASCADE` so
-- purging old search history doesn't leave orphan push rows.
--
-- `arr_instance_id` FK to `arr_instances` with `ON DELETE SET NULL`
-- so deleting an *arr config doesn't lose the audit trail of what was
-- pushed to it.
--
-- `status` is a free-form short string ('ok' / 'http_400' / 'transport_error' /
-- 'rejected'). Kept text rather than enum for forward-compat — adding a
-- new variant doesn't need a migration.
--
-- `response_body` carries the *arr-side rejection reason when present,
-- truncated to 1KiB at the application layer to bound row size.

CREATE TABLE push_history (
    id                TEXT PRIMARY KEY NOT NULL,
    decision_id       TEXT NOT NULL,
    arr_instance_id   TEXT,
    arr_instance_name TEXT NOT NULL,
    arr_kind          TEXT NOT NULL,
    pushed_at         INTEGER NOT NULL,
    status            TEXT NOT NULL,
    http_status       INTEGER,
    response_body     TEXT,
    FOREIGN KEY (decision_id) REFERENCES decisions(id) ON DELETE CASCADE,
    FOREIGN KEY (arr_instance_id) REFERENCES arr_instances(id) ON DELETE SET NULL
) STRICT;

CREATE INDEX idx_push_history_decision ON push_history(decision_id);
CREATE INDEX idx_push_history_pushed_at ON push_history(pushed_at DESC);
