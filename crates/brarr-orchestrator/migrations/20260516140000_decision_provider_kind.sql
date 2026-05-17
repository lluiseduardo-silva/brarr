-- Snapshot the provider kind on each decision row so the Torznab
-- outbound feed can emit the right `<enclosure type>` per item:
--   * `application/x-nzb`           for Newznab (Usenet) providers
--   * `application/x-bittorrent`    for UNIT3D, Torznab, and WASM
--                                    plugin providers
--
-- Sonarr / Radarr added as a Torznab indexer filter out items typed
-- as `x-nzb` (they expect torrents); the same brarr instance added as
-- a Newznab indexer filters out the torrent items. Mixing the two on
-- one feed without the type discriminator confuses both *arr clients
-- into "everything is a torrent" and breaks Usenet downloads.
--
-- Legacy rows pre-dating this migration keep a NULL `provider_kind`;
-- the feed renderer falls back to `application/x-bittorrent` for
-- those, matching the historical behaviour.

ALTER TABLE decisions ADD COLUMN provider_kind TEXT;
