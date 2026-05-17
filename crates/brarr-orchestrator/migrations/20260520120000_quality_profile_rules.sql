-- Quality profiles gain a `rules_json` column persisting the rule list
-- the profile editor (P6.6) writes. Stored as TEXT — the orchestrator
-- runs `serde_json::to_string(&brarr_decision_service::RuleSet)` and
-- decodes back through the same `RuleSet` type so the engine constructs
-- without any DB-specific shape.
--
-- A `'[]'` default keeps legacy rows readable: the orchestrator treats
-- "empty rule list" as "fall back to Engine::baseline()" so a profile
-- created before this migration keeps behaving exactly as it did
-- before the rule-builder feature lands. Operators who do want
-- per-profile rules edit the row through the new editor and persist
-- a non-empty array.
--
-- The 5 preset rows seeded by 20260518120000_quality_profiles.sql get
-- a backfill with baseline-equivalent rules so an operator who clicks
-- "Edit" on, say, "FHD Dublado" sees the same scoring an unconfigured
-- profile would produce — making the editor feel like "tweak from the
-- baseline" instead of "start from nothing".
--
-- The baseline rule list mirrors `RuleSet::baseline()` in
-- brarr-decision-service. Keeping them in lockstep is a documented
-- invariant; the orchestrator has an integration test covering it.

ALTER TABLE quality_profiles
    ADD COLUMN rules_json TEXT NOT NULL DEFAULT '[]';

-- Backfill: serialised RuleSet::baseline() — JSON shape matches the
-- serde Serialize derive on RuleSet/Rule/Condition with kebab-case
-- enum filters. Hand-rolled to dodge an Engine bootstrap inside the
-- migration; the integration test guards the equivalence.
UPDATE quality_profiles
SET rules_json = '{"rule":[' ||
    '{"name":"PT-BR audio","when":{"audio":"pt-br"},"add_score":100,"tag":null,"reject":false},' ||
    '{"name":"PT-PT audio","when":{"audio":"pt-pt"},"add_score":25,"tag":null,"reject":false},' ||
    '{"name":"PT ambíguo (sem hint regional)","when":{"audio":"pt"},"add_score":50,"tag":null,"reject":false},' ||
    '{"name":"Legenda PT-BR","when":{"subtitle":"pt-br"},"add_score":50,"tag":null,"reject":false},' ||
    '{"name":"Legenda PT-PT","when":{"subtitle":"pt-pt"},"add_score":15,"tag":null,"reject":false},' ||
    '{"name":"HDR","when":{"hdr":true},"add_score":10,"tag":null,"reject":false},' ||
    '{"name":"Resolução 2160p","when":{"resolution":"exact-2160"},"add_score":20,"tag":null,"reject":false},' ||
    '{"name":"Resolução 1080p","when":{"resolution":"exact-1080"},"add_score":10,"tag":null,"reject":false}' ||
']}'
WHERE is_preset = 1;
