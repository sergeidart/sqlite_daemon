mod actor;
mod db;
mod protocol;
mod server;
mod single_instance;

use anyhow::{Context, Result};
use single_instance::SingleInstanceGuard;
use std::path::PathBuf;
use tokio::sync::mpsc;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[cfg(windows)]
const PIPE_NAME: &str = r"\\.\pipe\SkylineDBd-v1";

#[cfg(unix)]
const PIPE_NAME: &str = "/tmp/skylinedb-v1.sock";

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();

    info!(
        version = env!("CARGO_PKG_VERSION"),
        pipe = PIPE_NAME,
        "Starting SQLite daemon"
    );

    // Acquire single-instance lock (prevents multiple daemons)
    let _instance_guard = SingleInstanceGuard::try_acquire()
        .context("Failed to acquire single-instance lock")?;

    // Get database path from args or use default
    let db_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let mut path = std::env::current_dir().unwrap();
            path.push("data.db");
            path
        });

    info!(db_path = %db_path.display(), "Database path");

    // Initialize database
    let pool = db::init_db(&db_path)
        .await
        .context("Failed to initialize database")?;

    // Create actor channel
    let (actor_tx, actor_rx) = mpsc::channel(1000);

    // Spawn write actor
    let db_path_str = db_path.display().to_string();
    let actor_handle = tokio::spawn(actor::actor_loop(actor_rx, pool, db_path_str));

    // Run IPC server
    let server_result = server::run_server(PIPE_NAME, actor_tx).await;

    if let Err(e) = server_result {
        error!(error = %e, "Server error");
    }

    // Wait for actor to finish
    info!("Waiting for actor to finish...");
    let _ = actor_handle.await;

    info!("Daemon shutdown complete");

    Ok(())
}
