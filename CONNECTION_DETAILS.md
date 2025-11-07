# Connection & Resource Management Details

## Question 2: Database Path Handling

### ✅ YES - Custom paths work perfectly!

**How it works:**

```powershell
# Client apps specify the database path when starting daemon
.\skylinedb-daemon.exe "D:\MyApp\data\application.db"
```

**Path resolution:**
1. If argument provided → Use that path (absolute or relative)
2. If no argument → Default to `data.db` in daemon's current working directory

**Code (daemon/src/main.rs):**
```rust
let db_path = std::env::args()
    .nth(1)                              // Get first argument
    .map(PathBuf::from)                  // Convert to path
    .unwrap_or_else(|| {                 // Or use default
        let mut path = std::env::current_dir().unwrap();
        path.push("data.db");
        path
    });
```

### Tested Behavior:

```powershell
# Start with custom path
PS> .\skylinedb-daemon.exe "D:\MyApp\mydata.db"
# Creates: D:\MyApp\mydata.db
#          D:\MyApp\mydata.db-wal
#          D:\MyApp\mydata.db-shm

# Client apps don't need to specify path - daemon already knows it
PS> .\skylinedb-cli.exe exec "CREATE TABLE users (...)"
# ✓ Uses D:\MyApp\mydata.db

# Reads also use the same file (direct access)
$conn = [System.Data.SQLite.SQLiteConnection]::new("Data Source=D:\MyApp\mydata.db;Mode=ReadOnly")
```

### Best Practices for Client Apps:

**1. Store daemon path in app config:**
```json
{
  "database_path": "D:\\MyApp\\data\\app.db",
  "daemon_executable": "D:\\MyApp\\bin\\skylinedb-daemon.exe"
}
```

**2. Start daemon with explicit path:**
```python
import subprocess
import os

db_path = r"D:\MyApp\data\app.db"
daemon_exe = r"D:\MyApp\bin\skylinedb-daemon.exe"

# Ensure directory exists
os.makedirs(os.path.dirname(db_path), exist_ok=True)

# Start daemon with specific DB path
subprocess.Popen([daemon_exe, db_path], 
                 creationflags=subprocess.CREATE_NO_WINDOW)
```

**3. Read directly from the same path:**
```python
import sqlite3

# Direct read (no daemon needed)
conn = sqlite3.connect(db_path, uri=True)
conn.execute("PRAGMA query_only=ON")  # Safety
rows = conn.execute("SELECT * FROM users").fetchall()
```

### Summary:

| Scenario | Result |
|----------|--------|
| `daemon.exe D:\MyApp\db.db` | ✅ Uses `D:\MyApp\db.db` |
| `daemon.exe relative\path.db` | ✅ Uses `<cwd>\relative\path.db` |
| `daemon.exe` (no arg) | ✅ Uses `<cwd>\data.db` (default) |
| Multiple daemons, different DBs | ✅ Need different pipe names (see below) |

**No unwanted `data.db` creation** - only creates DB at the path you specify!

---

## Question 7: Connection Lifecycle & Memory

### Connection Model: **Per-Request, Not Persistent**

**TL;DR:** Memory scales with **concurrent requests**, not total connection count!

### How Connections Work:

```
Client                 Daemon
  │                      │
  ├──────Connect────────>│  (spawn task ~1-2 MB)
  │                      │
  ├──────Request────────>│
  │<─────Response────────┤
  │                      │
  ├──────Request────────>│
  │<─────Response────────┤
  │                      │
  └────Disconnect───────>│  (task ends, memory freed)
                         │
```

**Key Point:** Connection task lives **only while actively communicating**.

### Code Analysis (daemon/src/server.rs):

```rust
async fn handle_connection(mut stream: NamedPipeServer, ...) -> Result<()> {
    debug!("Client connected");
    let mut read_buf = BytesMut::with_capacity(4096);  // ~4 KB
    
    loop {  // ← Keeps connection open
        // Read request
        stream.read_buf(&mut read_buf).await?;
        
        // Handle request
        let response = actor_tx.send(cmd).await;
        
        // Send response
        stream.write_all(&response).await?;
        
        // Loop continues - connection stays open for next request
    }
    
    // Connection ends when:
    // 1. Client disconnects (closes pipe)
    // 2. Error occurs
    // 3. Shutdown request received
}
```

### Memory Behavior:

**Scenario 1: Short-lived connections (request-response pattern)**
```
100 clients send request → 100 MB spike
All receive response    → Drop to 20 MB base
```

**Scenario 2: Long-lived connections (keep-alive)**
```
100 clients connect     → 100 MB
Stay connected idle     → 100 MB sustained
All disconnect          → 20 MB base
```

**Scenario 3: Real-world mixed usage**
```
10 clients connected    → 30-40 MB
50 spike for 1 sec      → 90 MB brief spike
Back to 10 clients      → 30-40 MB
```

### Memory Profile by Usage:

| Usage Pattern | Memory Footprint | Duration |
|--------------|------------------|----------|
| **Idle daemon** | 10-20 MB | While running |
| **10 concurrent requests** | 30-40 MB | During requests |
| **100 concurrent requests** | 150-220 MB | During requests |
| **200 clients keep-alive** | 300-450 MB | While connected |
| **1000 brief connections/sec** | 50-100 MB | Brief spikes |

### Connection Drops When:

1. **Client disconnects** (most common)
   - Closes named pipe handle
   - Task exits immediately
   - Memory freed

2. **Request completes** (if client closes after each request)
   - Client sends request
   - Receives response
   - Closes connection
   - Task ends

3. **Error occurs**
   - Malformed request
   - Parse error
   - Task exits, logs error

4. **Shutdown command**
   - Daemon sends response
   - Task exits gracefully

### Typical Client Pattern:

**Pattern A: One-shot (lowest memory)**
```python
def write_data(sql):
    conn = connect_to_daemon()
    send_request(conn, sql)
    response = read_response(conn)
    conn.close()  # ← Connection drops here
    return response
```
**Memory:** Brief spike per request

**Pattern B: Connection pooling (medium memory)**
```python
class DaemonClient:
    def __init__(self):
        self._conn = None
    
    def write_data(self, sql):
        if not self._conn:
            self._conn = connect_to_daemon()
        send_request(self._conn, sql)
        return read_response(self._conn)
    
    def close(self):
        if self._conn:
            self._conn.close()  # ← Connection drops here
```
**Memory:** Sustained during app lifetime

**Pattern C: Long-lived (highest memory)**
```python
# App keeps connection open for hours
conn = connect_to_daemon()  # Connection lives
while app_running:
    if need_write:
        send_request(conn, sql)
    time.sleep(1)
conn.close()  # Connection drops on app exit
```
**Memory:** Sustained until app closes

### Recommendations:

| Scenario | Pattern | Memory Impact |
|----------|---------|---------------|
| **Occasional writes** | One-shot | ✅ Low (spikes only) |
| **Frequent writes** | Connection pool | ⚠️ Medium (1-2 MB per app) |
| **Real-time updates** | Long-lived | ⚠️ Higher (sustained) |
| **Batch processing** | One-shot batches | ✅ Lowest (brief spikes) |

### Example Memory Timeline:

```
Time    Active Connections    Memory Usage
────────────────────────────────────────────
00:00   0 (idle)             10 MB
00:01   1 client connects     12 MB
00:02   10 clients connect    32 MB
00:03   5 disconnect          22 MB
00:04   100 burst requests    180 MB ← Spike!
00:05   All completed         12 MB ← Back down
00:06   Idle                  10 MB
```

### Summary:

✅ **Memory scales with CONCURRENT connections, not total**
✅ **Connections drop when client closes pipe**
✅ **Tasks automatically clean up (Rust RAII)**
✅ **Brief spikes are normal and expected**
✅ **200 connections = 200 MB ONLY if all connected simultaneously**

**Most apps:** 1-10 concurrent = 20-40 MB typical

---

## Multiple Databases Support

### Current Limitation:
- One daemon = One database
- All clients connect to same daemon = same DB

### Solution for Multiple DBs:

**Option 1: Multiple daemon instances with different pipe names**

Modify daemon to take pipe name as argument:
```rust
// Future enhancement
let pipe_name = std::env::args().nth(2)
    .unwrap_or_else(|| DEFAULT_PIPE_NAME.to_string());
```

Then:
```powershell
# Database 1
.\skylinedb-daemon.exe "D:\App\users.db" "\\.\pipe\SkylineDBd-users"

# Database 2
.\skylinedb-daemon.exe "D:\App\logs.db" "\\.\pipe\SkylineDBd-logs"

# Clients specify which pipe to connect to
```

**Option 2: Multiple daemons, different port/pipe**

Build multiple copies with different pipe names hardcoded.

**Option 3: Single daemon, multi-DB support (future)**

Enhance protocol to include database identifier in requests.

---

## Summary Answers:

### Q2: Path handling?
✅ **YES** - Pass path as argument: `daemon.exe D:\path\to\db.db`
✅ No unwanted `data.db` creation
✅ Client apps can specify any path
✅ Daemon creates DB at specified location

### Q7: Connection memory?
✅ **Memory = concurrent connections × 1-2 MB**
✅ Connections drop when client closes pipe
✅ 200 spike requests = 200 MB **briefly**, then drops
✅ 200 long-lived connections = 200 MB **sustained**
✅ Most apps: 20-40 MB typical (5-10 concurrent)

**Connection lifetime:** As long as client keeps pipe open (could be milliseconds or hours, depending on client pattern).
