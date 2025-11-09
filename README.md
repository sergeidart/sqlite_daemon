# SQLite Daemon

**A lightweight two-tier daemon for safe concurrent SQLite access across multiple processes and databases.**

## What is this?

A local database daemon that:
- ✅ **Serializes all writes** through a single process (no `SQLITE_BUSY` errors)
- ✅ **Multi-database support** - One daemon manages multiple databases independently
- ✅ **Database maintenance** - Safe file replacement for sync operations
- ✅ **Allows direct read-only access** for low-latency reads  
- ✅ **Auto-starts** when needed with smart lifecycle management
- ✅ **Uses SQLite WAL mode** for concurrent readers
- ✅ **Simple JSON protocol** over Windows Named Pipes (or Unix sockets)

## Quick Start

### 1. Start the daemon

```powershell
# Start the daemon (it manages all databases in the specified directory)
.\target\release\skylinedb-daemon.exe

# Or specify a custom database directory
.\target\release\skylinedb-daemon.exe D:\MyApp\data
```

**Architecture:**
- **Router Daemon**: Main process that accepts client connections (30-minute idle timeout)
- **Worker Daemons**: One per database file, spawned on-demand (5-minute idle timeout)
- **Multi-DB Support**: Access multiple databases through a single daemon instance

**Single Instance Protection:**
```powershell
# First instance starts
PS> .\skylinedb-daemon.exe
✓ Daemon started

# Second instance is blocked
PS> .\skylinedb-daemon.exe
✗ Error: Another daemon instance is already running!

# Check if running
PS> .\skylinedb-cli.exe ping --db data.db
✓ Daemon is running

# Stop daemon
PS> .\skylinedb-cli.exe shutdown
✓ Daemon stopped
```

**What happens:**
- ✅ Router daemon starts and listens on `\\.\pipe\SkylineDBd-v1`
- ✅ Worker daemons spawned on-demand for each database
- ✅ Each database gets independent WAL files: `db.db-wal`, `db.db-shm`
- ✅ Workers auto-shutdown after 5 minutes of inactivity
- ✅ Router stays alive longer (30 minutes) to quickly spawn workers

### 2. Work with databases

**Multiple databases through one daemon:**
```powershell
# Work with different databases
.\target\release\skylinedb-cli.exe exec --db galaxy.db "CREATE TABLE stars (id INTEGER, name TEXT)"
.\target\release\skylinedb-cli.exe exec --db users.db "CREATE TABLE users (id INTEGER, name TEXT)"
.\target\release\skylinedb-cli.exe exec --db settings.db "CREATE TABLE config (key TEXT, value TEXT)"

# Each database operates independently
.\target\release\skylinedb-cli.exe ping --db galaxy.db
.\target\release\skylinedb-cli.exe ping --db users.db
```

**Write data (via daemon):**

```powershell
# Single statement
.\target\release\skylinedb-cli.exe exec --db galaxy.db "INSERT INTO stars (name) VALUES ('Sirius')"

# Batch operations (recommended for performance)
.\target\release\skylinedb-cli.exe exec --db galaxy.db \
  "INSERT INTO stars (name) VALUES ('Betelgeuse')" \
  "INSERT INTO stars (name) VALUES ('Rigel')" \
  "UPDATE meta SET last_sync = datetime('now')"

# Complex operations - all atomic
.\target\release\skylinedb-cli.exe exec --db galaxy.db \
  "DELETE FROM stars WHERE brightness < 0.5" \
  "UPDATE stars SET catalog_id = id WHERE catalog_id IS NULL" \
  "INSERT INTO audit_log (action) VALUES ('cleanup')"
```

All statements in one `exec` call are executed in a **single atomic transaction**.

All writes **must** go through the daemon to ensure serialization.

### 3. Read data (direct access)

```rust
use sqlx::SqlitePool;

// Open read-only connection (no daemon needed!)
let pool = SqlitePool::connect("sqlite:galaxy.db?mode=ro").await?;

// Query directly
let stars = sqlx::query!("SELECT * FROM stars")
    .fetch_all(&pool)
    .await?;
```

Reads bypass the daemon for **maximum performance**.

### 4. Database Maintenance & File Replacement

**For Google Drive sync and database file replacement:**

```powershell
# Step 1: Prepare database for maintenance (checkpoint WAL)
.\target\release\skylinedb-cli.exe prepare-for-maintenance --db galaxy.db
# ✓ WAL checkpointed - all data flushed to main .db file
# ✓ Now you can calculate hash of galaxy.db

# Step 2: Close database (releases file locks)
.\target\release\skylinedb-cli.exe close-database --db galaxy.db
# ✓ File locks released - safe to replace galaxy.db file

# Step 3: Replace the database file
# (Your sync service downloads and replaces galaxy.db here)
Copy-Item -Force galaxy-new.db galaxy.db

# Step 4: Reopen database (resume operations)
.\target\release\skylinedb-cli.exe reopen-database --db galaxy.db
# ✓ Database reopened - operations resume

# Meanwhile, other databases continue working!
.\target\release\skylinedb-cli.exe exec --db users.db "INSERT INTO users (name) VALUES ('Alice')"
# ✓ Works! Other databases unaffected by maintenance
```

**Key Points:**
- ✅ `prepare-for-maintenance` checkpoints WAL → only `.db` file needs syncing
- ✅ `close-database` releases all file locks → safe for file replacement
- ✅ `reopen-database` reopens the new file → operations resume
- ✅ **Other databases keep working** during maintenance
- ✅ Operations on closed DB get clear error: "Database is closed for maintenance"

See `MAINTENANCE_GUIDE.md` for detailed integration instructions.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                          Your Apps                               │
└────────────────────────────┬────────────────────────────────────┘
                             │
                             ▼
                ┌────────────────────────────┐
                │   Router Daemon            │
                │   (Main Entry Point)       │
                │                            │
                │  • Routes by database      │
                │  • Spawns workers          │
                │  • 30-min idle timeout     │
                └────────────┬───────────────┘
                             │
        ┌────────────────────┼────────────────────┐
        │                    │                    │
        ▼                    ▼                    ▼
┌───────────────┐    ┌───────────────┐    ┌───────────────┐
│ Worker:       │    │ Worker:       │    │ Worker:       │
│ galaxy.db     │    │ users.db      │    │ settings.db   │
│               │    │               │    │               │
│ • WAL mode    │    │ • Independent │    │ • Per-DB      │
│ • Serialized  │    │   maintenance │    │   state       │
│   writes      │    │   state       │    │ • 5-min idle  │
│ • Direct reads│    │               │    │   timeout     │
└───────────────┘    └───────────────┘    └───────────────┘
```

**Key Points:**
- **Router daemon** accepts all client connections
- **Worker daemons** spawned per database on first access
- Each worker serializes writes for its database
- Apps can read directly from `.db` files (WAL mode)
- Closing one database doesn't affect others

## Protocol

**Transport:** Length-prefixed JSON over named pipe

**Request:**
```json
{
  "type": "ExecBatch",
  "db": "galaxy.db",
  "stmts": [
    {
      "sql": "INSERT INTO stars (name, magnitude) VALUES (?, ?)",
      "params": ["Sirius", -1.46]
    }
  ],
  "tx": "atomic"
}
```

**Response:**
```json
{
  "status": "ok",
  "rev": 43,
  "rows_affected": 1
}
```

**Maintenance Commands:**

```json
// Prepare for maintenance
{
  "type": "PrepareForMaintenance",
  "db": "galaxy.db"
}
// Response: { "status": "ok", "checkpointed": true }

// Close database
{
  "type": "CloseDatabase",
  "db": "galaxy.db"
}
// Response: { "status": "ok", "closed": true }

// Reopen database
{
  "type": "ReopenDatabase",
  "db": "galaxy.db"
}
// Response: { "status": "ok", "reopened": true, "rev": 43 }
```

See `daemon/src/protocol.rs` for full types.

**Response:**
```json
{
  "status": "ok",
  "rev": 42,
  "rows_affected": 1
}
```

See `daemon/src/protocol.rs` for full types.

## CLI Usage

### Check daemon status
```powershell
.\target\release\skylinedb-cli.exe ping
```

### Execute SQL
```powershell
.\target\release\skylinedb-cli.exe exec "CREATE TABLE tasks (id INTEGER PRIMARY KEY, title TEXT)"
.\target\release\skylinedb-cli.exe exec "INSERT INTO tasks (title) VALUES ('First')" "INSERT INTO tasks (title) VALUES ('Second')"
```

### Shutdown daemon

**Using CLI (recommended):**
```powershell
# Graceful shutdown - closes all databases and stops daemon
.\target\release\skylinedb-cli.exe shutdown
```

**Forceful shutdown (if needed):**
```powershell
# Find and kill the daemon process
Stop-Process -Name skylinedb-daemon -Force

# Or use Task Manager to end "skylinedb-daemon.exe"
```

**What happens during shutdown:**
- ✅ All active database workers are notified
- ✅ In-flight operations complete gracefully
- ✅ All database connections close properly
- ✅ WAL files are checkpointed
- ✅ File locks are released
- ✅ Named pipe is closed

**When to restart:**
- After forceful shutdown (to ensure clean state)
- After updating daemon binary
- When troubleshooting connection issues

**Note:** The daemon auto-restarts on next client request if using a service manager, or you can start it manually again.

## Building

```powershell
# Build release binaries
cargo build --release
```

**Produces:**
- `target/release/skylinedb-daemon.exe` - The daemon  
- `target/release/skylinedb-cli.exe` - CLI tool

**If build fails with "Access is denied":**
```powershell
# The daemon is running and holding the .exe file
# Stop it first:
.\target\release\skylinedb-cli.exe shutdown

# Then rebuild:
cargo build --release
```

## How Apps Should Connect

### Concurrent Connections

✅ **Multiple apps can connect simultaneously**
- Each connection is handled in a separate async task
- Writes are serialized through the actor (no conflicts)
- Reads can happen concurrently (WAL mode)
- No connection limit (bounded by system resources)

### Option 1: Implement protocol yourself

Apps can implement the simple JSON protocol directly in any language:

1. Connect to `\\.\pipe\SkylineDBd-v1`
2. Send length-prefixed JSON requests
3. Receive length-prefixed JSON responses

Example in Python, C#, Go, etc. – just implement the 4-byte length prefix + JSON.

**Protocol Format:**
```
┌────────────────┬──────────────────────┐
│  4 bytes (u32) │   N bytes (JSON)     │
│  little-endian │   UTF-8 encoded      │
└────────────────┴──────────────────────┘
```

### Option 2: Spawn daemon on first write

If the pipe doesn't exist:
1. Try to acquire named mutex `Global\SkylineDBd-v1`
2. If acquired, spawn daemon in background
3. Wait/retry connection with exponential backoff
4. If not acquired, another process is starting it – just retry

See `ARCHITECTURE.md` for full details.

---

## Protocol Implementation Examples

### Python Example

```python
import socket
import json
import struct

def send_request(request):
    # Connect to named pipe (Windows)
    sock = socket.socket(socket.AF_UNIX)  # On Unix
    # On Windows: use win32pipe instead
    sock.connect(r'\\.\pipe\SkylineDBd-v1')
    
    # Serialize request
    json_bytes = json.dumps(request).encode('utf-8')
    length = struct.pack('<I', len(json_bytes))  # Little-endian u32
    
    # Send length + JSON
    sock.sendall(length + json_bytes)
    
    # Read response length
    resp_len = struct.unpack('<I', sock.recv(4))[0]
    
    # Read response JSON
    resp_json = sock.recv(resp_len).decode('utf-8')
    response = json.loads(resp_json)
    
    sock.close()
    return response

# Execute a batch
response = send_request({
    "type": "ExecBatch",
    "stmts": [
        {
            "sql": "INSERT INTO tasks (title, status) VALUES (?, ?)",
            "params": ["Buy groceries", "pending"]
        },
        {
            "sql": "DELETE FROM tasks WHERE status = ?",
            "params": ["done"]
        }
    ],
    "tx": "atomic"
})

print(f"Revision: {response['rev']}, Rows affected: {response['rows_affected']}")
```

### C# Example

```csharp
using System;
using System.IO;
using System.IO.Pipes;
using System.Text;
using System.Text.Json;

public class DaemonClient
{
    public async Task<JsonDocument> SendRequest(object request)
    {
        using var pipe = new NamedPipeClientStream(".", "SkylineDBd-v1", 
            PipeDirection.InOut, PipeOptions.Asynchronous);
        
        await pipe.ConnectAsync();
        
        // Serialize request
        var json = JsonSerializer.Serialize(request);
        var jsonBytes = Encoding.UTF8.GetBytes(json);
        var length = BitConverter.GetBytes((uint)jsonBytes.Length);
        
        // Send length + JSON
        await pipe.WriteAsync(length, 0, 4);
        await pipe.WriteAsync(jsonBytes, 0, jsonBytes.Length);
        await pipe.FlushAsync();
        
        // Read response length
        var respLenBytes = new byte[4];
        await pipe.ReadAsync(respLenBytes, 0, 4);
        var respLen = BitConverter.ToUInt32(respLenBytes, 0);
        
        // Read response JSON
        var respBytes = new byte[respLen];
        await pipe.ReadAsync(respBytes, 0, (int)respLen);
        var respJson = Encoding.UTF8.GetString(respBytes);
        
        return JsonDocument.Parse(respJson);
    }
    
    public async Task<int> ExecBatch(params (string sql, object[] args)[] stmts)
    {
        var request = new
        {
            type = "ExecBatch",
            stmts = stmts.Select(s => new { sql = s.sql, @params = s.args }),
            tx = "atomic"
        };
        
        var response = await SendRequest(request);
        return response.RootElement.GetProperty("rev").GetInt32();
    }
}

// Usage
var client = new DaemonClient();
var rev = await client.ExecBatch(
    ("INSERT INTO tasks (title) VALUES (?)", new object[] { "Task 1" }),
    ("DELETE FROM tasks WHERE id = ?", new object[] { 42 })
);
Console.WriteLine($"New revision: {rev}");
```

### Go Example

```go
package main

import (
	"encoding/binary"
	"encoding/json"
	"io"
	"net"
)

func sendRequest(req interface{}) (map[string]interface{}, error) {
	// Connect to named pipe
	conn, err := net.Dial("unix", "/tmp/skylinedb-v1.sock")
	// On Windows: use winio or similar for named pipes
	if err != nil {
		return nil, err
	}
	defer conn.Close()
	
	// Serialize request
	jsonBytes, _ := json.Marshal(req)
	length := uint32(len(jsonBytes))
	
	// Send length (little-endian)
	binary.Write(conn, binary.LittleEndian, length)
	conn.Write(jsonBytes)
	
	// Read response length
	var respLen uint32
	binary.Read(conn, binary.LittleEndian, &respLen)
	
	// Read response JSON
	respBytes := make([]byte, respLen)
	io.ReadFull(conn, respBytes)
	
	var response map[string]interface{}
	json.Unmarshal(respBytes, &response)
	
	return response, nil
}

// Execute batch
response, _ := sendRequest(map[string]interface{}{
	"type": "ExecBatch",
	"stmts": []map[string]interface{}{
		{"sql": "INSERT INTO tasks (title) VALUES (?)", "params": []interface{}{"Task"}},
		{"sql": "DELETE FROM old_tasks WHERE created < ?", "params": []interface{}{"2024-01-01"}},
	},
	"tx": "atomic",
})

fmt.Printf("Revision: %.0f\n", response["rev"])
```

## Configuration

### SQLite Settings

The daemon uses optimized, production-ready settings:

- `PRAGMA journal_mode=WAL` - **Write-Ahead Logging** (better than classic rollback journal)
  - Allows concurrent reads during writes
  - Crash-safe with better performance
  - Readers never block writers, writers never block readers
  
- `PRAGMA synchronous=NORMAL` - Fast writes while maintaining crash safety
  - Guarantees database integrity after OS crash
  - Better performance than `FULL` mode
  
- `PRAGMA busy_timeout=5000` - 5 second retry on locks
  
- `PRAGMA wal_autocheckpoint=1000` - Checkpoint every 1000 pages
  - Keeps WAL file size reasonable
  - Automatic cleanup

### Batch Operations

All statements in an `ExecBatch` request are executed in a **single transaction** when `tx: "atomic"` is set:

```json
{
  "type": "ExecBatch",
  "stmts": [
    {"sql": "DELETE FROM logs WHERE date < ?", "params": ["2024-01-01"]},
    {"sql": "UPDATE users SET active = 0 WHERE last_login < ?", "params": ["2024-06-01"]},
    {"sql": "INSERT INTO audit (action) VALUES (?)", "params": ["cleanup"]}
  ],
  "tx": "atomic"
}
```

**Benefits:**
- ✅ All succeed or all fail (atomicity)
- ✅ Single fsync = faster than multiple separate writes
- ✅ Consistent database state
- ✅ Automatic rollback on error

## Monitoring

Check daemon logs (stdout) for:
- Connection events
- Write batch statistics
- Error conditions
- Idle shutdown

Set `RUST_LOG=debug` for verbose logging.

## Performance

**Latency:**
- Reads (direct): 1-10ms
- Writes (via daemon): 5-20ms per batch
- Batching amortizes IPC cost

**Throughput:**
- Writes: ~200-500 batches/sec (SQLite bound)
- Reads: 10k-100k queries/sec (direct access)

**Tip:** Batch multiple statements into one `ExecBatch` request.

## Limitations

- **Single database per daemon** - Run multiple daemons for multiple DBs
- **No distributed writes** - Only for single-machine concurrency
- **15 minute idle timeout** - Daemon exits if unused (restarts on demand)
- **Windows/Unix only** - Uses platform-specific IPC

## Reliability & Safety

### Safety Guarantees

✅ **No lost writes** - Actor serializes all writes, impossible to have concurrent write conflicts  
✅ **No SQLITE_BUSY errors** - Single writer pattern eliminates lock contention  
✅ **Crash recovery** - SQLite WAL mode + atomicity handles crashes gracefully  
✅ **No torn writes** - Transactions are atomic (all-or-nothing)  
✅ **Concurrent reads** - Multiple readers never block each other or the writer  
✅ **Process isolation** - Daemon crash doesn't affect client apps (they just retry)

### Reliability Assessment

| Aspect | Rating | Notes |
|--------|--------|-------|
| **Data integrity** | 🟢 Excellent | SQLite ACID + WAL mode |
| **Write consistency** | 🟢 Excellent | Actor serialization = no races |
| **Crash safety** | 🟢 Excellent | WAL journal recovers automatically |
| **Concurrent access** | 🟢 Excellent | Unlimited readers, serialized writes |
| **Network reliability** | 🔴 N/A | Local IPC only (named pipes) |
| **High availability** | 🟡 Good | Auto-restart possible, 15min idle timeout |
| **Backup/replication** | 🔴 None | Single file, no built-in replication |

### Known Limitations

- **Single machine only** - Uses local named pipes/Unix sockets
- **No distributed writes** - For multi-server, use different solution
- **15 minute idle timeout** - Daemon shuts down if unused (restarts on demand)
- **No built-in backups** - Standard SQLite backup strategies apply
- **Memory bound** - Daemon + all connections fit in RAM (typical: 20-200 MB)

### Resource Footprint

| Resource | Typical | Notes |
|----------|---------|-------|
| Daemon base | 10-20 MB | Idle daemon |
| Per connection | 1-2 MB | **While connected** |
| 10 concurrent clients | 30-40 MB | Typical usage |
| 100 concurrent clients | 150-220 MB | Heavy load |
| 200 brief requests | 50-100 MB | Spike, then drops |
| DB file size | Varies | Unlimited (SQLite limit: 281 TB) |
| CPU usage | < 1% idle | Varies with query load |

**Key:** Memory scales with **concurrent** connections, not total connection count!

**Connection lifetime:** As long as client keeps pipe open. Could be:
- Milliseconds (one-shot requests - lowest memory)
- Seconds (request-response cycles)  
- Minutes/hours (long-lived connections - sustained memory)

Most apps use one-shot pattern: connect → request → response → disconnect (brief memory spike).

## Files

```
sqlite_daemon/
├── daemon/          # The daemon binary
│   └── src/
│       ├── main.rs      # Entry point
│       ├── actor.rs     # Write serialization
│       ├── server.rs    # IPC server
│       ├── protocol.rs  # Request/response types
│       └── db.rs        # SQLite setup
├── cli/             # CLI tool
├── examples/        # Usage examples
└── ARCHITECTURE.md  # Detailed design doc
```

## License

MIT

---

**Questions?** Check `ARCHITECTURE.md` for the full design or dive into the source code!
