# SQLite Daemon Architecture

**Last Updated:** November 7, 2025

## Executive Summary

A **local database daemon** that provides safe, concurrent SQLite access across multiple processes:
- **Daemon** owns the SQLite file and serializes all writes
- **Clients** perform direct read-only queries for low latency
- **IPC** via Windows Named Pipes (cross-platform capable)
- **Automatic lifecycle** management with idle shutdown and race-free startup

---

## Design Principles

1. **Single Writer, Multiple Readers**: Only the daemon opens SQLite in write mode
2. **Zero-Config**: Auto-start daemon on first client connection
3. **Low Latency Reads**: Direct read-only SQLite connections (no IPC overhead)
4. **Safe Concurrency**: WAL mode + actor-based write serialization
5. **Crash Resilient**: SQLite atomicity guarantees, client auto-reconnect

---

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────┐
│                        Workspace                            │
│                                                             │
│  ┌──────────────┐    ┌──────────────┐    ┌──────────────┐ │
│  │   Tool A     │    │   Tool B     │    │   Tool N     │ │
│  └──────┬───────┘    └──────┬───────┘    └──────┬───────┘ │
│         │                   │                    │         │
│    WRITES (IPC)        WRITES (IPC)         WRITES (IPC)   │
│         │                   │                    │         │
│         └───────────────────┴────────────────────┘         │
│                             │                              │
│                             ▼                              │
│              ┌──────────────────────────────┐              │
│              │   SQLite Daemon (Writer)     │              │
│              │  \\.\pipe\SkylineDBd-v1      │              │
│              │                              │              │
│              │  • Serializes writes         │              │
│              │  • Runs migrations           │              │
│              │  • Manages revision counter  │              │
│              │  • Idle shutdown (15 min)    │              │
│              └────────────┬─────────────────┘              │
│                           │                                │
│                    WRITE (WAL)                             │
│                           │                                │
│                           ▼                                │
│              ┌────────────────────────────┐                │
│              │   data.db (SQLite + WAL)   │                │
│              │                            │                │
│              │  • journal_mode=WAL        │                │
│              │  • synchronous=NORMAL      │                │
│              └────────────┬───────────────┘                │
│                           │                                │
│                    READ (Direct)                           │
│                           │                                │
│         ┌─────────────────┼─────────────────┐             │
│         │                 │                 │             │
│    ┌────▼────┐       ┌────▼────┐       ┌────▼────┐       │
│    │ Tool A  │       │ Tool B  │       │ Tool N  │       │
│    │ (RO)    │       │ (RO)    │       │ (RO)    │       │
│    └─────────┘       └─────────┘       └─────────┘       │
│                                                            │
└────────────────────────────────────────────────────────────┘
```

---

## Component Breakdown

### 1. Transport Layer (IPC)

**Named Pipe Configuration:**
- **Windows**: `\\.\pipe\SkylineDBd-v1`
- **Unix**: Use Unix Domain Socket (UDS) via same API
- **Library**: `interprocess` crate (cross-platform abstraction)

**Protocol: Length-Prefixed JSON**
```
┌────────────┬──────────────────────────────┐
│  4 bytes   │         N bytes              │
│  (length)  │      (JSON payload)          │
└────────────┴──────────────────────────────┘
```

Simple, inspectable, versionable. Binary protocol (e.g., MessagePack) possible later if needed.

---

### 2. Protocol Specification

#### Request Types

**Ping** - Health check
```json
{
  "type": "Ping"
}
```
Response:
```json
{
  "ok": true,
  "version": "1.0.0",
  "db_path": "/path/to/data.db",
  "rev": 42
}
```

**ExecBatch** - Execute write statements
```json
{
  "type": "ExecBatch",
  "stmts": [
    {
      "sql": "INSERT INTO tasks (title, status) VALUES (?, ?)",
      "params": ["Buy milk", "pending"]
    },
    {
      "sql": "UPDATE meta SET last_sync = ?",
      "params": [1699382400]
    }
  ],
  "tx": "atomic"  // or "none" for separate transactions
}
```
Response:
```json
{
  "ok": true,
  "rev": 43,
  "rows_affected": 2
}
```

Error response:
```json
{
  "ok": false,
  "error": "SQLITE_CONSTRAINT: UNIQUE constraint failed: tasks.id",
  "code": "SQLITE_CONSTRAINT"
}
```

**Subscribe** (Optional) - Watch for changes
```json
{
  "type": "Subscribe"
}
```
Server-sent stream:
```json
{"rev": 43, "hints": ["tasks", "meta"]}
{"rev": 44, "hints": ["projects"]}
```

---

### 3. SQLite Configuration

#### Daemon (Writer) Connection

```sql
PRAGMA journal_mode=WAL;           -- Write-Ahead Logging
PRAGMA synchronous=NORMAL;         -- FULL for max durability, NORMAL for speed
PRAGMA busy_timeout=5000;          -- 5 second wait on lock
PRAGMA wal_autocheckpoint=1000;    -- Checkpoint every 1000 pages
```

**Connection URL:**
```
sqlite://data.db?mode=rwc
```

**Pool Size:** 1-4 connections (SQLite has limited concurrency benefit)

#### Client (Reader) Connections

```sql
PRAGMA query_only=ON;  -- Hard fail on write attempts
```

**Connection URL:**
```
sqlite://data.db?mode=ro
```

**Pool Size:** 8-16 connections (read-heavy workload)

---

### 4. Revision Tracking

**Schema:**
```sql
CREATE TABLE IF NOT EXISTS meta (
  rev INTEGER NOT NULL PRIMARY KEY,
  ts  INTEGER NOT NULL  -- Unix timestamp
);

-- Initialize
INSERT INTO meta(rev, ts)
SELECT 0, CAST(strftime('%s','now') AS INTEGER)
WHERE NOT EXISTS(SELECT 1 FROM meta);
```

**Update Strategy:**
- Bump `rev` **once per committed write batch** (in same transaction)
- Clients can:
  - Poll `SELECT rev FROM meta` periodically
  - Subscribe to daemon's change stream
  - Cache last-seen rev and refresh UI when it advances

**Querying Current Revision (read-only clients):**
```rust
let current_rev: i64 = sqlx::query_scalar("SELECT rev FROM meta")
    .fetch_one(&pool)
    .await?;
```

---

### 5. Daemon Lifecycle Management

#### Startup Sequence

```
┌────────────┐
│   Client   │
└─────┬──────┘
      │
      ├─ 1. Try connect to \\.\pipe\SkylineDBd-v1
      │
      ├─ 2. Connection failed?
      │    ├─ Acquire named mutex: Global\SkylineDBd-v1
      │    ├─ If acquired:
      │    │   ├─ Spawn daemon (detached process)
      │    │   └─ Retry connect with backoff
      │    └─ If not acquired:
      │        └─ Another process starting daemon, backoff-retry
      │
      └─ 3. Connected! Send request
```

**Named Mutex (Windows):**
```rust
use windows::Win32::System::Threading::CreateMutexW;
use std::sync::Arc;

fn try_acquire_daemon_mutex() -> Option<Arc<Mutex>> {
    // Returns Some(mutex) if this process should start daemon
    // Returns None if another process owns it
}
```

**Cross-platform alternative:** `single-instance` crate

#### Idle Shutdown

```rust
let IDLE_TIMEOUT = Duration::from_secs(15 * 60);  // 15 minutes

loop {
    tokio::select! {
        Some(cmd) = rx.recv() => {
            handle_command(cmd).await;
            last_activity = Instant::now();
        }
        _ = tokio::time::sleep_until(last_activity + IDLE_TIMEOUT) => {
            if rx.is_empty() && in_flight_requests == 0 {
                info!("Idle timeout reached, shutting down daemon");
                break;
            }
        }
    }
}
```

**Keepalive Mechanism:**
- Every request extends `last_activity` timestamp
- Daemon only exits when: `now - last_activity > IDLE_TIMEOUT` AND no in-flight work

---

### 6. Daemon Internal Architecture

#### Actor-Based Write Serialization

```rust
enum DaemonCommand {
    Ping {
        reply: oneshot::Sender<PingResponse>
    },
    ExecBatch {
        stmts: Vec<Statement>,
        tx_mode: TransactionMode,
        reply: oneshot::Sender<Result<BatchResult>>
    },
    Shutdown {
        reply: oneshot::Sender<()>
    }
}

struct Statement {
    sql: String,
    params: Vec<serde_json::Value>
}

struct BatchResult {
    rev: i64,
    rows_affected: u64
}

enum TransactionMode {
    Atomic,   // All statements in one transaction
    None      // Each statement separate (risky!)
}
```

**Actor Loop Pattern:**
```rust
async fn writer_actor(
    mut rx: mpsc::Receiver<DaemonCommand>,
    pool: Pool<Sqlite>
) {
    let mut last_activity = Instant::now();
    
    loop {
        tokio::select! {
            biased;  // Prioritize commands over timeout
            
            maybe_cmd = rx.recv() => {
                match maybe_cmd {
                    Some(cmd) => {
                        handle_command(cmd, &pool).await;
                        last_activity = Instant::now();
                    }
                    None => break  // Channel closed
                }
            }
            
            _ = sleep_until(last_activity + IDLE_TIMEOUT) => {
                if rx.is_empty() {
                    info!("Shutting down due to inactivity");
                    break;
                }
            }
        }
    }
}
```

**Why Actor Pattern?**
- Serializes all writes (no race conditions)
- Single SQLite connection per actor (optimal)
- Bounded queue prevents memory bloat
- Clean shutdown handling

---

### 7. Error Handling Strategy

#### Client-Side Retry Policy

```rust
const MAX_RETRIES: u32 = 5;
const INITIAL_BACKOFF: Duration = Duration::from_millis(50);
const MAX_BACKOFF: Duration = Duration::from_secs(5);

async fn exec_batch_with_retry(
    client: &DaemonClient,
    stmts: Vec<Statement>
) -> Result<BatchResult> {
    let mut backoff = INITIAL_BACKOFF;
    
    for attempt in 0..MAX_RETRIES {
        match client.exec_batch(stmts.clone()).await {
            Ok(result) => return Ok(result),
            
            Err(e) if e.is_connection_error() => {
                // Daemon might be starting or crashed
                if attempt == 0 {
                    ensure_daemon_running().await?;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            
            Err(e) => return Err(e)  // SQL error, don't retry
        }
    }
    
    Err(anyhow!("Failed after {} retries", MAX_RETRIES))
}
```

#### Daemon-Side Error Handling

**SQL Errors:**
- Return to client immediately (e.g., constraint violations)
- Log for diagnostics
- Don't crash daemon

**Connection Errors:**
- Log client disconnect
- Clean up resources
- Continue serving other clients

**Panic Safety:**
```rust
let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
    // Dangerous operation
}));

match result {
    Ok(val) => val,
    Err(_) => {
        error!("Panic caught in daemon, continuing...");
        // Return error to client, don't crash daemon
    }
}
```

---

### 8. Migration System

#### Migration Files

```
migrations/
  ├── 001_initial_schema.sql
  ├── 002_add_projects_table.sql
  └── 003_add_indexes.sql
```

#### Migration Runner (Daemon Boot)

```rust
async fn run_migrations(pool: &Pool<Sqlite>) -> Result<()> {
    // Create migrations table
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS _migrations (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL UNIQUE,
            applied_at INTEGER NOT NULL
        )"
    ).execute(pool).await?;
    
    // Read migration files
    let migrations = read_migration_files("migrations/")?;
    
    for migration in migrations {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM _migrations WHERE name = ?)"
        )
        .bind(&migration.name)
        .fetch_one(pool)
        .await?;
        
        if !exists {
            info!("Applying migration: {}", migration.name);
            
            let mut tx = pool.begin().await?;
            sqlx::raw_sql(&migration.sql).execute(&mut *tx).await?;
            
            sqlx::query(
                "INSERT INTO _migrations (name, applied_at) VALUES (?, ?)"
            )
            .bind(&migration.name)
            .bind(time::OffsetDateTime::now_utc().unix_timestamp())
            .execute(&mut *tx)
            .await?;
            
            tx.commit().await?;
        }
    }
    
    Ok(())
}
```

**Migration Guarantees:**
- Run once at daemon startup (before accepting connections)
- Atomic per-migration (transaction wrapped)
- Idempotent (track applied migrations)
- Single daemon = no concurrent migration race

---

### 9. Client Library API

#### Rust Client Example

```rust
use skylinedb_client::{DaemonClient, Statement};

#[tokio::main]
async fn main() -> Result<()> {
    // Auto-connects and starts daemon if needed
    let client = DaemonClient::connect("data.db").await?;
    
    // Write operation (via daemon)
    let result = client.exec_batch(vec![
        Statement::new(
            "INSERT INTO tasks (title, status) VALUES (?, ?)",
            vec!["Buy milk".into(), "pending".into()]
        ),
        Statement::new(
            "UPDATE meta SET last_modified = ?",
            vec![chrono::Utc::now().timestamp().into()]
        )
    ]).await?;
    
    println!("Written at rev: {}", result.rev);
    
    // Read operation (direct SQLite)
    let read_pool = client.read_pool().await?;
    let tasks: Vec<Task> = sqlx::query_as("SELECT * FROM tasks")
        .fetch_all(&read_pool)
        .await?;
    
    // Watch for changes
    let mut changes = client.subscribe().await?;
    tokio::spawn(async move {
        while let Some(rev) = changes.next().await {
            println!("DB changed, new rev: {}", rev);
            // Refresh UI
        }
    });
    
    Ok(())
}
```

#### API Surface

```rust
pub struct DaemonClient {
    pipe_client: PipeClient,
    db_path: PathBuf,
    read_pool: OnceCell<Pool<Sqlite>>
}

impl DaemonClient {
    /// Connect to daemon, starting it if needed
    pub async fn connect(db_path: impl AsRef<Path>) -> Result<Self>;
    
    /// Execute write batch via daemon
    pub async fn exec_batch(&self, stmts: Vec<Statement>) -> Result<BatchResult>;
    
    /// Ping daemon (health check)
    pub async fn ping(&self) -> Result<PingResponse>;
    
    /// Get read-only SQLite pool (direct access)
    pub async fn read_pool(&self) -> Result<&Pool<Sqlite>>;
    
    /// Subscribe to change notifications
    pub async fn subscribe(&self) -> Result<impl Stream<Item = i64>>;
}
```

---

### 10. Performance Characteristics

#### Latency Budget

| Operation | Target | Notes |
|-----------|--------|-------|
| Read query (direct) | 1-10ms | No IPC, just SQLite |
| Write batch (1 stmt) | 5-20ms | IPC + WAL write |
| Write batch (10 stmts) | 10-50ms | Amortized IPC cost |
| Daemon startup | 100-500ms | Includes migration check |

#### Throughput

**Writes:**
- Single client: ~200-500 batches/sec (bounded by SQLite fsync)
- Multiple clients: Similar (serialized through actor)
- **Optimization:** Batch multiple statements per request

**Reads:**
- Direct SQLite read-only: ~10k-100k queries/sec (in-memory hot data)
- Scales linearly with client count (no daemon bottleneck)

#### Memory Footprint

- **Daemon**: 10-50 MB (Rust binary + SQLite cache)
- **Client**: 2-10 MB (connection pool + caches)

---

### 11. Security Considerations

#### Access Control

**Named Pipe Permissions (Windows):**
```rust
// Create pipe with restrictive ACL (current user only)
let security_attributes = SecurityAttributes::allow_current_user_only();
let listener = LocalSocketListener::bind_with_security(
    PIPE_NAME,
    security_attributes
)?;
```

**File Permissions:**
- Database file: Read/write for current user only
- WAL file: Same as database
- SHM file: Same as database

#### SQL Injection Prevention

**Client-side:** Always use parameterized queries
```rust
// ✅ GOOD
Statement::new("SELECT * FROM tasks WHERE id = ?", vec![id.into()])

// ❌ BAD
Statement::new(&format!("SELECT * FROM tasks WHERE id = {}", id), vec![])
```

**Daemon-side:** Consider rejecting dynamic SQL (require allowlist of patterns)

#### Input Validation

```rust
fn validate_statement(stmt: &Statement) -> Result<()> {
    // Limit SQL length
    if stmt.sql.len() > 10_000 {
        bail!("SQL statement too long");
    }
    
    // Limit parameter count
    if stmt.params.len() > 100 {
        bail!("Too many parameters");
    }
    
    // Deny dangerous pragmas
    let sql_upper = stmt.sql.to_uppercase();
    if sql_upper.contains("PRAGMA WRITABLE_SCHEMA") {
        bail!("Dangerous pragma rejected");
    }
    
    Ok(())
}
```

---

### 12. Edge Cases & Solutions

| Edge Case | Solution |
|-----------|----------|
| **Daemon crashes mid-transaction** | SQLite atomicity ensures DB consistency. Clients retry on disconnect. |
| **Multiple processes try to start daemon** | Named mutex ensures only one succeeds. Others wait and retry connect. |
| **Long-running read blocks write** | WAL mode allows concurrent reads/writes. No blocking. |
| **Client crashes with open read connection** | SQLite handles this gracefully. File handles auto-close. |
| **Daemon OOM under load** | Bounded MPSC queue (e.g., 1000 slots). Return "busy" error if full. |
| **Disk full during write** | SQLite returns error, transaction rolls back. Client sees error and retries later. |
| **Clock skew (timestamp in meta)** | Timestamps are informational only. `rev` counter is source of truth. |
| **Schema migration fails** | Daemon refuses to start. Client gets connection error. Manual intervention required. |
| **Thundering herd at startup** | Named mutex + exponential backoff prevent stampede. |
| **Stale read-only connection** | Clients poll `meta.rev` periodically and refresh UI when changed. |

---

### 13. Monitoring & Observability

#### Metrics to Track

**Daemon:**
- Active connections count
- Write queue depth
- Statements/sec (read, write)
- Average batch size
- Error rate by type
- Last activity timestamp
- Current revision number
- WAL file size

**Client:**
- Connection state (connected, reconnecting, failed)
- Request latency (p50, p99)
- Retry count
- Cache hit rate (if caching reads)

#### Logging Strategy

```rust
use tracing::{info, warn, error, debug};

// Daemon startup
info!(
    db_path = %db_path,
    pipe_name = PIPE_NAME,
    "Daemon started"
);

// Write batch
debug!(
    batch_size = stmts.len(),
    rows_affected = result.rows_affected,
    duration_ms = elapsed.as_millis(),
    "Executed batch"
);

// Client connection
info!(
    client_id = %conn_id,
    "Client connected"
);

// Error
error!(
    error = %e,
    client_id = %conn_id,
    "Failed to execute batch"
);
```

#### Health Check Endpoint

```bash
# CLI tool to check daemon health
> skylinedb-cli status
✓ Daemon running (PID 12345)
  Uptime: 2h 34m
  DB: C:\Users\...\data.db
  Revision: 1542
  Clients: 3 active
  Last write: 12s ago
```

---

### 14. Deployment Considerations

#### File Locations (Windows)

```
%LOCALAPPDATA%\YourApp\
  ├── data.db           # Main database
  ├── data.db-wal       # Write-ahead log
  ├── data.db-shm       # Shared memory (WAL index)
  └── daemon.log        # Daemon logs
  
\\.\pipe\YourApp-DBd-v1  # Named pipe
```

#### Packaging

**Daemon Binary:**
- Single executable (~5-10 MB with Rust)
- No external dependencies (static linking)
- Version in filename for parallel installs: `skylinedb-daemon-v1.exe`

**Client Library:**
- Rust: Cargo crate
- Other languages: C FFI wrapper

**Installer:**
- No daemon auto-start required (lazy spawn)
- Just copy binaries to program directory

---

### 15. Testing Strategy

#### Unit Tests

```rust
#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_write_batch() {
        let daemon = DaemonClient::connect(":memory:").await?;
        let result = daemon.exec_batch(vec![
            Statement::new("CREATE TABLE test (id INT)", vec![]),
            Statement::new("INSERT INTO test VALUES (1)", vec![])
        ]).await?;
        
        assert_eq!(result.rows_affected, 1);
        assert_eq!(result.rev, 1);
    }
    
    #[tokio::test]
    async fn test_concurrent_reads() {
        let daemon = DaemonClient::connect("test.db").await?;
        let pool = daemon.read_pool().await?;
        
        let handles: Vec<_> = (0..10).map(|i| {
            let pool = pool.clone();
            tokio::spawn(async move {
                sqlx::query!("SELECT * FROM test WHERE id = ?", i)
                    .fetch_one(&pool)
                    .await
            })
        }).collect();
        
        for h in handles {
            h.await??;
        }
    }
}
```

#### Integration Tests

1. **Daemon Lifecycle**
   - Start daemon
   - Client connects
   - Daemon idles out after timeout
   - Client reconnects (daemon restarts)

2. **Concurrent Access**
   - Multiple clients writing simultaneously
   - Verify serialization (no lost updates)

3. **Crash Recovery**
   - Kill daemon mid-write
   - Verify DB integrity
   - Client auto-reconnects

4. **Migration**
   - Run with old schema
   - Deploy new daemon with migration
   - Verify schema updated

#### Load Testing

```bash
# Simulate 10 clients, 1000 writes each
> skylinedb-bench --clients 10 --writes 1000 --db test.db

Results:
  Duration: 12.5s
  Throughput: 800 writes/sec
  Latency p50: 8ms, p99: 25ms
  Errors: 0
```

---

### 16. Future Enhancements

#### Phase 2 (Optional)

1. **Read Replicas**
   - Daemon exposes read-only endpoints
   - Useful for network access (vs. direct file access)

2. **Compression**
   - LZ4 compress large JSON params/results
   - Reduces IPC overhead

3. **Query Plan Cache**
   - Daemon caches prepared statements
   - Reduces SQLite parsing overhead

4. **Change Streaming**
   - Fine-grained table/row-level change notifications
   - Useful for reactive UI updates

5. **Multi-Database Support**
   - One daemon manages multiple DB files
   - Indexed by workspace/project ID

6. **HTTP Admin Interface**
   - Web UI for monitoring daemon state
   - Trigger manual checkpoints, vacuum, etc.

#### Phase 3 (Advanced)

1. **Distributed Mode**
   - Multiple daemons + Raft consensus
   - For cross-machine scenarios

2. **Time-Travel Queries**
   - Preserve old revisions
   - Query historical state

3. **Encryption at Rest**
   - SQLCipher integration
   - Key management strategy

---

## Implementation Roadmap

### Milestone 1: Core Daemon (Week 1-2)
- [ ] IPC server (named pipe + length-prefixed JSON)
- [ ] Actor-based write serialization
- [ ] SQLite pool with WAL configuration
- [ ] Migration runner
- [ ] Ping and ExecBatch handlers
- [ ] Idle shutdown logic
- [ ] Basic logging

### Milestone 2: Client Library (Week 2-3)
- [ ] Connection management (with auto-start)
- [ ] Named mutex startup guard
- [ ] Retry logic with exponential backoff
- [ ] Read-only pool creation
- [ ] `exec_batch()` API
- [ ] Error types and handling

### Milestone 3: Testing & Polish (Week 3-4)
- [ ] Unit tests (protocol, SQL binding)
- [ ] Integration tests (lifecycle, concurrency)
- [ ] Load testing harness
- [ ] Documentation (API docs, examples)
- [ ] CLI tool (`skylinedb-cli status`)

### Milestone 4: Production Readiness (Week 4-5)
- [ ] Security audit (input validation, permissions)
- [ ] Error handling hardening
- [ ] Performance profiling and optimization
- [ ] Windows installer
- [ ] Deployment guide

### Milestone 5: Optional Features (Week 6+)
- [ ] Change subscription (Subscribe command)
- [ ] Query plan cache
- [ ] Metrics/monitoring endpoints
- [ ] Cross-platform testing (macOS, Linux)

---

## Dependencies (Cargo.toml)

```toml
[workspace]
members = ["daemon", "client", "cli"]

[workspace.dependencies]
tokio = { version = "1", features = ["full"] }
sqlx = { version = "0.7", features = ["sqlite", "runtime-tokio-rustls"] }
interprocess = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
thiserror = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
time = { version = "0.3", features = ["macros"] }
```

**Platform-specific:**
```toml
[target.'cfg(windows)'.dependencies]
windows = { version = "0.52", features = [
    "Win32_System_Threading",
    "Win32_Security",
] }

[target.'cfg(unix)'.dependencies]
nix = { version = "0.27", features = ["signal"] }
```

---

## File Structure

```
sqlite_daemon/
├── Cargo.toml              # Workspace definition
├── ARCHITECTURE.md         # This file
├── README.md               # User-facing documentation
│
├── daemon/                 # Daemon binary crate
│   ├── Cargo.toml
│   ├── src/
│   │   ├── main.rs         # Entry point
│   │   ├── actor.rs        # Write actor loop
│   │   ├── server.rs       # IPC server
│   │   ├── protocol.rs     # Request/response types
│   │   ├── db.rs           # SQLite setup & migrations
│   │   └── config.rs       # Configuration
│   └── migrations/         # SQL migration files
│       ├── 001_initial.sql
│       └── 002_meta.sql
│
├── client/                 # Client library crate
│   ├── Cargo.toml
│   ├── src/
│   │   ├── lib.rs          # Public API
│   │   ├── connection.rs   # IPC client
│   │   ├── spawn.rs        # Daemon lifecycle
│   │   ├── reader.rs       # Read-only pool
│   │   └── protocol.rs     # Shared with daemon
│   └── examples/
│       └── basic_usage.rs
│
├── cli/                    # CLI tool crate
│   ├── Cargo.toml
│   └── src/
│       └── main.rs         # status, check, etc.
│
└── tests/                  # Integration tests
    ├── test_lifecycle.rs
    ├── test_concurrency.rs
    └── test_migrations.rs
```

---

## Quick Reference: Key Policies

| Policy | Guideline |
|--------|-----------|
| **Write batching** | Group related statements to amortize IPC cost |
| **Transaction size** | Keep < 50ms typical (aim for 10-20 statements) |
| **Idempotency** | Use `INSERT ... ON CONFLICT DO UPDATE` for safe retries |
| **Schema changes** | Only via migrations in daemon (single source of truth) |
| **Error retry** | Retry connection errors with backoff, fail fast on SQL errors |
| **DB sharding** | One DB file per workspace/project if needed |
| **Read caching** | Client-side caching OK (invalidate on rev change) |
| **Long transactions** | Not supported over IPC (always atomic batch) |
| **Busy handling** | Bounded queue; return "busy" error if full |

---

## Conclusion

This architecture provides:
- ✅ **Safe concurrency** without complex locking
- ✅ **High performance** reads (direct SQLite, no IPC)
- ✅ **Simple deployment** (zero-config, auto-start)
- ✅ **Crash resilience** (SQLite durability + client retry)
- ✅ **Easy testing** (actor pattern + dependency injection)
- ✅ **Cross-platform** ready (with minimal code changes)

**Next Steps:**
1. Review this plan with stakeholders
2. Create project structure (`cargo new --bin daemon`, etc.)
3. Start with Milestone 1 (core daemon)
4. Iterate based on real-world usage patterns

---

*Document Version: 1.0*
*Target Implementation: Q1 2026*
