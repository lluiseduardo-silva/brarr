-- Add `plugin_path` to trackers.
--
-- When NULL → tracker is a direct UNIT3D HTTP client (legacy behaviour).
-- When set → tracker is loaded as a WASM plugin from this filesystem
-- path through `brarr_plugin_host::WasmTrackerProvider`. The plugin
-- ABI v1 is documented in `brarr-plugin-host`'s crate-level rustdoc.
--
-- The path is stored as written by the admin form; the orchestrator
-- resolves it relative to its current working directory at search
-- time and surfaces I/O failures into the per-tracker `failures` list
-- so one broken plugin can't abort the whole search.

ALTER TABLE trackers ADD COLUMN plugin_path TEXT;
