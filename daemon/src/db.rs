use anyhow::{Context, Result};
use sqlx::{sqlite::SqliteConnectOptions, SqlitePool};
use std::path::Path;
use std::str::FromStr;
use tracing::info;

/// Initialize SQLite with WAL mode and proper settings
pub async fn init_db(db_path: &Path) -> Result<SqlitePool> {
    let db_url = format!("sqlite:{}", db_path.display());
    
    let options = SqliteConnectOptions::from_str(&db_url)?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .busy_timeout(std::time::Duration::from_secs(5));

    let pool = SqlitePool::connect_with(options)
        .await
        .context("Failed to connect to database")?;

    // Apply additional pragmas
    sqlx::query("PRAGMA wal_autocheckpoint=1000")
        .execute(&pool)
        .await?;

    info!(
        db_path = %db_path.display(),
        "Database initialized with WAL mode"
    );

    // Run migrations
    run_migrations(&pool).await?;

    Ok(pool)
}

/// Run database migrations
async fn run_migrations(pool: &SqlitePool) -> Result<()> {
    // Create meta table for revision tracking
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS meta (
            rev INTEGER NOT NULL PRIMARY KEY,
            ts INTEGER NOT NULL
        )
        "#,
    )
    .execute(pool)
    .await?;

    // Initialize revision to 0 if not exists
    sqlx::query(
        r#"
        INSERT INTO meta(rev, ts)
        SELECT 0, CAST(strftime('%s','now') AS INTEGER)
        WHERE NOT EXISTS(SELECT 1 FROM meta)
        "#,
    )
    .execute(pool)
    .await?;

    info!("Migrations completed");

    Ok(())
}

/// Get current revision number
pub async fn get_current_rev(pool: &SqlitePool) -> Result<i64> {
    let rev: i64 = sqlx::query_scalar("SELECT rev FROM meta")
        .fetch_one(pool)
        .await?;
    Ok(rev)
}
