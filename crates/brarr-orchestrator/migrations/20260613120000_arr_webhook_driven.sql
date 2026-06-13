-- Per-instance "webhook-driven" flag. When set, the scheduled poller
-- skips this *arr instance: the operator wired Sonarr/Radarr's
-- Connect → Webhook to brarr, so real-time events cover it and the
-- periodic /wanted sweep is redundant. The manual "rodar agora" button
-- still triggers a poll regardless of this flag.
ALTER TABLE arr_instances ADD COLUMN webhook_driven INTEGER NOT NULL DEFAULT 0;
