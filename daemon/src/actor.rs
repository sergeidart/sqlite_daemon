use crate::protocol::{Request, Response, Statement, TransactionMode};
use anyhow::{bail, Result};
use sqlx::SqlitePool;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60); // 15 minutes

pub enum ActorCommand {
    Request {
        req: Request,
        reply: oneshot::Sender<Response>,
    },
}

/// Write actor that serializes all database writes
pub async fn actor_loop(mut rx: mpsc::Receiver<ActorCommand>, pool: SqlitePool, db_path: String) {
    let mut last_activity = Instant::now();

    info!("Write actor started");

    loop {
        // Calculate time until idle timeout
        let time_until_timeout = IDLE_TIMEOUT.saturating_sub(last_activity.elapsed());

        tokio::select! {
            biased;

            maybe_cmd = rx.recv() => {
                match maybe_cmd {
                    Some(ActorCommand::Request { req, reply }) => {
                        // Reset timeout on ANY incoming message
                        last_activity = Instant::now();
                        
                        // Process the request
                        let resp = handle_request(req, &pool, &db_path).await;
                        let _ = reply.send(resp);
                    }
                    None => {
                        info!("Command channel closed, shutting down");
                        break;
                    }
                }
            }

            _ = tokio::time::sleep(time_until_timeout) => {
                // Timeout fired - double check if we're truly idle
                // (in case a message arrived just as timeout fired)
                if rx.is_empty() && last_activity.elapsed() >= IDLE_TIMEOUT {
                    info!(
                        idle_duration_secs = last_activity.elapsed().as_secs(),
                        "Idle timeout reached, shutting down daemon"
                    );
                    break;
                } else {
                    // False alarm - message arrived or clock skew, continue loop
                    // The next iteration will recalculate the timeout
                    debug!("Idle timeout fired but activity detected, continuing");
                }
            }
        }
    }

    info!("Write actor stopped");
}

async fn handle_request(req: Request, pool: &SqlitePool, db_path: &str) -> Response {
    match req {
        Request::Ping => handle_ping(pool, db_path).await,
        Request::ExecBatch { stmts, tx } => handle_exec_batch(stmts, tx, pool).await,
        Request::Shutdown => {
            info!("Shutdown requested");
            Response::ok_shutdown()
        }
    }
}

async fn handle_ping(pool: &SqlitePool, db_path: &str) -> Response {
    match crate::db::get_current_rev(pool).await {
        Ok(rev) => Response::ok_ping(
            env!("CARGO_PKG_VERSION").to_string(),
            db_path.to_string(),
            rev,
        ),
        Err(e) => {
            error!(error = %e, "Failed to get current revision");
            Response::error(format!("Failed to get revision: {}", e))
        }
    }
}

async fn handle_exec_batch(
    stmts: Vec<Statement>,
    tx_mode: TransactionMode,
    pool: &SqlitePool,
) -> Response {
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

    let mut total_rows = 0u64;

    // Execute all statements
    for (i, stmt) in stmts.iter().enumerate() {
        let mut query = sqlx::query(&stmt.sql);

        // Bind parameters
        for param in &stmt.params {
            query = bind_param(query, param);
        }

        match query.execute(&mut *tx).await {
            Ok(result) => {
                total_rows += result.rows_affected();
            }
            Err(e) => {
                error!(
                    error = %e,
                    statement_index = i,
                    sql = %stmt.sql,
                    "Statement execution failed"
                );
                // Transaction will auto-rollback on drop
                return Response::error_with_code(
                    format!("Statement {}: {}", i, e),
                    "SQL_ERROR",
                );
            }
        }
    }

    // Bump revision
    let ts = time::OffsetDateTime::now_utc().unix_timestamp();
    if let Err(e) = sqlx::query("UPDATE meta SET rev = rev + 1, ts = ?")
        .bind(ts)
        .execute(&mut *tx)
        .await
    {
        error!(error = %e, "Failed to update revision");
        return Response::error("Failed to update revision");
    }

    // Get new revision
    let rev: i64 = match sqlx::query_scalar("SELECT rev FROM meta")
        .fetch_one(&mut *tx)
        .await
    {
        Ok(rev) => rev,
        Err(e) => {
            error!(error = %e, "Failed to read revision");
            return Response::error("Failed to read revision");
        }
    };

    // Commit transaction
    if let Err(e) = tx.commit().await {
        error!(error = %e, "Failed to commit transaction");
        return Response::error_with_code(e.to_string(), "TX_COMMIT_FAILED");
    }

    let elapsed = start.elapsed();
    debug!(
        batch_size = stmts.len(),
        rows_affected = total_rows,
        duration_ms = elapsed.as_millis(),
        rev = rev,
        "Executed atomic batch"
    );

    Response::ok_exec(rev, total_rows)
}

async fn execute_separate_batch(stmts: Vec<Statement>, pool: &SqlitePool) -> Response {
    warn!("Executing batch in separate transactions (dangerous!)");

    let mut total_rows = 0u64;

    for (i, stmt) in stmts.iter().enumerate() {
        let mut query = sqlx::query(&stmt.sql);

        for param in &stmt.params {
            query = bind_param(query, param);
        }

        match query.execute(pool).await {
            Ok(result) => {
                total_rows += result.rows_affected();
            }
            Err(e) => {
                error!(
                    error = %e,
                    statement_index = i,
                    sql = %stmt.sql,
                    "Statement execution failed"
                );
                return Response::error_with_code(
                    format!("Statement {}: {}", i, e),
                    "SQL_ERROR",
                );
            }
        }
    }

    // Bump revision (outside transaction - risky!)
    let ts = time::OffsetDateTime::now_utc().unix_timestamp();
    if let Err(e) = sqlx::query("UPDATE meta SET rev = rev + 1, ts = ?")
        .bind(ts)
        .execute(pool)
        .await
    {
        error!(error = %e, "Failed to update revision");
        return Response::error("Failed to update revision");
    }

    let rev = match crate::db::get_current_rev(pool).await {
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
            // Store complex types as JSON strings
            query.bind(value.to_string())
        }
    }
}

fn validate_statement(stmt: &Statement) -> Result<()> {
    // Limit SQL length
    if stmt.sql.len() > 100_000 {
        bail!("SQL statement too long (max 100KB)");
    }

    // Limit parameter count
    if stmt.params.len() > 999 {
        bail!("Too many parameters (SQLite limit is 999)");
    }

    // Check for dangerous pragmas
    let sql_upper = stmt.sql.trim().to_uppercase();
    if sql_upper.contains("PRAGMA WRITABLE_SCHEMA") {
        bail!("Dangerous pragma rejected");
    }

    Ok(())
}
