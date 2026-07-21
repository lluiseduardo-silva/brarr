-- Observability for the search pipeline and the *arr-facing indexer
-- endpoints.
--
-- `provider_metrics`: one row per provider per search fan-out — how long
-- the upstream API took, whether it succeeded / errored / hit the
-- per-provider budget, and how many releases it returned. This is the
-- ground truth for "which tracker is the bottleneck".
--
-- `endpoint_metrics`: one row per request to the `/torznab/*` and
-- `/newznab/*` surfaces — total handler latency as Sonarr/Radarr see it,
-- the `t=` function, HTTP status, and whether the short-TTL search cache
-- absorbed the request (`hit`) or a full fan-out ran (`miss`; NULL for
-- non-search functions like `caps` and `download`).
--
-- Both FKs use ON DELETE SET NULL: pruning `searches` / deleting a
-- provider must not take the health history down with it. Rows have
-- their own retention (pruned alongside decisions by the maintenance
-- task).

CREATE TABLE provider_metrics (
    id            TEXT PRIMARY KEY NOT NULL,
    search_id     TEXT,
    provider_id   TEXT,
    provider_name TEXT NOT NULL,
    provider_kind TEXT NOT NULL,
    outcome       TEXT NOT NULL CHECK (outcome IN ('ok', 'error', 'timeout')),
    error         TEXT,
    duration_ms   INTEGER NOT NULL,
    release_count INTEGER NOT NULL DEFAULT 0,
    recorded_at   INTEGER NOT NULL,
    FOREIGN KEY (search_id) REFERENCES searches(id) ON DELETE SET NULL,
    FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE SET NULL
) STRICT;

CREATE INDEX idx_provider_metrics_recorded ON provider_metrics(recorded_at DESC);
CREATE INDEX idx_provider_metrics_name     ON provider_metrics(provider_name);

CREATE TABLE endpoint_metrics (
    id          TEXT PRIMARY KEY NOT NULL,
    endpoint    TEXT NOT NULL CHECK (endpoint IN ('torznab', 'newznab')),
    function    TEXT NOT NULL,
    status      INTEGER NOT NULL,
    duration_ms INTEGER NOT NULL,
    cache       TEXT CHECK (cache IN ('hit', 'miss')),
    recorded_at INTEGER NOT NULL
) STRICT;

CREATE INDEX idx_endpoint_metrics_recorded ON endpoint_metrics(recorded_at DESC);
