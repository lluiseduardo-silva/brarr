//! `brarr-orchestrator` binary entry point.
//!
//! Reads configuration from environment variables and launches both the
//! gRPC and HTTP servers concurrently, wiring them to a shared
//! [`brarr_orchestrator::AppState`].
//!
//! Environment variables (all optional):
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
//! | `RUST_LOG`                     | `info`                       | tracing-subscriber env filter        |

#![allow(clippy::print_stdout, reason = "user-facing startup banner is fine")]

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use brarr_decision_service::Engine;
use brarr_orchestrator::{AppState, AuthConfig, BypassConfig, TrustedPeers, db, grpc, poll, web};
use tracing::warn;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

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
    let auth = AuthConfig::from_optional(std::env::var("BRARR_AUTH_TOKEN").ok().as_deref());
    if !auth.is_enabled() {
        warn!(
            target: "brarr_orchestrator",
            "BRARR_AUTH_TOKEN is unset — admin UI and gRPC are unauthenticated. Set it for production deployments."
        );
    }
    let bypass = load_bypass()?;
    if !bypass.is_disabled() {
        tracing::info!(
            target: "brarr_orchestrator",
            peer_rules = bypass.peers.len(),
            proxy_rules = bypass.proxies.len(),
            "auth bypass configured — requests from listed peers will skip the cookie/apikey check"
        );
    }
    let state = AppState::with_auth_and_bypass(pool, Engine::baseline(), auth.clone(), bypass);

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
    let bypass_summary = if state.bypass().is_disabled() {
        "off".to_string()
    } else {
        format!(
            "{} peer rule(s), {} proxy rule(s)",
            state.bypass().peers.len(),
            state.bypass().proxies.len()
        )
    };
    println!("  bypass → {bypass_summary}");
    let poll_interval = poll::interval_from_env();
    println!(
        "  poll  → every {}s (set BRARR_ARR_POLL_INTERVAL_SECS to tune)",
        poll_interval.as_secs()
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
    let _poll_handle = poll::spawn(poll_state, poll_interval);

    tokio::select! {
        res = web_task => res.context("web task panicked")?.context("web server")?,
        res = grpc_task => res.context("grpc task panicked")?.context("grpc server")?,
        () = shutdown_signal() => {
            println!("shutdown signal received — stopping");
        }
    }

    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Build the [`BypassConfig`] from the two env vars
/// (`BRARR_BYPASS_AUTH_FROM`, `BRARR_TRUSTED_PROXIES`). Unset vars
/// produce empty lists. Bad entries crash startup with a clear message
/// so misconfiguration is loud.
fn load_bypass() -> Result<BypassConfig> {
    let peers = match std::env::var("BRARR_BYPASS_AUTH_FROM").ok() {
        Some(spec) if !spec.trim().is_empty() => TrustedPeers::parse(&spec)
            .map_err(|e| anyhow::anyhow!(e))
            .context("BRARR_BYPASS_AUTH_FROM")?,
        _ => TrustedPeers::default(),
    };
    let proxies = match std::env::var("BRARR_TRUSTED_PROXIES").ok() {
        Some(spec) if !spec.trim().is_empty() => TrustedPeers::parse(&spec)
            .map_err(|e| anyhow::anyhow!(e))
            .context("BRARR_TRUSTED_PROXIES")?,
        _ => TrustedPeers::default(),
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
