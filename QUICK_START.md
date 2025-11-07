# Quick Start Guide

## 5-Minute Setup

### 1. Build the daemon (one time)
```powershell
cargo build --release
```

### 2. Start the daemon
```powershell
# In your app's data folder
cd D:\MyApp\data
D:\Projects\sqlite_daemon\target\release\skylinedb-daemon.exe myapp.db
```

The daemon is now **running in the background** and will:
- Accept connections on `\\.\pipe\SkylineDBd-v1`
- Stay alive until 15 minutes of inactivity
- Auto-restart when needed

### 3. Use from your app

**Write** (via daemon):
```python
# See README.md for full Python example
response = send_to_daemon({
    "type": "ExecBatch",
    "stmts": [
        {"sql": "INSERT INTO users (name) VALUES (?)", "params": ["Alice"]}
    ],
    "tx": "atomic"
})
```

**Read** (direct access, no daemon):
```python
import sqlite3
conn = sqlite3.connect("myapp.db", uri=True, check_same_thread=False)
conn.execute("PRAGMA query_only=ON")  # Safety
users = conn.execute("SELECT * FROM users").fetchall()
```

## Common Patterns

### Pattern 1: Multiple Inserts (batched)
```json
{
  "type": "ExecBatch",
  "stmts": [
    {"sql": "INSERT INTO logs (msg) VALUES (?)", "params": ["Log 1"]},
    {"sql": "INSERT INTO logs (msg) VALUES (?)", "params": ["Log 2"]},
    {"sql": "INSERT INTO logs (msg) VALUES (?)", "params": ["Log 3"]}
  ],
  "tx": "atomic"
}
```
✅ All 3 inserts in one transaction = fast!

### Pattern 2: Update + Insert (atomic)
```json
{
  "type": "ExecBatch",
  "stmts": [
    {"sql": "UPDATE users SET last_login = ? WHERE id = ?", "params": ["2025-11-07", 42]},
    {"sql": "INSERT INTO audit (user_id, action) VALUES (?, ?)", "params": [42, "login"]}
  ],
  "tx": "atomic"
}
```
✅ Both succeed or both fail!

### Pattern 3: Delete + Cleanup
```json
{
  "type": "ExecBatch",
  "stmts": [
    {"sql": "DELETE FROM temp_data WHERE created < datetime('now', '-7 days')"},
    {"sql": "VACUUM"}
  ],
  "tx": "atomic"
}
```

## FAQ

**Q: Where should I put the database file?**  
A: Anywhere! Pass the full path to the daemon: `skylinedb-daemon.exe D:\MyApp\data\app.db`

**Q: Can multiple apps connect at once?**  
A: Yes! Unlimited concurrent connections.

**Q: What happens if daemon crashes?**  
A: SQLite WAL mode ensures data integrity. Just restart daemon, no data loss.

**Q: How do I know daemon is running?**  
A: `skylinedb-cli.exe ping` - returns version and DB path if running.

**Q: Can I have multiple databases?**  
A: One daemon per database. Run multiple daemons on different pipes.

**Q: Performance vs raw SQLite?**  
A: Reads: identical (direct access). Writes: +5-10ms for IPC overhead. Batch to amortize.

**Q: Is it safe for production?**  
A: Yes, if your requirements are single-machine concurrent access. Same safety as SQLite itself.
