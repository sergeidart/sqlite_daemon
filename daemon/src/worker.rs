use crate::protocol::{Request, Response, Statement, TransactionMode};
use anyhow::{bail, Context, Result};
use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

const WORKER_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60); // 5 minutes

pub enum WorkerCommand {
    Request {
        req: Request,
        reply: oneshot::Sender<Response>,
    },
}

enum DatabaseState {
    Open(SqlitePool),
    Preparing,  // Checkpointing in progress
    Closed,     // File replacement allowed
}

struct WorkerState {
    db_state: DatabaseState,
    db_path: PathBuf,
    db_name: String,
    last_activity: Instant,
}

pub async fn worker_loop(
    mut rx: mpsc::Receiver<WorkerCommand>,
    db_path: PathBuf,
    db_name: String,
) {
    let mut state = WorkerState {
        db_state: DatabaseState::Closed,
        db_path: db_path.clone(),
        db_name: db_name.clone(),
        last_activity: Instant::now(),
    };

    match init_database(&db_path).await {
        Ok(pool) => {
            state.db_state = DatabaseState::Open(pool);
            info!(db = %db_name, "Worker started and database opened");
        }
        Err(e) => {
            error!(db = %db_name, error = %e, "Failed to initialize database");
            return;
        }
    }

    loop {
        let time_until_timeout = WORKER_IDLE_TIMEOUT.saturating_sub(state.last_activity.elapsed());

        tokio::select! {
            biased;

            maybe_cmd = rx.recv() => {
                match maybe_cmd {
                    Some(WorkerCommand::Request { req, reply }) => {
                        state.last_activity = Instant::now();
                        let resp = handle_request(req, &mut state).await;
                        let _ = reply.send(resp);
                    }
                    None => {
                        info!(db = %db_name, "Command channel closed, shutting down worker");
                        break;
                    }
                }
            }

            _ = tokio::time::sleep(time_until_timeout) => {
                if rx.is_empty() && state.last_activity.elapsed() >= WORKER_IDLE_TIMEOUT {
                    info!(
                        db = %db_name,
                        idle_duration_secs = state.last_activity.elapsed().as_secs(),
                        "Idle timeout reached, shutting down worker"
                    );
                    break;
                }
            }
        }
    }

    info!(db = %db_name, "Worker stopped");
}

async fn init_database(db_path: &PathBuf) -> Result<SqlitePool> {
    let db_url = format!("sqlite:{}", db_path.display());
    
    let options = SqliteConnectOptions::from_str(&db_url)?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .busy_timeout(std::time::Duration::from_secs(5));

    let pool = SqlitePool::connect_with(options)
        .await
        .context("Failed to connect to database")?;

    sqlx::query("PRAGMA wal_autocheckpoint=1000")
        .execute(&pool)
        .await?;

    run_migrations(&pool).await?;

    Ok(pool)
}

async fn run_migrations(pool: &SqlitePool) -> Result<()> {
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

    Ok(())
}

async fn handle_request(req: Request, state: &mut WorkerState) -> Response {
    match req {
        Request::Ping { db: _ } => handle_ping(state).await,
        Request::ExecBatch { db: _, stmts, tx } => handle_exec_batch(stmts, tx, state).await,
        Request::PrepareForMaintenance { db: _ } => handle_prepare_maintenance(state).await,
        Request::CloseDatabase { db: _ } => handle_close_database(state).await,
        Request::ReopenDatabase { db: _ } => handle_reopen_database(state).await,
        Request::Shutdown => {
            info!("Shutdown requested");
            Response::ok_shutdown()
        }
    }
}

async fn handle_ping(state: &WorkerState) -> Response {
    match &state.db_state {
        DatabaseState::Open(pool) => {
            match get_current_rev(pool).await {
                Ok(rev) => Response::ok_ping(
                    env!("CARGO_PKG_VERSION").to_string(),
                    state.db_path.display().to_string(),
                    rev,
                ),
                Err(e) => {
                    error!(error = %e, "Failed to get current revision");
                    Response::error(format!("Failed to get revision: {}", e))
                }
            }
        }
        DatabaseState::Preparing => {
            Response::error_with_code(
                "Database is preparing for maintenance",
                "DATABASE_PREPARING"
            )
        }
        DatabaseState::Closed => {
            Response::error_with_code(
                "Database is closed for maintenance",
                "DATABASE_CLOSED"
            )
        }
    }
}

async fn handle_exec_batch(
    stmts: Vec<Statement>,
    tx_mode: TransactionMode,
    state: &WorkerState,
) -> Response {
    match &state.db_state {
        DatabaseState::Open(pool) => {
            if stmts.is_empty() {
                return Response::error("Empty statement batch");
            }

            // Validate statements
            for (i, stmt) in stmts.iter().enumerate() {
                if let Err(e) = validate_statement(stmt) {
                    return Response::error(format!("Statement {}: {}", i, e));
                }
            }

            match tx_mode {
                TransactionMode::Atomic => execute_atomic_batch(stmts, pool).await,
                TransactionMode::None => execute_separate_batch(stmts, pool).await,
            }
        }
        DatabaseState::Preparing => {
            Response::error_with_code(
                "Database is preparing for maintenance",
                "DATABASE_PREPARING"
            )
        }
        DatabaseState::Closed => {
            Response::error_with_code(
                "Database is closed for maintenance",
                "DATABASE_CLOSED"
            )
        }
    }
}

async fn handle_prepare_maintenance(state: &mut WorkerState) -> Response {
    match &state.db_state {
        DatabaseState::Open(pool) => {
            info!(db = %state.db_name, "Preparing database for maintenance");
            
            // Checkpoint WAL to flush all data to main DB file
            if let Err(e) = checkpoint_wal(pool).await {
                error!(db = %state.db_name, error = %e, "Failed to checkpoint WAL");
                return Response::error(format!("Failed to checkpoint WAL: {}", e));
            }
            
            info!(db = %state.db_name, "WAL checkpoint completed");
            
            // Transition to Preparing state and close pool to release read locks
            let pool = match std::mem::replace(&mut state.db_state, DatabaseState::Preparing) {
                DatabaseState::Open(p) => p,
                _ => unreachable!(),
            };
            pool.close().await;
            
            info!(db = %state.db_name, "Database in preparing state, read locks released");
            Response::ok_prepare_maintenance()
        }
        DatabaseState::Preparing => Response::error("Database is already preparing"),
        DatabaseState::Closed => Response::error("Database is already closed"),
    }
}

async fn handle_close_database(state: &mut WorkerState) -> Response {
    match &state.db_state {
        DatabaseState::Open(pool) => {
            info!(db = %state.db_name, "Closing database");
            
            // Final checkpoint before closing
            if let Err(e) = checkpoint_wal(pool).await {
                warn!(db = %state.db_name, error = %e, "Failed final checkpoint before close");
            }
            
            pool.close().await;
            state.db_state = DatabaseState::Closed;
            
            info!(db = %state.db_name, "Database closed, file locks released");
            Response::ok_close_database()
        }
        DatabaseState::Preparing => {
            // Allow closing from Preparing state (pool already closed)
            info!(db = %state.db_name, "Closing database from preparing state");
            state.db_state = DatabaseState::Closed;
            Response::ok_close_database()
        }
        DatabaseState::Closed => Response::error("Database is already closed"),
    }
}

async fn handle_reopen_database(state: &mut WorkerState) -> Response {
    if matches!(state.db_state, DatabaseState::Open(_)) {
        return Response::error("Database is already open");
    }
    
    info!(db = %state.db_name, "Reopening database");
    
    let pool = match init_database(&state.db_path).await {
        Ok(pool) => pool,
        Err(e) => {
            error!(db = %state.db_name, error = %e, "Failed to reopen database");
            return Response::error(format!("Failed to open database: {}", e));
        }
    };
    
    let rev = match get_current_rev(&pool).await {
        Ok(rev) => rev,
        Err(e) => {
            error!(db = %state.db_name, error = %e, "Failed to get revision after reopen");
            state.db_state = DatabaseState::Open(pool);
            return Response::error(format!("Database opened but failed to get revision: {}", e));
        }
    };
    
    state.db_state = DatabaseState::Open(pool);
    info!(db = %state.db_name, rev = rev, "Database reopened successfully");
    Response::ok_reopen_database(rev)
}

async fn execute_atomic_batch(stmts: Vec<Statement>, pool: &SqlitePool) -> Response {
    let start = Instant::now();

    // Begin transaction
    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            error!(error = %e, "Failed to begin transaction");
            return Response::error_with_code(e.to_string(), "TX_BEGIN_FAILED");
        }
    };

    // Execute all statements
    let total_rows = match execute_statements_in_tx(&stmts, &mut tx).await {
        Ok(rows) => rows,
        Err((i, e)) => {
            error!(error = %e, statement_index = i, sql = %stmts[i].sql, "Statement execution failed");
            return Response::error_with_code(format!("Statement {}: {}", i, e), "SQL_ERROR");
        }
    };

    // Bump revision
    let rev = match bump_revision_in_tx(&mut tx).await {
        Ok(rev) => rev,
        Err(e) => {
            error!(error = %e, "Failed to update revision");
            return Response::error("Failed to update revision");
        }
    };

    // Commit transaction
    if let Err(e) = tx.commit().await {
        error!(error = %e, "Failed to commit transaction");
        return Response::error_with_code(e.to_string(), "TX_COMMIT_FAILED");
    }

    debug!(
        batch_size = stmts.len(),
        rows_affected = total_rows,
        duration_ms = start.elapsed().as_millis(),
        rev = rev,
        "Executed atomic batch"
    );

    Response::ok_exec(rev, total_rows)
}

async fn execute_separate_batch(stmts: Vec<Statement>, pool: &SqlitePool) -> Response {
    warn!("Executing batch in separate transactions (dangerous!)");

    // Execute all statements
    let total_rows = match execute_statements_in_pool(&stmts, pool).await {
        Ok(rows) => rows,
        Err((i, e)) => {
            error!(error = %e, statement_index = i, sql = %stmts[i].sql, "Statement execution failed");
            return Response::error_with_code(format!("Statement {}: {}", i, e), "SQL_ERROR");
        }
    };

    // Bump revision
    let rev = match bump_revision(pool).await {
        Ok(rev) => rev,
        Err(e) => {
            error!(error = %e, "Failed to read revision");
            return Response::error("Failed to read revision");
        }
    };

    Response::ok_exec(rev, total_rows)
}

fn bind_param<'q>(
    query: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    value: &'q serde_json::Value,
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
    use serde_json::Value;

    match value {
        Value::Null => query.bind(None::<String>),
        Value::Bool(b) => query.bind(*b as i64),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                query.bind(i)
            } else if let Some(u) = n.as_u64() {
                query.bind(u as i64)
            } else if let Some(f) = n.as_f64() {
                query.bind(f)
            } else {
                query.bind(None::<i64>)
            }
        }
        Value::String(s) => query.bind(s.as_str()),
        Value::Array(_) | Value::Object(_) => {
            query.bind(value.to_string())
        }
    }
}

fn validate_statement(stmt: &Statement) -> Result<()> {
    if stmt.sql.len() > 100_000 {
        bail!("SQL statement too long (max 100KB)");
    }

    if stmt.params.len() > 999 {
        bail!("Too many parameters (SQLite limit is 999)");
    }

    let sql_upper = stmt.sql.trim().to_uppercase();
    if sql_upper.contains("PRAGMA WRITABLE_SCHEMA") {
        bail!("Dangerous pragma rejected");
    }

    Ok(())
}

async fn get_current_rev(pool: &SqlitePool) -> Result<i64> {
    let rev: i64 = sqlx::query_scalar("SELECT rev FROM meta")
        .fetch_one(pool)
        .await?;
    Ok(rev)
}

async fn bump_revision(pool: &SqlitePool) -> Result<i64> {
    let ts = time::OffsetDateTime::now_utc().unix_timestamp();
    sqlx::query("UPDATE meta SET rev = rev + 1, ts = ?")
        .bind(ts)
        .execute(pool)
        .await?;
    get_current_rev(pool).await
}

async fn bump_revision_in_tx(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>) -> Result<i64> {
    let ts = time::OffsetDateTime::now_utc().unix_timestamp();
    sqlx::query("UPDATE meta SET rev = rev + 1, ts = ?")
        .bind(ts)
        .execute(&mut **tx)
        .await?;
    let rev: i64 = sqlx::query_scalar("SELECT rev FROM meta")
        .fetch_one(&mut **tx)
        .await?;
    Ok(rev)
}

async fn checkpoint_wal(pool: &SqlitePool) -> Result<()> {
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(pool)
        .await?;
    Ok(())
}

async fn execute_statements_in_tx(
    stmts: &[Statement],
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> Result<u64, (usize, sqlx::Error)> {
    let mut total_rows = 0u64;
    
    for (i, stmt) in stmts.iter().enumerate() {
        let mut query = sqlx::query(&stmt.sql);
        for param in &stmt.params {
            query = bind_param(query, param);
        }
        
        match query.execute(&mut **tx).await {
            Ok(result) => total_rows += result.rows_affected(),
            Err(e) => return Err((i, e)),
        }
    }
    
    Ok(total_rows)
}

async fn execute_statements_in_pool(
    stmts: &[Statement],
    pool: &SqlitePool,
) -> Result<u64, (usize, sqlx::Error)> {
    let mut total_rows = 0u64;
    
    for (i, stmt) in stmts.iter().enumerate() {
        let mut query = sqlx::query(&stmt.sql);
        for param in &stmt.params {
            query = bind_param(query, param);
        }
        
        match query.execute(pool).await {
            Ok(result) => total_rows += result.rows_affected(),
            Err(e) => return Err((i, e)),
        }
    }
    
    Ok(total_rows)
}
