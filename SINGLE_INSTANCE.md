# Single Instance Protection

## Overview

The daemon implements **single-instance protection** to prevent multiple daemon processes from running simultaneously.

## Why This Matters

Without protection:
- ❌ Multiple daemons could connect to same database
- ❌ Conflicting pipe names
- ❌ Confusing for users (which daemon is active?)
- ❌ Resource waste (multiple processes)

With protection:
- ✅ Only one daemon runs at a time
- ✅ Clear error message if trying to start a second
- ✅ Automatic cleanup when daemon exits
- ✅ No conflicting processes

## How It Works

### Windows Implementation
Uses a **named mutex**: `Global\SkylineDBd-v1-SingleInstance`

```rust
CreateMutexW(
    None,
    true,  // Initial owner
    "Global\\SkylineDBd-v1-SingleInstance"
)
```

- Mutex is system-wide (all users can see it)
- Automatically released when process exits
- Even survives crashes (OS cleans up)

### Unix Implementation
Uses **file locking** with `flock()`: `/var/run/skylinedb-v1.lock`

```rust
flock(fd, LOCK_EX | LOCK_NB)  // Exclusive, non-blocking
```

- Lock file in `/var/run/`
- Contains PID for debugging
- Automatically released when process exits

## Behavior

### Starting First Instance ✅
```powershell
PS> .\skylinedb-daemon.exe
2025-11-07T04:00:00.000Z  INFO skylinedb_daemon: Starting SQLite daemon
2025-11-07T04:00:00.001Z  INFO skylinedb_daemon::single_instance: Acquired single-instance lock
2025-11-07T04:00:00.010Z  INFO skylinedb_daemon::server: IPC server listening
# ✓ Daemon running
```

### Trying to Start Second Instance ❌
```powershell
PS> .\skylinedb-daemon.exe
2025-11-07T04:01:00.000Z  INFO skylinedb_daemon: Starting SQLite daemon
Error: Failed to acquire single-instance lock

Caused by:
    Another daemon instance is already running!
    Only one daemon instance is allowed at a time.

    To check if daemon is running: skylinedb-cli.exe ping
    To stop existing daemon: skylinedb-cli.exe shutdown

# ✗ Exits with code 1 (does NOT kill existing daemon)
```

### After First Instance Exits ✅
```powershell
PS> .\skylinedb-cli.exe shutdown
✓ Daemon shutdown requested

# Wait 1-2 seconds for cleanup

PS> .\skylinedb-daemon.exe
2025-11-07T04:02:00.000Z  INFO skylinedb_daemon: Starting SQLite daemon
2025-11-07T04:02:00.001Z  INFO skylinedb_daemon::single_instance: Acquired single-instance lock
# ✓ New instance starts successfully
```

## Client App Usage

Your app should handle this gracefully:

### Pattern 1: Check Before Starting (Recommended)

```python
import subprocess
import socket

def is_daemon_running():
    try:
        # Try to ping daemon
        conn = connect_to_daemon()
        send_ping(conn)
        conn.close()
        return True
    except:
        return False

def ensure_daemon_running():
    if not is_daemon_running():
        # Start daemon
        subprocess.Popen(
            ['skylinedb-daemon.exe', r'D:\MyApp\data.db'],
            creationflags=subprocess.CREATE_NO_WINDOW
        )
        
        # Wait for startup
        for _ in range(10):
            time.sleep(0.5)
            if is_daemon_running():
                return True
        
        raise Exception("Daemon failed to start")
    
    # Already running
    return True
```

### Pattern 2: Try-Start with Error Handling

```python
def start_daemon_if_needed():
    try:
        result = subprocess.run(
            ['skylinedb-daemon.exe', r'D:\MyApp\data.db'],
            capture_output=True,
            text=True,
            timeout=2
        )
        
        if "already running" in result.stderr:
            # Another instance exists - this is OK!
            print("Daemon already running (using existing instance)")
            return True
        elif result.returncode != 0:
            raise Exception(f"Daemon error: {result.stderr}")
        
    except subprocess.TimeoutExpired:
        # Daemon started and is running in background
        return True
```

### Pattern 3: Simple Startup (Let It Fail)

```python
# Just try to start daemon (ignore if already running)
subprocess.Popen(
    ['skylinedb-daemon.exe', r'D:\MyApp\data.db'],
    creationflags=subprocess.CREATE_NO_WINDOW,
    stderr=subprocess.DEVNULL  # Ignore "already running" error
)

# Wait a moment then try to connect
time.sleep(1)
conn = connect_to_daemon()  # Will succeed whether we started it or not
```

## Edge Cases Handled

### Case 1: Daemon Crashes
```
Daemon process dies → Mutex/lock automatically released by OS
Next start attempt → Succeeds ✓
```

### Case 2: Force Kill (Task Manager)
```
Process killed → OS releases mutex/lock immediately
Next start attempt → Succeeds ✓
```

### Case 3: System Reboot
```
All processes exit → All mutexes/locks released
Next boot → Fresh start ✓
```

### Case 4: Multiple Users (Windows)
```
User A starts daemon → Creates Global\... mutex
User B tries to start → Blocked (global mutex) ✓

Note: This is intentional. One daemon per machine.
For per-user daemons, modify mutex name to use Local\ instead.
```

## Testing

### Test Single Instance Protection
```powershell
# Terminal 1
PS> .\target\release\skylinedb-daemon.exe
# Daemon starts

# Terminal 2
PS> .\target\release\skylinedb-daemon.exe
Error: Another daemon instance is already running!
# ✓ Correctly rejected
```

### Test Lock Cleanup
```powershell
PS> .\target\release\skylinedb-daemon.exe &
PS> .\target\release\skylinedb-cli.exe shutdown

# Wait 2 seconds

PS> .\target\release\skylinedb-daemon.exe
# ✓ Starts successfully (lock was released)
```

### Test Crash Recovery
```powershell
PS> .\target\release\skylinedb-daemon.exe &
PS> Get-Process skylinedb-daemon | Stop-Process -Force

# Wait 1 second (OS cleanup)

PS> .\target\release\skylinedb-daemon.exe
# ✓ Starts successfully (OS released lock)
```

## Implementation Details

### Code Location
- Module: `daemon/src/single_instance.rs`
- Acquisition: `main.rs` (before any initialization)
- Release: Automatic (Drop trait)

### Lock Timing
```
Daemon Startup:
1. Initialize logging           ← No lock yet
2. Acquire single-instance lock ← LOCK HERE (fail fast)
3. Parse arguments
4. Initialize database
5. Start IPC server
6. Run forever...
7. Shutdown
8. Drop guard                   ← UNLOCK HERE (automatic)
```

**Why acquire early?** Fail fast before wasting resources.

### Memory Overhead
- Windows mutex: ~4 KB
- Unix lock file: ~100 bytes on disk
- Negligible impact

## Future Enhancements

### Per-Database Locking (Future)
Currently: One daemon per machine
Future: One daemon per database file

```rust
// Lock based on database path hash
let mutex_name = format!(
    "Global\\SkylineDBd-v1-{}",
    hash_db_path(&db_path)
);
```

This would allow:
```powershell
# Different databases, different daemons ✓
.\skylinedb-daemon.exe D:\App1\data.db
.\skylinedb-daemon.exe D:\App2\data.db
```

### Per-User Daemons (Future)
Currently: Global (all users)
Future: Per-user option

```rust
// Windows: Use Local\ instead of Global\
let mutex_name = "Local\\SkylineDBd-v1-SingleInstance";

// Unix: Lock in user-specific directory
let lock_path = format!("{}/.skylinedb-v1.lock", env::var("HOME"));
```

## Summary

| Aspect | Behavior |
|--------|----------|
| **Protection method** | Windows: Named mutex, Unix: flock |
| **Scope** | System-wide (all users) |
| **Second instance** | Exits with error (does NOT kill first) |
| **Lock release** | Automatic on process exit |
| **Crash recovery** | OS releases lock, next start succeeds |
| **Overhead** | Negligible (~4 KB) |
| **Client handling** | Check with ping, or just try to start |

**Key takeaway:** Your app can safely try to start the daemon multiple times - only the first succeeds, others fail gracefully!
