//! Integration Tests for SQLite Daemon
//!
//! ## Running Tests
//!
//! These tests require the daemon to be running before execution:
//!
//! ```powershell
//! # Terminal 1: Start the daemon
//! .\target\release\skylinedb-daemon.exe
//!
//! # Terminal 2: Run tests
//! cargo test --manifest-path daemon/Cargo.toml --test integration_tests -- --test-threads=1
//! ```
//!
//! Tests are run with `--test-threads=1` to avoid conflicts between concurrent tests.
//!
//! ## Test Coverage
//!
//! - Multi-database operations
//! - Maintenance cycle (prepare/close/reopen)
//! - Database isolation during maintenance
//! - File replacement simulation
//! - Concurrent operations across multiple databases
//! - Error handling and recovery

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::ClientOptions;

const PIPE_NAME: &str = r"\\.\pipe\SkylineDBd-v1";
const TEST_DB_DIR: &str = "test_dbs";

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
enum Request {
    Ping {
        db: String,
    },
    ExecBatch {
        db: String,
        stmts: Vec<Statement>,
        #[serde(default = "default_tx_mode")]
        tx: String,
    },
    PrepareForMaintenance {
        db: String,
    },
    CloseDatabase {
        db: String,
    },
    ReopenDatabase {
        db: String,
    },
}

fn default_tx_mode() -> String {
    "atomic".to_string()
}

#[derive(Debug, Serialize, Deserialize)]
struct Statement {
    sql: String,
    #[serde(default)]
    params: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "status")]
enum Response {
    #[serde(rename = "ok")]
    Ok {
        #[serde(flatten)]
        data: ResponseData,
    },
    #[serde(rename = "error")]
    Error { message: String },
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ResponseData {
    #[allow(dead_code)]
    Ping {
        version: String,
        db_path: String,
        rev: i64,
    },
    #[allow(dead_code)]
    ExecBatch {
        rev: i64,
        rows_affected: u64,
    },
    PrepareForMaintenance {
        checkpointed: bool,
    },
    CloseDatabase {
        closed: bool,
    },
    ReopenDatabase {
        reopened: bool,
        rev: i64,
    },
}

struct TestClient;

impl TestClient {
    async fn send_request(request: Request) -> Result<Response> {
        let mut stream = ClientOptions::new().open(PIPE_NAME)?;

        let json = serde_json::to_vec(&request)?;
        let length = json.len() as u32;

        stream.write_all(&length.to_le_bytes()).await?;
        stream.write_all(&json).await?;
        stream.flush().await?;

        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let response_len = u32::from_le_bytes(len_buf) as usize;

        let mut response_buf = vec![0u8; response_len];
        stream.read_exact(&mut response_buf).await?;

        let response: Response = serde_json::from_slice(&response_buf)?;
        Ok(response)
    }

    async fn exec(db: &str, sql: &str) -> Result<Response> {
        Self::send_request(Request::ExecBatch {
            db: db.to_string(),
            stmts: vec![Statement {
                sql: sql.to_string(),
                params: vec![],
            }],
            tx: "atomic".to_string(),
        })
        .await
    }

    async fn ping(db: &str) -> Result<Response> {
        Self::send_request(Request::Ping {
            db: db.to_string(),
        })
        .await
    }

    async fn prepare_maintenance(db: &str) -> Result<Response> {
        Self::send_request(Request::PrepareForMaintenance {
            db: db.to_string(),
        })
        .await
    }

    async fn close_database(db: &str) -> Result<Response> {
        Self::send_request(Request::CloseDatabase {
            db: db.to_string(),
        })
        .await
    }

    async fn reopen_database(db: &str) -> Result<Response> {
        Self::send_request(Request::ReopenDatabase {
            db: db.to_string(),
        })
        .await
    }
}

fn setup_test_env() {
    let _ = fs::create_dir(TEST_DB_DIR);
}

fn cleanup_test_db(db_name: &str) {
    let path = PathBuf::from(TEST_DB_DIR).join(db_name);
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(format!("{}-wal", path.display()));
    let _ = fs::remove_file(format!("{}-shm", path.display()));
}

#[tokio::test]
async fn test_multi_database_operations() -> Result<()> {
    setup_test_env();
    
    let db1 = format!("{}/test_multi1.db", TEST_DB_DIR);
    let db2 = format!("{}/test_multi2.db", TEST_DB_DIR);
    
    cleanup_test_db(&db1);
    cleanup_test_db(&db2);

    // Create tables in both databases
    TestClient::exec(&db1, "CREATE TABLE test (id INTEGER, name TEXT)").await?;
    TestClient::exec(&db2, "CREATE TABLE users (id INTEGER, email TEXT)").await?;

    // Insert data into both
    TestClient::exec(&db1, "INSERT INTO test VALUES (1, 'Alice')").await?;
    TestClient::exec(&db2, "INSERT INTO users VALUES (1, 'alice@example.com')").await?;

    // Verify both databases work
    let resp1 = TestClient::ping(&db1).await?;
    let resp2 = TestClient::ping(&db2).await?;

    match (resp1, resp2) {
        (Response::Ok { .. }, Response::Ok { .. }) => {
            println!("✓ Multi-database operations work");
            Ok(())
        }
        _ => panic!("Expected OK responses"),
    }
}

#[tokio::test]
async fn test_maintenance_cycle() -> Result<()> {
    setup_test_env();
    
    let db = format!("{}/test_maintenance.db", TEST_DB_DIR);
    cleanup_test_db(&db);

    // Create test data
    TestClient::exec(&db, "CREATE TABLE test (id INTEGER)").await?;
    TestClient::exec(&db, "INSERT INTO test VALUES (1)").await?;

    // Step 1: Prepare for maintenance
    let prep_resp = TestClient::prepare_maintenance(&db).await?;
    match prep_resp {
        Response::Ok {
            data: ResponseData::PrepareForMaintenance { checkpointed },
        } => {
            assert!(checkpointed);
            println!("✓ PrepareForMaintenance successful");
        }
        _ => panic!("Expected PrepareForMaintenance OK response"),
    }

    // Step 2: Close database
    let close_resp = TestClient::close_database(&db).await?;
    match close_resp {
        Response::Ok {
            data: ResponseData::CloseDatabase { closed },
        } => {
            assert!(closed);
            println!("✓ CloseDatabase successful");
        }
        _ => panic!("Expected CloseDatabase OK response"),
    }

    // Step 3: Verify operations are blocked
    let blocked_resp = TestClient::exec(&db, "INSERT INTO test VALUES (2)").await?;
    match blocked_resp {
        Response::Error { message } => {
            assert!(message.contains("closed"));
            println!("✓ Operations correctly blocked while closed");
        }
        _ => panic!("Expected error response for closed database"),
    }

    // Step 4: Reopen database
    let reopen_resp = TestClient::reopen_database(&db).await?;
    match reopen_resp {
        Response::Ok {
            data: ResponseData::ReopenDatabase { reopened, rev },
        } => {
            assert!(reopened);
            assert!(rev >= 0);
            println!("✓ ReopenDatabase successful, rev: {}", rev);
        }
        _ => panic!("Expected ReopenDatabase OK response"),
    }

    // Step 5: Verify operations work again
    let final_resp = TestClient::exec(&db, "INSERT INTO test VALUES (2)").await?;
    match final_resp {
        Response::Ok {
            data: ResponseData::ExecBatch { .. },
        } => {
            println!("✓ Operations work after reopen");
            Ok(())
        }
        _ => panic!("Expected ExecBatch OK response"),
    }
}

#[tokio::test]
async fn test_multi_db_isolation_during_maintenance() -> Result<()> {
    setup_test_env();
    
    let db1 = format!("{}/test_isolation1.db", TEST_DB_DIR);
    let db2 = format!("{}/test_isolation2.db", TEST_DB_DIR);
    
    cleanup_test_db(&db1);
    cleanup_test_db(&db2);

    // Setup both databases
    TestClient::exec(&db1, "CREATE TABLE test (id INTEGER)").await?;
    TestClient::exec(&db2, "CREATE TABLE test (id INTEGER)").await?;

    // Close db1
    TestClient::prepare_maintenance(&db1).await?;
    TestClient::close_database(&db1).await?;

    // Verify db1 is blocked
    let blocked_resp = TestClient::exec(&db1, "INSERT INTO test VALUES (1)").await?;
    match blocked_resp {
        Response::Error { .. } => {
            println!("✓ db1 correctly blocked");
        }
        _ => panic!("Expected db1 to be blocked"),
    }

    // Verify db2 still works
    let working_resp = TestClient::exec(&db2, "INSERT INTO test VALUES (1)").await?;
    match working_resp {
        Response::Ok { .. } => {
            println!("✓ db2 continues working while db1 is closed");
        }
        _ => panic!("Expected db2 to work"),
    }

    // Reopen db1
    TestClient::reopen_database(&db1).await?;

    // Verify both databases work now
    let resp1 = TestClient::exec(&db1, "INSERT INTO test VALUES (2)").await?;
    let resp2 = TestClient::exec(&db2, "INSERT INTO test VALUES (2)").await?;

    match (resp1, resp2) {
        (Response::Ok { .. }, Response::Ok { .. }) => {
            println!("✓ Both databases working after maintenance");
            Ok(())
        }
        _ => panic!("Expected both databases to work"),
    }
}

#[tokio::test]
async fn test_file_replacement_simulation() -> Result<()> {
    setup_test_env();
    
    let db_path = PathBuf::from(TEST_DB_DIR).join("test_replacement.db");
    let db = db_path.to_str().unwrap();
    let backup_path = PathBuf::from(TEST_DB_DIR).join("test_replacement_backup.db");
    
    cleanup_test_db(db);
    cleanup_test_db(backup_path.to_str().unwrap());

    // Create original database
    TestClient::exec(db, "CREATE TABLE test (id INTEGER)").await?;
    TestClient::exec(db, "INSERT INTO test VALUES (1)").await?;

    // Prepare for maintenance
    TestClient::prepare_maintenance(db).await?;
    
    // Backup the current file
    fs::copy(&db_path, &backup_path)?;

    // Close database
    TestClient::close_database(db).await?;

    // Wait a bit to ensure file locks are released
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Replace file (simulate downloading new version)
    // For this test, we'll just copy the backup back
    fs::copy(&backup_path, &db_path)?;

    println!("✓ File replacement simulated");

    // Reopen database
    let reopen_resp = TestClient::reopen_database(db).await?;
    match reopen_resp {
        Response::Ok {
            data: ResponseData::ReopenDatabase { reopened, .. },
        } => {
            assert!(reopened);
            println!("✓ Database reopened after file replacement");
        }
        _ => panic!("Expected ReopenDatabase OK response"),
    }

    // Verify database is accessible
    let verify_resp = TestClient::exec(db, "SELECT * FROM test").await?;
    match verify_resp {
        Response::Ok { .. } => {
            println!("✓ Database accessible after replacement");
            Ok(())
        }
        _ => panic!("Expected OK response after replacement"),
    }
}

#[tokio::test]
async fn test_concurrent_operations() -> Result<()> {
    setup_test_env();
    
    let db1 = format!("{}/test_concurrent1.db", TEST_DB_DIR);
    let db2 = format!("{}/test_concurrent2.db", TEST_DB_DIR);
    let db3 = format!("{}/test_concurrent3.db", TEST_DB_DIR);
    
    cleanup_test_db(&db1);
    cleanup_test_db(&db2);
    cleanup_test_db(&db3);

    // Setup databases
    TestClient::exec(&db1, "CREATE TABLE test (id INTEGER)").await?;
    TestClient::exec(&db2, "CREATE TABLE test (id INTEGER)").await?;
    TestClient::exec(&db3, "CREATE TABLE test (id INTEGER)").await?;

    // Run concurrent operations
    let handle1 = tokio::spawn(async move {
        for i in 0..10 {
            let _ = TestClient::exec(&db1, &format!("INSERT INTO test VALUES ({})", i)).await;
        }
    });

    let handle2 = tokio::spawn(async move {
        for i in 0..10 {
            let _ = TestClient::exec(&db2, &format!("INSERT INTO test VALUES ({})", i)).await;
        }
    });

    let handle3 = tokio::spawn(async move {
        for i in 0..10 {
            let _ = TestClient::exec(&db3, &format!("INSERT INTO test VALUES ({})", i)).await;
        }
    });

    // Wait for all to complete
    let _ = tokio::try_join!(handle1, handle2, handle3);

    println!("✓ Concurrent operations on multiple databases completed");
    Ok(())
}

#[tokio::test]
async fn test_error_handling() -> Result<()> {
    setup_test_env();
    
    let db = format!("{}/test_errors.db", TEST_DB_DIR);
    cleanup_test_db(&db);

    // Test 1: Close database that doesn't exist yet
    let close_resp = TestClient::close_database(&db).await?;
    match close_resp {
        Response::Error { .. } => {
            println!("✓ Closing non-existent database returns error");
        }
        _ => panic!("Expected error for closing non-existent database"),
    }

    // Test 2: Create database
    TestClient::exec(&db, "CREATE TABLE test (id INTEGER)").await?;

    // Test 3: Double close
    TestClient::prepare_maintenance(&db).await?;
    TestClient::close_database(&db).await?;
    
    let double_close = TestClient::close_database(&db).await?;
    match double_close {
        Response::Error { message } => {
            assert!(message.contains("already closed"));
            println!("✓ Double close returns appropriate error");
        }
        _ => panic!("Expected error for double close"),
    }

    // Test 4: Reopen and verify it works
    TestClient::reopen_database(&db).await?;
    let verify = TestClient::exec(&db, "INSERT INTO test VALUES (1)").await?;
    match verify {
        Response::Ok { .. } => {
            println!("✓ Database works after error recovery");
            Ok(())
        }
        _ => panic!("Expected OK after recovery"),
    }
}
