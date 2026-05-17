-- Snapshot the *arr-side `rejections` array on each push history row.
-- Sonarr/Radarr return rejection reasons inside the HTTP 200 response
-- body when the title parser succeeded but the release fails a
-- downstream check (quality profile, custom format, library lookup,
-- queue dedup, etc.). Brarr already persists the full body in
-- `response_body`; this column carries the parsed reasons as a JSON
-- array (`["motivo 1", "motivo 2"]`) so the audit UI can render them
-- as a clean list instead of asking the operator to spelunk through
-- a 1.5 KiB ReleaseResource blob.
--
-- Stored as TEXT (JSON-encoded array of strings). NULL = brarr didn't
-- parse the body (legacy rows or transport-error pushes where there's
-- nothing to parse).

ALTER TABLE push_history ADD COLUMN rejections_json TEXT;
