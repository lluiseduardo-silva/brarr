-- Soft-disable flag for providers. Mirrors `arr_instances.enabled` —
-- operator can take a tracker out of the search fan-out without
-- losing its config (URL, apikey, kind, plugin path). Useful for
-- targeted testing ("force this poll to only hit NZBGeek") and for
-- temporarily silencing a flaky upstream without re-entering credentials.
--
-- Default 1 = legacy rows stay enabled after the migration runs, so
-- nothing changes silently in production.

ALTER TABLE providers ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1));

CREATE INDEX idx_providers_enabled ON providers(enabled);
