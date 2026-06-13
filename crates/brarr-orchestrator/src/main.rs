//! `brarr-orchestrator` binary entry point.
//!
//! Reads configuration from environment variables and launches both the
//! gRPC and HTTP servers concurrently, wiring them to a shared
//! [`brarr_orchestrator::AppState`].
//!
//! ## Configuration precedence
//!
//! For every knob the operator can edit at runtime, the value comes
//! from (in order):
//!
//!   1. The `settings` table in the `SQLite` DB (edited via `/settings`).
//!   2. The matching `BRARR_*` env var (or `RUST_LOG`).
//!   3. A hard-coded sensible default.
//!
//! Once the runtime config is built, it lives inside
//! [`brarr_orchestrator::state::RuntimeConfig`] and is swapped
//! atomically on every save from the admin UI — no restart required
//! for token / bypass / public URL / poll interval / log level.
//! `RUST_BACKTRACE` is the one exception: persisted in the DB but
//! only applied on next start (Rust 2024 made `std::env::set_var`
//! unsafe and the workspace forbids `unsafe_code`).
//!
//! Environment variables (all optional, all overridable via UI):
//!
//! | Variable                       | Default                      | Purpose                              |
//! |--------------------------------|------------------------------|--------------------------------------|
//! | `BRARR_DB_PATH`                | `./brarr.db`                 | SQLite database path                 |
//! | `BRARR_HTTP_ADDR`              | `127.0.0.1:3000`             | bind address for the admin UI        |
//! | `BRARR_GRPC_ADDR`              | `127.0.0.1:50051`            | bind address for the gRPC service    |
//! | `BRARR_STATIC_DIR`             | `crates/brarr-orchestrator/static` | static asset directory         |
//! | `BRARR_AUTH_TOKEN`             | _(unset, auth disabled)_     | shared admin token for UI + gRPC     |
//! | `BRARR_BYPASS_AUTH_FROM`       | _(unset, no bypass)_         | trusted peer allowlist (CIDR/`loopback`/`private`) |
//! | `BRARR_TRUSTED_PROXIES`        | _(unset, XFF ignored)_       | reverse proxies whose `X-Forwarded-For` we honor   |
//! | `BRARR_PUBLIC_URL`             | _(derived from request)_     | external origin for *arr push URLs   |
//! | `BRARR_ARR_POLL_INTERVAL_SECS` | `1800`                       | autobrr-style auto-push cadence      |
//! | `BRARR_DECISIONS_RETENTION_DAYS`| `7`                         | history prune window (`0` = keep all)|
//! | `RUST_LOG`                     | `info`                       | tracing-subscriber env filter        |
//! | `RUST_BACKTRACE`               | `0`                          | error backtrace verbosity            |

#![allow(clippy::print_stdout, reason = "user-facing startup banner is fine")]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use brarr_decision_service::Engine;
use brarr_orchestrator::db::settings::{
    self, KEY_AUTH_TOKEN, KEY_BYPASS_AUTH_FROM, KEY_DECISIONS_RETENTION_DAYS, KEY_LOG_LEVEL,
    KEY_POLL_INTERVAL_SECS, KEY_PUBLIC_URL, KEY_TRUSTED_PROXIES,
};
use brarr_orchestrator::state::{LogReloader, RuntimeConfig};
use brarr_orchestrator::{
    AppState, AuthConfig, BypassConfig, TrustedPeers, db, grpc, maintenance, poll, web,
};
use tracing::warn;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, reload};

const DEFAULT_POLL_INTERVAL_SECS: u64 = 1800;
const DEFAULT_RETENTION_DAYS: u32 = 7;

#[tokio::main]
#[allow(
    clippy::too_many_lines,
    reason = "main is one linear story (env→DB→runtime→spawn); splitting hides the precedence cascade"
)]
async fn main() -> Result<()> {
    let log_reloader = init_tracing();

    let db_path = env_or("BRARR_DB_PATH", "./brarr.db");
    let http_addr: SocketAddr = env_or("BRARR_HTTP_ADDR", "127.0.0.1:3000")
        .parse()
        .context("BRARR_HTTP_ADDR must be a valid socket address")?;
    let grpc_addr: SocketAddr = env_or("BRARR_GRPC_ADDR", "127.0.0.1:50051")
        .parse()
        .context("BRARR_GRPC_ADDR must be a valid socket address")?;
    let static_dir = PathBuf::from(env_or(
        "BRARR_STATIC_DIR",
        "crates/brarr-orchestrator/static",
    ));

    let pool = db::open(&db_path)
        .await
        .with_context(|| format!("opening database at {db_path}"))?;

    // Merge DB-persisted settings on top of env vars so the operator's
    // last UI save wins over the container env, but a fresh install
    // still works from BRARR_* alone.
    let persisted = settings::get_all(&pool)
        .await
        .context("loading settings table")?;
    let lookup =
        |key: &str| -> Option<String> { persisted.get(key).cloned().filter(|s| !s.is_empty()) };

    let auth_token = lookup(KEY_AUTH_TOKEN)
        .or_else(|| std::env::var("BRARR_AUTH_TOKEN").ok())
        .filter(|s| !s.trim().is_empty());
    let auth = AuthConfig::from_optional(auth_token.as_deref());
    if !auth.is_enabled() {
        warn!(
            target: "brarr_orchestrator",
            "no admin token configured — admin UI and gRPC are unauthenticated. Set BRARR_AUTH_TOKEN or save one from /settings."
        );
    }

    let bypass_spec = lookup(KEY_BYPASS_AUTH_FROM)
        .or_else(|| std::env::var("BRARR_BYPASS_AUTH_FROM").ok())
        .unwrap_or_default();
    let proxies_spec = lookup(KEY_TRUSTED_PROXIES)
        .or_else(|| std::env::var("BRARR_TRUSTED_PROXIES").ok())
        .unwrap_or_default();
    let bypass = build_bypass(&bypass_spec, &proxies_spec)?;

    let public_url = lookup(KEY_PUBLIC_URL).or_else(|| std::env::var("BRARR_PUBLIC_URL").ok());

    let poll_interval = lookup(KEY_POLL_INTERVAL_SECS)
        .or_else(|| std::env::var("BRARR_ARR_POLL_INTERVAL_SECS").ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map_or_else(
            || Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS),
            |secs| Duration::from_secs(secs.max(60)),
        );

    let retention_days = lookup(KEY_DECISIONS_RETENTION_DAYS)
        .or_else(|| std::env::var("BRARR_DECISIONS_RETENTION_DAYS").ok())
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(DEFAULT_RETENTION_DAYS);

    // Apply a persisted log_level override on top of whatever env did
    // at tracing init — no-op when no DB row exists.
    if let Some(spec) = lookup(KEY_LOG_LEVEL)
        && let Err(e) = log_reloader.apply(&spec)
    {
        warn!(
            target: "brarr_orchestrator",
            spec = %spec,
            error = %e,
            "failed to apply persisted log_level setting; falling back to env"
        );
    }

    let runtime = RuntimeConfig {
        auth: ArcSwap::from_pointee(auth.clone()),
        bypass: ArcSwap::from_pointee(bypass),
        public_url: ArcSwap::from_pointee(public_url),
        poll_interval: ArcSwap::from_pointee(poll_interval),
        retention_days: ArcSwap::from_pointee(retention_days),
        log_reload: log_reloader,
    };
    let state = AppState::with_runtime(pool, Engine::baseline(), runtime);

    println!("brarr-orchestrator");
    println!("  http  → http://{http_addr}");
    println!("  grpc  → {grpc_addr}");
    println!("  db    → {db_path}");
    println!(
        "  auth  → {}",
        if auth.is_enabled() {
            "enabled"
        } else {
            "DISABLED (dev mode)"
        }
    );
    {
        let bypass_snapshot = state.bypass();
        let bypass_summary = if bypass_snapshot.is_disabled() {
            "off".to_string()
        } else {
            format!(
                "{} peer rule(s), {} proxy rule(s)",
                bypass_snapshot.peers.len(),
                bypass_snapshot.proxies.len()
            )
        };
        println!("  bypass → {bypass_summary}");
    }
    println!(
        "  poll  → every {}s (hot-reloadable via /settings)",
        state.poll_interval().as_secs()
    );
    println!(
        "  keep  → {} day(s) of history{}",
        state.retention_days(),
        if state.retention_days() == 0 {
            " (retention disabled)"
        } else {
            ""
        }
    );
    println!("  press Ctrl-C to stop");

    let web_state = state.clone();
    let grpc_state = state.clone();
    let poll_state = state.clone();
    let static_dir_clone = static_dir.clone();

    let web_task =
        tokio::spawn(async move { web::serve(web_state, http_addr, &static_dir_clone).await });
    let grpc_task = tokio::spawn(async move { grpc::serve(grpc_state, grpc_addr).await });
    // Long-lived poller. The handle is kept on the stack but never
    // joined explicitly — the task is aborted when the binary exits
    // (the runtime drops it as part of shutdown). Operationally we
    // don't want it to terminate the binary if the loop happens to
    // panic, so it's a fire-and-forget spawn.
    let _poll_handle = poll::spawn(poll_state);
    // Long-lived db janitor. Same fire-and-forget contract as the poller:
    // prunes history past the retention window and reclaims space.
    let _maint_handle = maintenance::spawn(state.clone());

    tokio::select! {
        res = web_task => res.context("web task panicked")?.context("web server")?,
        res = grpc_task => res.context("grpc task panicked")?.context("grpc server")?,
        () = shutdown_signal() => {
            println!("shutdown signal received — stopping");
        }
    }

    Ok(())
}

/// Initialise `tracing-subscriber` with a reloadable env filter and
/// return the [`LogReloader`] handle the settings UI can later use
/// to swap the spec at runtime.
fn init_tracing() -> LogReloader {
    let initial = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let (filter_layer, handle) = reload::Layer::new(initial);
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(tracing_subscriber::fmt::layer())
        .init();
    let handle = Arc::new(handle);
    LogReloader::new(move |spec: &str| {
        let new_filter = EnvFilter::try_new(spec)
            .map_err(|e| format!("invalid env-filter spec `{spec}`: {e}"))?;
        handle
            .reload(new_filter)
            .map_err(|e| format!("reload handle failed: {e}"))?;
        Ok(())
    })
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Build a [`BypassConfig`] from the two spec strings (each already
/// resolved through the DB-settings → env-var → empty cascade).
fn build_bypass(peers_spec: &str, proxies_spec: &str) -> Result<BypassConfig> {
    let peers = if peers_spec.trim().is_empty() {
        TrustedPeers::default()
    } else {
        TrustedPeers::parse(peers_spec)
            .map_err(|e| anyhow::anyhow!(e))
            .context("BRARR_BYPASS_AUTH_FROM / settings:bypass_auth_from")?
    };
    let proxies = if proxies_spec.trim().is_empty() {
        TrustedPeers::default()
    } else {
        TrustedPeers::parse(proxies_spec)
            .map_err(|e| anyhow::anyhow!(e))
            .context("BRARR_TRUSTED_PROXIES / settings:trusted_proxies")?
    };
    Ok(BypassConfig { peers, proxies })
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
