# Database Maintenance Guide

**For Google Drive Sync and External Applications**

This guide explains how to safely replace database files while the daemon is running.

---

## Problem Statement

When syncing database files with Google Drive (or any cloud service), you need to:
1. Download a new version of the database file
2. Replace the local file with the downloaded version
3. Resume operations with the new file

**Challenge:** The daemon holds an exclusive lock on the database file, preventing replacement.

**Solution:** Use the maintenance commands to temporarily close the database.

---

## Three-Step Maintenance Protocol

### Step 1: Prepare for Maintenance

**Command:**
```json
{
  "type": "PrepareForMaintenance",
  "db": "galaxy.db"
}
```

**What it does:**
- Executes `PRAGMA wal_checkpoint(TRUNCATE)` to flush all Write-Ahead Log data to the main `.db` file
- The `.db-wal` file is truncated to 0 bytes (or deleted)
- All committed data is now in the main `galaxy.db` file

**Response:**
```json
{
  "status": "ok",
  "checkpointed": true
}
```

**Why this matters:**
- After checkpointing, you only need to sync the main `.db` file
- No need to sync `.db-wal` or `.db-shm` files
- You can now calculate a stable hash of `galaxy.db` for comparison

### Step 2: Close Database

**Command:**
```json
{
  "type": "CloseDatabase",
  "db": "galaxy.db"
}
```

**What it does:**
- Performs a final checkpoint (just to be safe)
- Closes the SQLite connection pool
- Releases all file locks on `galaxy.db`, `galaxy.db-wal`, and `galaxy.db-shm`
- Sets database state to "Closed"

**Response:**
```json
{
  "status": "ok",
  "closed": true
}
```

**Important:**
- Any operations on this database will now return error: `"Database is closed for maintenance"`
- **Other databases continue working normally**
- File system locks are fully released - safe to replace files

### Step 3: Reopen Database

**Command:**
```json
{
  "type": "ReopenDatabase",
  "db": "galaxy.db"
}
```

**What it does:**
- Opens the database file (which may be the new replaced file)
- Initializes SQLite with WAL mode
- Runs migrations if needed
- Sets database state to "Open"

**Response:**
```json
{
  "status": "ok",
  "reopened": true,
  "rev": 42
}
```

**If reopening fails:**
```json
{
  "status": "error",
  "error": "Failed to open database: disk I/O error"
}
```

---

## Complete Integration Example (Dart)

```dart
import 'dart:io';
import 'package:crypto/crypto.dart';

class GoogleDriveSyncService {
  final DaemonClient _daemon;
  
  /// Sync database with Google Drive
  Future<void> syncDatabase(String dbName) async {
    // Step 1: Prepare for maintenance (checkpoint WAL)
    print('Preparing $dbName for maintenance...');
    await _daemon.prepareForMaintenance(dbName);
    
    // Step 2: Calculate hash of local file
    final localFile = File(dbName);
    final localHash = await _calculateHash(localFile);
    print('Local hash: $localHash');
    
    // Step 3: Get hash from Google Drive metadata
    final driveHash = await _getDriveFileHash(dbName);
    print('Drive hash: $driveHash');
    
    if (localHash == driveHash) {
      print('Database is up to date, no sync needed');
      return;
    }
    
    // Step 4: Close database (release locks)
    print('Closing database...');
    await _daemon.closeDatabase(dbName);
    
    try {
      // Step 5: Download new file from Google Drive
      print('Downloading new database from Google Drive...');
      final tempFile = File('$dbName.tmp');
      await _downloadFromDrive(dbName, tempFile);
      
      // Step 6: Verify downloaded file
      final downloadedHash = await _calculateHash(tempFile);
      if (downloadedHash != driveHash) {
        throw Exception('Downloaded file hash mismatch!');
      }
      
      // Step 7: Atomically replace the old file
      print('Replacing database file...');
      await tempFile.rename(dbName);
      print('Database file replaced successfully');
      
    } catch (e) {
      print('Error during file replacement: $e');
      // If something went wrong, try to reopen the old file
    } finally {
      // Step 8: Reopen database (with new or old file)
      print('Reopening database...');
      final result = await _daemon.reopenDatabase(dbName);
      
      if (result.success) {
        print('Database reopened successfully, rev: ${result.rev}');
      } else {
        print('Failed to reopen database: ${result.error}');
        throw Exception('Database reopen failed: ${result.error}');
      }
    }
  }
  
  Future<String> _calculateHash(File file) async {
    final bytes = await file.readAsBytes();
    return md5.convert(bytes).toString();
  }
  
  Future<String> _getDriveFileHash(String dbName) async {
    // Your Google Drive API call to get file metadata
    // Return the MD5 hash stored in Drive metadata
  }
  
  Future<void> _downloadFromDrive(String dbName, File destination) async {
    // Your Google Drive API call to download file
  }
}
```

---

## Error Handling

### During Closed State

If a client tries to access a closed database:

**Request:**
```json
{
  "type": "ExecBatch",
  "db": "galaxy.db",
  "stmts": [{"sql": "INSERT INTO stars VALUES (1, 'Betelgeuse')", "params": []}]
}
```

**Response:**
```json
{
  "status": "error",
  "error": "Database is closed for maintenance",
  "code": "DATABASE_CLOSED"
}
```

**Your app should:**
1. Detect the `DATABASE_CLOSED` error code
2. Wait for maintenance to complete (poll or use a delay)
3. Retry the operation

```dart
Future<void> executeWithRetry(String db, String sql) async {
  const maxRetries = 5;
  const retryDelay = Duration(seconds: 2);
  
  for (int i = 0; i < maxRetries; i++) {
    try {
      await _daemon.exec(db, sql);
      return; // Success!
    } catch (e) {
      if (e.code == 'DATABASE_CLOSED') {
        print('Database under maintenance, retrying in ${retryDelay.inSeconds}s...');
        await Future.delayed(retryDelay);
        continue;
      }
      rethrow; // Other errors
    }
  }
  
  throw Exception('Failed after $maxRetries retries');
}
```

### Failed Reopen

If reopening fails (e.g., corrupted file), the database remains closed:

```json
{
  "status": "error",
  "error": "Failed to open database: file is not a database"
}
```

**Recovery options:**
1. Restore from backup
2. Re-download from Google Drive
3. Delete and reinitialize database

---

## Multi-Database Isolation

**Key Feature:** Maintenance on one database doesn't affect others.

```dart
// Close galaxy.db for maintenance
await _daemon.closeDatabase('galaxy.db');

// Meanwhile, other databases work fine
await _daemon.exec('users.db', 'INSERT INTO users ...');  // ✓ Works
await _daemon.exec('settings.db', 'UPDATE config ...');   // ✓ Works

// galaxy.db operations are blocked
await _daemon.exec('galaxy.db', 'INSERT INTO stars ...');  // ✗ Error: DATABASE_CLOSED

// Reopen galaxy.db
await _daemon.reopenDatabase('galaxy.db');

// Now everything works
await _daemon.exec('galaxy.db', 'INSERT INTO stars ...');  // ✓ Works
```

---

## Best Practices

### 1. Always Checkpoint Before Calculating Hash

```dart
// ✓ CORRECT
await _daemon.prepareForMaintenance('galaxy.db');
final hash = await calculateHash('galaxy.db');

// ✗ WRONG - hash may be incorrect if WAL has uncommitted data
final hash = await calculateHash('galaxy.db');
await _daemon.prepareForMaintenance('galaxy.db');
```

### 2. Use Atomic File Replacement

```dart
// ✓ CORRECT - atomic rename
final tempFile = File('galaxy.db.tmp');
await downloadFromDrive(tempFile);
await tempFile.rename('galaxy.db');  // Atomic operation

// ✗ WRONG - not atomic, can corrupt mid-copy
await File('galaxy.db').delete();
await downloadFromDrive(File('galaxy.db'));
```

### 3. Always Reopen in Finally Block

```dart
try {
  await _daemon.closeDatabase('galaxy.db');
  await replaceFile('galaxy.db');
} finally {
  // ALWAYS reopen, even if replacement failed
  await _daemon.reopenDatabase('galaxy.db');
}
```

### 4. Handle Reopen Failures

```dart
final result = await _daemon.reopenDatabase('galaxy.db');
if (!result.success) {
  // Log error
  logger.error('Failed to reopen galaxy.db: ${result.error}');
  
  // Try recovery
  await restoreFromBackup('galaxy.db');
  await _daemon.reopenDatabase('galaxy.db');
}
```

### 5. Only Sync the Main .db File

After `prepare-for-maintenance`, the WAL is empty:
- ✓ Sync: `galaxy.db` (contains all data)
- ✗ Don't sync: `galaxy.db-wal` (empty or deleted)
- ✗ Don't sync: `galaxy.db-shm` (transient shared memory)

---

## Testing Your Integration

### Test 1: Basic Maintenance Cycle

```dart
test('maintenance cycle works', () async {
  // Create test data
  await daemon.exec('test.db', 'CREATE TABLE test (id INTEGER)');
  await daemon.exec('test.db', 'INSERT INTO test VALUES (1)');
  
  // Maintenance cycle
  await daemon.prepareForMaintenance('test.db');
  await daemon.closeDatabase('test.db');
  
  // Verify operations are blocked
  expect(
    () => daemon.exec('test.db', 'INSERT INTO test VALUES (2)'),
    throwsA(hasCode('DATABASE_CLOSED'))
  );
  
  // Reopen
  await daemon.reopenDatabase('test.db');
  
  // Verify operations work again
  await daemon.exec('test.db', 'INSERT INTO test VALUES (2)');
});
```

### Test 2: Multi-Database Isolation

```dart
test('other databases unaffected by maintenance', () async {
  // Setup databases
  await daemon.exec('db1.db', 'CREATE TABLE test (id INTEGER)');
  await daemon.exec('db2.db', 'CREATE TABLE test (id INTEGER)');
  
  // Close db1
  await daemon.closeDatabase('db1.db');
  
  // db2 should still work
  await daemon.exec('db2.db', 'INSERT INTO test VALUES (1)');  // ✓
  
  // db1 should be blocked
  expect(
    () => daemon.exec('db1.db', 'INSERT INTO test VALUES (1)'),
    throwsA(hasCode('DATABASE_CLOSED'))
  );
});
```

### Test 3: File Replacement

```dart
test('file replacement works', () async {
  // Create original database
  await daemon.exec('test.db', 'CREATE TABLE test (id INTEGER)');
  await daemon.exec('test.db', 'INSERT INTO test VALUES (1)');
  
  // Create replacement database
  await daemon.exec('new.db', 'CREATE TABLE test (id INTEGER)');
  await daemon.exec('new.db', 'INSERT INTO test VALUES (999)');
  
  // Maintenance cycle
  await daemon.prepareForMaintenance('test.db');
  await daemon.closeDatabase('test.db');
  
  // Replace file
  await File('new.db').copy('test.db');
  
  // Reopen
  await daemon.reopenDatabase('test.db');
  
  // Verify new data
  final result = await readDirect('test.db', 'SELECT id FROM test');
  expect(result.first['id'], equals(999));
});
```

---

## Protocol Reference

### PrepareForMaintenance

**Request:**
```json
{
  "type": "PrepareForMaintenance",
  "db": "galaxy.db"
}
```

**Success Response:**
```json
{
  "status": "ok",
  "checkpointed": true
}
```

**Error Response:**
```json
{
  "status": "error",
  "error": "Database is already closed"
}
```

### CloseDatabase

**Request:**
```json
{
  "type": "CloseDatabase",
  "db": "galaxy.db"
}
```

**Success Response:**
```json
{
  "status": "ok",
  "closed": true
}
```

**Error Response:**
```json
{
  "status": "error",
  "error": "Database is already closed"
}
```

### ReopenDatabase

**Request:**
```json
{
  "type": "ReopenDatabase",
  "db": "galaxy.db"
}
```

**Success Response:**
```json
{
  "status": "ok",
  "reopened": true,
  "rev": 42
}
```

**Error Response:**
```json
{
  "status": "error",
  "error": "Failed to open database: disk I/O error"
}
```

---

## Frequently Asked Questions

### Q: Can I skip PrepareForMaintenance?

**A:** No! If you skip it, the WAL file may contain uncommitted transactions. Your hash calculation will be incorrect, and you might sync stale data.

### Q: Do I need to sync the WAL and SHM files?

**A:** No! After `PrepareForMaintenance`, all data is in the main `.db` file. The WAL is empty/truncated.

### Q: What happens if I kill the daemon during maintenance?

**A:** SQLite's WAL mode ensures atomicity. If you kill the daemon:
- Any committed transactions are safe in the `.db` file
- The new daemon instance will recover automatically
- Just restart the daemon and call `ReopenDatabase`

### Q: Can I perform maintenance on multiple databases simultaneously?

**A:** Yes! Each database has independent state. You can close multiple databases and replace them concurrently.

### Q: How long should I wait between Close and Reopen?

**A:** As long as needed! The database stays closed until you explicitly reopen it. Take your time downloading and replacing the file.

### Q: What if reopening fails?

**A:** The database remains closed. You can:
1. Fix the issue (restore backup, re-download, etc.)
2. Try reopening again
3. The daemon continues serving other databases

---

## Summary

**Safe Database Replacement Flow:**

1. **PrepareForMaintenance** → Checkpoint WAL
2. **Calculate hash** → Compare with remote
3. **CloseDatabase** → Release locks
4. **Replace file** → Atomic rename
5. **ReopenDatabase** → Resume operations

**Remember:**
- ✅ Other databases keep working
- ✅ Only main `.db` file needs syncing
- ✅ Always reopen, even if replacement fails
- ✅ Handle `DATABASE_CLOSED` errors gracefully
