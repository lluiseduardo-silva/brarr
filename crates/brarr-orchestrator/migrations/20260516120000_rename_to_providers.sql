-- Rename `trackers` table to `providers`.
--
-- Reasoning: the term "tracker" only fits the original UNIT3D source
-- type. The orchestrator now serves multiple source kinds — UNIT3D
-- torrent trackers, Newznab Usenet indexers, Torznab gateways (Jackett,
-- Prowlarr), and WASM plugins — and exposes a Torznab indexer surface
-- itself. "Provider" is the more accurate umbrella term.
--
-- Three rename steps, applied in order:
--   1. The configuration table itself.
--   2. The FK column in `decisions` pointing at it.
--   3. The denormalized name snapshot column in `decisions`.
--
-- SQLite 3.25+ propagates the table rename through foreign-key
-- references in `sqlite_master`, so the existing `REFERENCES trackers(id)`
-- definition becomes `REFERENCES providers(id)` automatically. The
-- column renames need to be explicit.

ALTER TABLE trackers RENAME TO providers;
ALTER TABLE decisions RENAME COLUMN tracker_id TO provider_id;
ALTER TABLE decisions RENAME COLUMN tracker_name TO provider_name;
