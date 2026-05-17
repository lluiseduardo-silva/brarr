-- Persist the per-decision audio/subtitle language list snapshot so the
-- release card can render explicit `PT-BR áudio` / `PT-BR legenda` /
-- `Dublado` / `JP áudio + leg PT` chips without re-fetching MediaInfo.
--
-- The values are `serde_json::to_string(&Vec<brarr_core::Language>)`. We
-- store as TEXT (sqlite has no native JSON column) and the read path
-- parses on row hydration.
--
-- Legacy rows pre-dating this migration get `'[]'` via the column
-- default, so the read path doesn't need a `NULL` branch — empty vec is
-- the same "no enrichment captured" state.

ALTER TABLE decisions
    ADD COLUMN audio_langs_json TEXT NOT NULL DEFAULT '[]';

ALTER TABLE decisions
    ADD COLUMN subtitle_langs_json TEXT NOT NULL DEFAULT '[]';
