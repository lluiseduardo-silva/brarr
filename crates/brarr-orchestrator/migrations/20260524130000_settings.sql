-- Runtime settings persisted by the admin UI's /settings page. One
-- row per configurable knob; key/value text so adding a new setting
-- doesn't require a schema migration. Loaded once at startup and
-- merged on top of the BRARR_* env vars (DB wins when present).
--
-- Known keys (informational — schema doesn't enforce):
--   auth_token          opaque admin token (replaces BRARR_AUTH_TOKEN)
--   bypass_auth_from    same grammar as BRARR_BYPASS_AUTH_FROM
--   trusted_proxies     same as BRARR_TRUSTED_PROXIES
--   public_url          replaces BRARR_PUBLIC_URL (no trailing slash)
--   poll_interval_secs  replaces BRARR_ARR_POLL_INTERVAL_SECS
--   log_level           replaces RUST_LOG (env-filter syntax)
--   backtrace           replaces RUST_BACKTRACE — restart required to
--                       take effect (Rust 2024 std::env::set_var is
--                       unsafe and the workspace forbids unsafe).
--
-- updated_at is unix seconds, kept for audit / display only — no app
-- logic depends on the timestamp.

CREATE TABLE settings (
    key         TEXT PRIMARY KEY NOT NULL,
    value       TEXT NOT NULL,
    updated_at  INTEGER NOT NULL
) STRICT;
