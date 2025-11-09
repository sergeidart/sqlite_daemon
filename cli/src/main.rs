use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(windows)]
const PIPE_NAME: &str = r"\\.\pipe\SkylineDBd-v1";

#[cfg(unix)]
const PIPE_NAME: &str = "/tmp/skylinedb-v1.sock";

#[derive(Parser)]
#[command(name = "skylinedb-cli")]
#[command(about = "SQLite daemon CLI", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Check daemon status
    Ping {
        /// Database name (e.g., "galaxy.db")
        #[arg(long, default_value = "data.db")]
        db: String,
    },
    
    /// Execute SQL statements
    Exec {
        /// Database name (e.g., "galaxy.db")
        #[arg(long, default_value = "data.db")]
        db: String,
        /// SQL statements (can be multiple)
        #[arg(required = true)]
        sql: Vec<String>,
    },
    
    /// Prepare database for maintenance (checkpoint WAL)
    PrepareForMaintenance {
        /// Database name (e.g., "galaxy.db")
        #[arg(long, default_value = "data.db")]
        db: String,
    },
    
    /// Close database for file replacement
    CloseDatabase {
        /// Database name (e.g., "galaxy.db")
        #[arg(long, default_value = "data.db")]
        db: String,
    },
    
    /// Reopen database after file replacement
    ReopenDatabase {
        /// Database name (e.g., "galaxy.db")
        #[arg(long, default_value = "data.db")]
        db: String,
    },
    
    /// Shutdown daemon gracefully
    Shutdown,
}

// Protocol types (minimal copy for CLI)
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
    Shutdown,
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
    Ok { #[serde(flatten)] data: ResponseData },
    
    #[serde(rename = "error")]
    Error { message: String },
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ResponseData {
    Ping {
        version: String,
        db_path: String,
        rev: i64,
    },
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Ping { db } => {
            let response = send_request(Request::Ping { db: db.clone() }).await?;
            match response {
                Response::Ok {
                    data: ResponseData::Ping { version, db_path, rev },
                } => {
                    println!("✓ Daemon is running");
                    println!("  Database: {}", db);
                    println!("  Version: {}", version);
                    println!("  Path: {}", db_path);
                    println!("  Revision: {}", rev);
                }
                Response::Error { message } => {
                    eprintln!("✗ Error: {}", message);
                    std::process::exit(1);
                }
                _ => {
                    eprintln!("✗ Unexpected response");
                    std::process::exit(1);
                }
            }
        }

        Commands::Exec { db, sql } => {
            let stmts = sql.into_iter().map(|s| Statement {
                sql: s,
                params: vec![],
            }).collect();

            let request = Request::ExecBatch {
                db: db.clone(),
                stmts,
                tx: "atomic".to_string(),
            };

            let response = send_request(request).await?;
            match response {
                Response::Ok {
                    data: ResponseData::ExecBatch { rev, rows_affected },
                } => {
                    println!("✓ Executed successfully on database: {}", db);
                    println!("  Rows affected: {}", rows_affected);
                    println!("  New revision: {}", rev);
                }
                Response::Error { message } => {
                    eprintln!("✗ Error: {}", message);
                    std::process::exit(1);
                }
                _ => {
                    eprintln!("✗ Unexpected response");
                    std::process::exit(1);
                }
            }
        }

        Commands::PrepareForMaintenance { db } => {
            let response = send_request(Request::PrepareForMaintenance { db: db.clone() }).await?;
            match response {
                Response::Ok {
                    data: ResponseData::PrepareForMaintenance { checkpointed },
                } => {
                    println!("✓ Database prepared for maintenance: {}", db);
                    println!("  WAL checkpointed: {}", checkpointed);
                }
                Response::Error { message } => {
                    eprintln!("✗ Error: {}", message);
                    std::process::exit(1);
                }
                _ => {
                    eprintln!("✗ Unexpected response");
                    std::process::exit(1);
                }
            }
        }

        Commands::CloseDatabase { db } => {
            let response = send_request(Request::CloseDatabase { db: db.clone() }).await?;
            match response {
                Response::Ok {
                    data: ResponseData::CloseDatabase { closed },
                } => {
                    println!("✓ Database closed: {}", db);
                    println!("  Closed: {}", closed);
                    println!("  File locks released - safe to replace files");
                }
                Response::Error { message } => {
                    eprintln!("✗ Error: {}", message);
                    std::process::exit(1);
                }
                _ => {
                    eprintln!("✗ Unexpected response");
                    std::process::exit(1);
                }
            }
        }

        Commands::ReopenDatabase { db } => {
            let response = send_request(Request::ReopenDatabase { db: db.clone() }).await?;
            match response {
                Response::Ok {
                    data: ResponseData::ReopenDatabase { reopened, rev },
                } => {
                    println!("✓ Database reopened: {}", db);
                    println!("  Reopened: {}", reopened);
                    println!("  Current revision: {}", rev);
                }
                Response::Error { message } => {
                    eprintln!("✗ Error: {}", message);
                    std::process::exit(1);
                }
                _ => {
                    eprintln!("✗ Unexpected response");
                    std::process::exit(1);
                }
            }
        }

        Commands::Shutdown => {
            // Shutdown response is just empty OK, ignore parsing error
            match send_request(Request::Shutdown).await {
                Ok(_) | Err(_) => {
                    println!("✓ Daemon shutdown requested");
                }
            }
        }
    }

    Ok(())
}

#[cfg(windows)]
async fn send_request(request: Request) -> Result<Response> {
    use tokio::net::windows::named_pipe::ClientOptions;
    
    // Connect to daemon
    let mut stream = ClientOptions::new()
        .open(PIPE_NAME)
        .context("Failed to connect to daemon. Is it running?")?;

    // Serialize request
    let json = serde_json::to_vec(&request)?;
    let length = json.len() as u32;

    // Send request (length-prefixed)
    stream.write_all(&length.to_le_bytes()).await?;
    stream.write_all(&json).await?;
    stream.flush().await?;

    // Read response length
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let response_len = u32::from_le_bytes(len_buf) as usize;

    // Read response body
    let mut response_buf = vec![0u8; response_len];
    stream.read_exact(&mut response_buf).await?;

    // Parse response
    let response: Response = serde_json::from_slice(&response_buf)?;

    Ok(response)
}

#[cfg(unix)]
async fn send_request(request: Request) -> Result<Response> {
    use tokio::net::UnixStream;
    
    // Connect to daemon
    let mut stream = UnixStream::connect(PIPE_NAME)
        .await
        .context("Failed to connect to daemon. Is it running?")?;

    // Serialize request
    let json = serde_json::to_vec(&request)?;
    let length = json.len() as u32;

    // Send request (length-prefixed)
    stream.write_all(&length.to_le_bytes()).await?;
    stream.write_all(&json).await?;
    stream.flush().await?;

    // Read response length
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let response_len = u32::from_le_bytes(len_buf) as usize;

    // Read response body
    let mut response_buf = vec![0u8; response_len];
    stream.read_exact(&mut response_buf).await?;

    // Parse response
    let response: Response = serde_json::from_slice(&response_buf)?;

    Ok(response)
}
