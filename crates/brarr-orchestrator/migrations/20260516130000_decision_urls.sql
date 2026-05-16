-- Persist the upstream download + details URLs on each decision row so
-- the Torznab outbound endpoint can serve a real `<enclosure url>`
-- instead of the `brarr:///download/<id>` placeholder. Sonarr/Radarr
-- fetch that URL directly to grab the `.torrent` / `.nzb`; without a
-- working URL the indexer is decorative.
--
-- Both columns are nullable: legacy rows pre-dating this migration
-- stay queryable, and providers that don't expose a download URL
-- (rare but possible) keep producing usable decisions for the admin
-- UI even though they can't be grabbed.

ALTER TABLE decisions ADD COLUMN download_url TEXT;
ALTER TABLE decisions ADD COLUMN details_url TEXT;
