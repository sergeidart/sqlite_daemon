mod protocol;
mod server;
mod single_instance;
mod worker;
mod router;

use anyhow::{Context, Result};
use router::Router;
use single_instance::SingleInstanceGuard;
use std::path::PathBuf;
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

    // Get database directory from args or use default
    let db_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .expect("Failed to get current directory")
        });

    info!(db_dir = %db_dir.display(), "Database directory");

    // Create router
    let router = Router::new(db_dir);

    // Run IPC server with router
    let server_result = server::run_server(PIPE_NAME, router).await;

    if let Err(e) = server_result {
        error!(error = %e, "Server error");
    }

    info!("Daemon shutdown complete");

    Ok(())
}
