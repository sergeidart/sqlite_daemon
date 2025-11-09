use serde::{Deserialize, Serialize};

/// Request from client to daemon
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Request {
    /// Health check
    Ping {
        /// Database identifier (file name)
        db: String,
    },
    
    /// Execute a batch of write statements
    ExecBatch {
        /// Database identifier (file name)
        db: String,
        /// SQL statements with parameters
        stmts: Vec<Statement>,
        /// Transaction mode: "atomic" or "none"
        #[serde(default = "default_tx_mode")]
        tx: TransactionMode,
    },
    
    /// Prepare database for maintenance (checkpoint WAL)
    PrepareForMaintenance {
        /// Database identifier (file name)
        db: String,
    },
    
    /// Close database connection (for file replacement)
    CloseDatabase {
        /// Database identifier (file name)
        db: String,
    },
    
    /// Reopen database connection (after file replacement)
    ReopenDatabase {
        /// Database identifier (file name)
        db: String,
    },
    
    /// Graceful shutdown (for testing)
    Shutdown,
}

fn default_tx_mode() -> TransactionMode {
    TransactionMode::Atomic
}

/// A single SQL statement with parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Statement {
    pub sql: String,
    #[serde(default)]
    pub params: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TransactionMode {
    /// All statements in one transaction (recommended)
    Atomic,
    /// Each statement separate (dangerous!)
    None,
}

/// Response from daemon to client
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status")]
pub enum Response {
    /// Success response
    #[serde(rename = "ok")]
    Ok {
        #[serde(flatten)]
        data: ResponseData,
    },
    
    /// Error response
    #[serde(rename = "error")]
    Error {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseData {
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
    Shutdown,
}

impl Response {
    pub fn ok_ping(version: String, db_path: String, rev: i64) -> Self {
        Response::Ok {
            data: ResponseData::Ping {
                version,
                db_path,
                rev,
            },
        }
    }

    pub fn ok_exec(rev: i64, rows_affected: u64) -> Self {
        Response::Ok {
            data: ResponseData::ExecBatch { rev, rows_affected },
        }
    }

    pub fn ok_shutdown() -> Self {
        Response::Ok {
            data: ResponseData::Shutdown,
        }
    }

    pub fn ok_prepare_maintenance() -> Self {
        Response::Ok {
            data: ResponseData::PrepareForMaintenance {
                checkpointed: true,
            },
        }
    }

    pub fn ok_close_database() -> Self {
        Response::Ok {
            data: ResponseData::CloseDatabase {
                closed: true,
            },
        }
    }

    pub fn ok_reopen_database(rev: i64) -> Self {
        Response::Ok {
            data: ResponseData::ReopenDatabase {
                reopened: true,
                rev,
            },
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Response::Error {
            message: message.into(),
            code: None,
        }
    }

    pub fn error_with_code(message: impl Into<String>, code: impl Into<String>) -> Self {
        Response::Error {
            message: message.into(),
            code: Some(code.into()),
        }
    }
}
