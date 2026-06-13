# Database maintenance & retention

The orchestrator's poller re-evaluates every `*arr`/wanted item on each
cycle and persists a `decisions` row per release. With the default
30-minute cadence and a couple hundred wanted items that is on the order
of 100k+ rows/day, so without retention the SQLite file grows without
bound (the original incident: ~2.74 GiB, 3.5M `decisions` rows in under a
month).

There are three layers of defense, from automatic to manual.

## 1. Automatic retention (background task)

A maintenance task ([`src/maintenance.rs`](../crates/brarr-orchestrator/src/maintenance.rs))
wakes every 6 hours and prunes `decisions` / `searches` older than the
retention window, then truncates the WAL and runs an incremental vacuum.

- **Window:** `BRARR_DECISIONS_RETENTION_DAYS` (default **7**). `0`
  disables pruning (keep forever).
- **Hot-reloadable:** edit it under **Configurações → Histórico &
  retenção** in the admin UI; the next cycle uses the new value, no
  restart.
- **Preserved rows:** any decision referenced by `push_history` (i.e.
  actually pushed to `*arr`) survives regardless of age, so the download
  proxy and the already-tried dedup keep working. Searches are only
  pruned once they have no decisions left.

Freed space is returned to the OS incrementally only when the database
was created with — or has since been `VACUUM`ed into —
`auto_vacuum=INCREMENTAL`. Fresh databases get this automatically
(`db::open` sets it); a pre-existing file converts on its first full
`VACUUM` (see §3).

## 2. On-demand maintenance (UI button / CLI)

To run a prune right now instead of waiting for the next cycle:

- **Admin UI:** **Configurações → Manutenção do banco**
  - *Podar agora* — prune at the saved window + reclaim.
  - *Compactar (VACUUM)* — full rewrite to shrink the file. Locks the DB;
    fine for the steady-state (small) database, **not** for the multi-GiB
    one — use §3 for that.
- **CLI (over gRPC):**
  ```bash
  brarr maintenance --addr 127.0.0.1:50051 [--token <BRARR_AUTH_TOKEN>] [--vacuum]
  ```
  Prints the number of `decisions` / `searches` rows removed.

## 3. One-time offline reduction (large existing DB)

A `VACUUM` on a multi-GiB file needs an exclusive lock plus ~2× free disk
and can run for minutes — do it **offline**, with the orchestrator
stopped, using [`scripts/db-maintenance.sql`](../scripts/db-maintenance.sql).

```bash
# 1. Stop the orchestrator (releases the DB lock).
docker compose -f docker-compose.prod.yml down

# 2. Prune + VACUUM the volume with any sqlite3-capable image.
docker run --rm \
  -v brarr-data:/data \
  -v "$PWD/scripts:/s:ro" \
  keinos/sqlite3 /data/brarr.db ".read /s/db-maintenance.sql"
# (prints before_bytes / after_bytes so you can confirm the shrink)

# 3. Start it again. From here the background task (§1) keeps it pruned,
#    and the auto_vacuum=INCREMENTAL conversion done by the VACUUM means
#    space is reclaimed on an ongoing basis without further full vacuums.
docker compose -f docker-compose.prod.yml up -d
```

The script defaults to a 7-day cutoff; edit the `'-7 days'` literals in
it if you want to keep more (or less) history for the one-time cleanup.
