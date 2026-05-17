-- Quality Profiles — reusable scoring presets that an *arr instance can
-- attach to instead of carrying a bare integer threshold. The redesign
-- introduces this so a user with multiple Sonarrs (e.g. "Séries
-- dubladas" + "Animes legendados em JP") can centralise the cutoff
-- choice, name it, and point several instances at the same row.
--
-- The MVP carries only `push_threshold` per profile — the rule-builder
-- UI in the Figma mockup lands in a follow-up phase once we serialise
-- `brarr_decision_service::Rule` to JSON / TOML and store the array
-- here. For now the profile is effectively "a labelled threshold".
--
-- Migration strategy
-- ==================
-- * `arr_instances.profile_id` is nullable. Existing rows keep their
--   `push_threshold` column as the source of truth (nullable FK = no
--   profile). New flows can either attach a profile *or* keep using
--   the raw column; the engine resolves to the profile's threshold
--   when set, falling back to the row's own value otherwise.
-- * We do NOT drop `arr_instances.push_threshold` in this migration —
--   that comes once every active instance has been migrated to a
--   profile (separate maintenance migration).

CREATE TABLE quality_profiles (
    id              TEXT PRIMARY KEY NOT NULL,
    name            TEXT NOT NULL UNIQUE,
    description     TEXT,
    push_threshold  INTEGER NOT NULL DEFAULT 150 CHECK (push_threshold BETWEEN 0 AND 1000),
    is_preset       INTEGER NOT NULL DEFAULT 0 CHECK (is_preset IN (0, 1)),
    created_at      INTEGER NOT NULL
) STRICT;

-- Seed the five presets surfaced in the providers-page hint card so
-- a fresh install ships ready-to-attach profiles. Operators can edit
-- or delete them like any other row — `is_preset=1` is a hint for
-- the UI to badge them, not a write lock.
INSERT INTO quality_profiles (id, name, description, push_threshold, is_preset, created_at) VALUES
    ('00000000-0000-0000-0000-000000000001', 'Mínimo PT',
     'Aceita qualquer release com algum match PT (áudio ou legenda).',
     50,  1, strftime('%s', 'now')),
    ('00000000-0000-0000-0000-000000000002', 'FHD Dublado',
     '1080p com áudio PT-BR garantido. Padrão pra Radarr/Sonarr principal.',
     110, 1, strftime('%s', 'now')),
    ('00000000-0000-0000-0000-000000000003', 'FHD Dublado + Legenda',
     '1080p + áudio PT-BR + legenda PT-BR embarcada.',
     170, 1, strftime('%s', 'now')),
    ('00000000-0000-0000-0000-000000000004', '4K HDR Dublado',
     '2160p + HDR + áudio PT-BR. Premium tier.',
     130, 1, strftime('%s', 'now')),
    ('00000000-0000-0000-0000-000000000005', 'Premium (4K HDR + legenda)',
     'Top tier baseline — cobre o máximo prático do scoring.',
     200, 1, strftime('%s', 'now'));

-- Nullable FK on arr_instances. Sqlite's column-add doesn't support
-- inline `REFERENCES` enforcement (foreign_keys=ON pragma is per
-- connection), but we add it anyway for documentation + future
-- table-rebuild migrations.
ALTER TABLE arr_instances
    ADD COLUMN profile_id TEXT REFERENCES quality_profiles(id) ON DELETE SET NULL;

CREATE INDEX idx_arr_instances_profile_id ON arr_instances(profile_id);
