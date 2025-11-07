# Memory & Connection Behavior - Visual Guide

## Connection Lifecycle

```
┌─────────────────────────────────────────────────────────────┐
│                    Connection Timeline                       │
└─────────────────────────────────────────────────────────────┘

Client App                          Daemon
    │                                  │
    │  1. Connect to pipe              │
    ├──────────────────────────────────>│  Spawn Task (+1-2 MB)
    │                                  │
    │  2. Send Request                 │
    ├──────────────────────────────────>│  Actor processes
    │                                  │  (writes serialized)
    │  3. Receive Response             │
    │<──────────────────────────────────┤
    │                                  │
    │  [Connection can stay open]      │
    │                                  │
    │  4. Send Another Request         │
    ├──────────────────────────────────>│  Reuse same task
    │<──────────────────────────────────┤
    │                                  │
    │  5. Close pipe / Disconnect      │
    └──────────────────────────────────>│  Task ends (-1-2 MB)
                                       │
```

## Memory Patterns

### Pattern A: One-Shot Requests (Recommended)

```
Time:  0s        1s        2s        3s        4s
       │         │         │         │         │
Apps:  App1 ──┐             App2 ──┐
           connects         │   connects
              │             │      │
           request      App1 │  request
              │         close│     │
           response         │  response
              │             │     │
           closes           │  closes
                                 │

Memory:
  30 MB ──────┬───────┬─────────┬───────┬──────
  20 MB       │       │         │       │
  10 MB ──────┴───────┴─────────┴───────┴──────
              ↑ spike          ↑ spike
        (brief, ~100ms)   (brief, ~100ms)
```

**Result:** Low sustained memory, brief spikes

### Pattern B: Long-Lived Connections

```
Time:  0s        10s       20s       30s       40s
       │         │         │         │         │
App1:  ├─────────────────────────────────────────
       connects stays connected
       
App2:            ├────────────────────────────────
                 connects

App3:                      ├──────────────────────
                           connects

Memory:
  50 MB                    ┌──────────────────────
  30 MB          ┌─────────┤
  20 MB ─────────┤
  10 MB ┌────────┘
        ↑        ↑         ↑
     App1     App2       App3
     stays    stays      stays
```

**Result:** Sustained memory = (# connected apps × 1-2 MB)

### Pattern C: Burst Traffic

```
Time:  0s     1s     2s     3s     4s     5s
       │      │      │      │      │      │

Clients:
       10 ──────────┐
                    ├─ All disconnect
       50 ─────┐    │
               spike │
                    │

Memory:
 120 MB      ┌─┐    
  50 MB      │ │    
  30 MB ─────┘ └─────────────────────────
  10 MB ──────────────────────────────────
              ↑ spike
           (50 clients)
           (1 second)
```

**Result:** Brief spike, then back to baseline

## Real-World Example

**E-commerce App with 3 Services**

```
┌──────────────────┐
│   Web Backend   │───┐
└──────────────────┘   │
                       ├──> Daemon (15-25 MB base)
┌──────────────────┐   │         │
│   Order Service │───┤         DB
└──────────────────┘   │         │
                       │         │
┌──────────────────┐   │         │
│  Email Worker   │───┘         │
└──────────────────┘             │
                                 │
         All reading directly ───┘
         (no daemon for reads)
```

**Scenario:**
- 3 services write occasionally (keep-alive connections)
- Memory: 15 MB base + (3 × 2 MB) = **21 MB sustained**

**During flash sale burst:**
- 50 order requests/second for 5 seconds
- Memory spike: 21 MB → 120 MB → back to 21 MB
- Duration: ~5 seconds

## Path Handling Example

```
Your App Structure:
D:\
├── MyApp\
│   ├── bin\
│   │   └── skylinedb-daemon.exe
│   └── data\
│       ├── app.db         ← Your data
│       ├── app.db-wal     ← Auto-created
│       └── app.db-shm     ← Auto-created

Start Command:
PS> D:\MyApp\bin\skylinedb-daemon.exe "D:\MyApp\data\app.db"
                                        └─────┬─────────┘
                                     Explicit path
                                     (no surprise files!)

Result:
✅ DB created at: D:\MyApp\data\app.db
✅ NOT created at: D:\MyApp\bin\data.db (daemon's folder)
```

## Multiple Apps, Same DB

```
┌──────────────┐       ┌──────────────┐       ┌──────────────┐
│   App A      │       │   App B      │       │   App C      │
│ (Electron)   │       │ (Python)     │       │ (C# Service) │
└──────┬───────┘       └──────┬───────┘       └──────┬───────┘
       │                      │                      │
       │  All write via daemon (serialized)          │
       └──────────────────────┼──────────────────────┘
                              │
                    ┌─────────▼─────────┐
                    │  Daemon Process   │
                    │    20-30 MB       │
                    └─────────┬─────────┘
                              │
                        ┌─────▼──────┐
                        │   app.db   │
                        │   (WAL)    │
                        └────────────┘
                              ▲
       ┌──────────────────────┼──────────────────────┐
       │                      │                      │
   Direct read           Direct read            Direct read
   (no daemon)           (no daemon)            (no daemon)
```

**Memory Impact:**
- If all 3 apps keep connections open: 20 + (3 × 2) = **26 MB**
- If apps connect/disconnect per request: 20 MB base, spikes to 26 MB briefly

## Summary Table

| Usage Pattern | Memory Behavior | Example |
|--------------|-----------------|---------|
| **Idle daemon** | 10-20 MB flat | No clients connected |
| **1-10 clients, one-shot** | 10-40 MB with spikes | Web app, occasional saves |
| **10 clients, keep-alive** | 30-40 MB sustained | Background services |
| **100 burst requests** | Spike to 200 MB, drops fast | Batch imports |
| **50 long-lived clients** | 100-120 MB sustained | Real-time apps |

## Best Practice Recommendations

### ✅ DO: One-shot pattern (lowest memory)
```python
def save_data(data):
    conn = connect_daemon()
    send_write_request(conn, data)
    conn.close()  # Immediate cleanup
```

### ⚠️ ACCEPTABLE: Connection pooling
```python
class App:
    def __init__(self):
        self.daemon_conn = connect_daemon()  # Kept alive
    
    def save_data(self, data):
        send_write_request(self.daemon_conn, data)
    
    def __del__(self):
        self.daemon_conn.close()  # Cleanup on app exit
```

### ❌ AVOID: Unnecessary long-lived connections
```python
# Don't do this if you write infrequently!
conn = connect_daemon()
while True:
    time.sleep(60)  # Sleeping for minutes with open connection
    if need_to_write:
        send_request(conn, data)  # Wastes 1-2 MB for an hour!
```

---

**Key Takeaways:**

1. **Path:** `daemon.exe D:\path\to\db.db` - DB created exactly there ✅
2. **Memory:** Scales with **concurrent** connections, not total ✅
3. **Connection:** Lives as long as client keeps pipe open ✅
4. **Pattern:** Use one-shot for lowest memory footprint ✅
