-- Persist a per-profile score map alongside each decision so the UI
-- can surface the score that a custom Quality Profile rule list would
-- produce, not just the baseline engine output. The release card
-- displays max(baseline, profile_scores) so an anime release with
-- "JP audio +80 / PT-sub +150 / 1080p+ +100" rules suddenly reads as
-- 330 instead of the 60 the baseline gives it.
--
-- Storage shape: `{"<profile-uuid>": 330, "<other-uuid>": 200}`. Empty
-- map (`'{}'` default) means "search ran before any profile existed,
-- fall back to baseline `score` column". The orchestrator's
-- `decision_view()` resolves the displayed score by taking the max of
-- baseline + every value in this map.

ALTER TABLE decisions
    ADD COLUMN profile_scores_json TEXT NOT NULL DEFAULT '{}';
