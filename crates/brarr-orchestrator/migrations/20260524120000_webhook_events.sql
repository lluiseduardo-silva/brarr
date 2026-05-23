-- Inbound webhook audit log. One row per *arr → brarr notification
-- ("Connect" webhook). Kept whether or not the event triggered a
-- search, so the admin can confirm the wiring is alive even for
-- ignored event types.
--
-- `arr_instance_id` FK to `arr_instances` with ON DELETE CASCADE:
-- deleting the *arr config also purges its inbound history (the rows
-- have no value once you can no longer identify which instance fired
-- them). FK on `triggered_search_id` uses SET NULL so cleaning up
-- search history doesn't break the audit trail of what arrived.
--
-- `kind` mirrors `brarr_arr::ArrKind`. `event_type` is the raw *arr
-- `eventType` field ('Test', 'MovieAdded', 'EpisodeAdded', etc.) —
-- free-form text so adding a new *arr event doesn't require a
-- migration.
--
-- `payload_json` stores the full incoming body verbatim so future
-- debugging can replay any decision brarr made from it.

CREATE TABLE webhook_events (
    id                   TEXT PRIMARY KEY NOT NULL,
    arr_instance_id      TEXT NOT NULL,
    kind                 TEXT NOT NULL CHECK (kind IN ('sonarr', 'radarr')),
    event_type           TEXT NOT NULL,
    payload_json         TEXT NOT NULL,
    received_at          INTEGER NOT NULL,
    triggered_search_id  TEXT,
    FOREIGN KEY (arr_instance_id) REFERENCES arr_instances(id) ON DELETE CASCADE,
    FOREIGN KEY (triggered_search_id) REFERENCES searches(id) ON DELETE SET NULL
) STRICT;

CREATE INDEX idx_webhook_events_received_at  ON webhook_events(received_at DESC);
CREATE INDEX idx_webhook_events_arr_instance ON webhook_events(arr_instance_id);
