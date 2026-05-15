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
//! | `RUST_LOG`                     | `info`                       | tracing-subscriber env filter        |

#![allow(clippy::print_stdout, reason = "user-facing startup banner is fine")]

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use brarr_decision_service::Engine;
use brarr_orchestrator::{AppState, db, grpc, web};
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
    let state = AppState::new(pool, Engine::baseline());

    println!("brarr-orchestrator");
    println!("  http  → http://{http_addr}");
    println!("  grpc  → {grpc_addr}");
    println!("  db    → {db_path}");
    println!("  press Ctrl-C to stop");

    let web_state = state.clone();
    let grpc_state = state.clone();
    let static_dir_clone = static_dir.clone();

    let web_task =
        tokio::spawn(async move { web::serve(web_state, http_addr, &static_dir_clone).await });
    let grpc_task = tokio::spawn(async move { grpc::serve(grpc_state, grpc_addr).await });

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
