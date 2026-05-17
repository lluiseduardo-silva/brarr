-- Snapshot the provider-side upload timestamp on each decision row so
-- the Torznab outbound feed can emit a real `<pubDate>` per item. Without
-- this, brarr stamps every item with "now" and Sonarr / Radarr show
-- "Age: 0 minutes" for every result, making age-based filtering useless.
--
-- Mapping per provider:
--   * UNIT3D    → `attributes.created_at` (ISO 8601 string)
--   * Newznab   → `<newznab:attr name="usenetdate">` (Unix seconds)
--                 falling back to the `<pubDate>` RFC 2822 element
--   * Torznab   → same as Newznab (shared wire format)
--   * Plugin    → not yet exposed by the ABI; rows leave NULL
--
-- Storage is Unix seconds (i64), matching `decided_at`. Legacy rows
-- pre-dating this migration keep a NULL `published_at`; the feed
-- renderer falls back to `now()` for those, matching historical
-- behaviour.

ALTER TABLE decisions ADD COLUMN published_at INTEGER;
