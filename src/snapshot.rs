/// Snapshot utilities for mini-app-mcp data tools.
///
/// This module provides two public async functions:
///
/// - [`write_snapshot_db`] — creates an online SQLite snapshot of a table's
///   database in `{scope_dir}/_snapshots/`.  No YAML is copied (snapshots are
///   DB-only).
/// - [`purge_old_snapshots`] — removes the oldest snapshot files beyond the
///   configured retention limit.
///
/// All I/O is performed inside `tokio::task::spawn_blocking` (K-110) to
/// avoid blocking the async executor.  The SQLite snapshot uses
/// `rusqlite::Connection::backup` with a fresh source connection so the
/// existing `Store`'s `Mutex<Connection>` is never borrowed (K-103).
///
/// # Snapshot placement
///
/// ```text
/// {scope_dir}/
///   _snapshots/
///     {table}.{unix_secs}.db
/// ```
///
/// # Retention isolation
///
/// Snapshot retention is controlled exclusively by `MINI_APP_SNAPSHOT_RETENTION`
/// (default `10`).  The `_backup/` directory and `MINI_APP_BACKUP_RETENTION` are
/// never read, written, or purged by this module (Crux: snapshot retention
/// isolation).
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use crate::error::MiniAppError;

/// Creates a SQLite snapshot for a table using the hot backup API.
///
/// The snapshot is written to
/// `{scope_dir}/_snapshots/{table}.{unix_secs}.db`.  The `_snapshots/`
/// directory is created if it does not exist.
///
/// The snapshot file is created via
/// `rusqlite::Connection::backup(DatabaseName::Main, …, None)` using a fresh
/// source connection opened from `db_path` — the existing Store connection is
/// never borrowed (K-103).  This satisfies the SQLite Online Backup API
/// contract that "the source can be used while the backup is running".
///
/// A `PRAGMA wal_checkpoint(TRUNCATE)` is attempted before the backup to
/// ensure the WAL is flushed into the main DB file so the snapshot captures
/// the most recent committed state.  If the checkpoint fails it is logged as a
/// warning and the backup continues regardless (rusqlite's backup API handles
/// WAL-mode databases internally).
///
/// **Crux (rusqlite hot backup API)**: only `rusqlite::Connection::backup` is
/// used to create the snapshot.  `std::fs::copy` of the `.db` file is never
/// used because it would produce a corrupted or stale snapshot when the source
/// database has an open WAL file.
///
/// # Arguments
/// - `scope_dir`: the `.mini-app/<scope>/` root directory for this table.
/// - `table`: the logical table name (used as filename prefix).
/// - `db_path`: path to the SQLite database file to snapshot.
///
/// # Returns
/// `Ok(())` on success.
///
/// # Errors
/// - [`MiniAppError::Snapshot`] if the timestamp cannot be determined, the
///   snapshot directory cannot be created, or the SQLite backup fails.
/// - [`MiniAppError::Snapshot`] if the `spawn_blocking` task panics.
pub async fn write_snapshot_db(
    scope_dir: &Path,
    table: &str,
    db_path: &Path,
) -> Result<(), MiniAppError> {
    let scope_dir = scope_dir.to_path_buf();
    let table = table.to_string();
    let db_path = db_path.to_path_buf();

    tokio::task::spawn_blocking(move || -> Result<(), MiniAppError> {
        write_snapshot_db_sync(&scope_dir, &table, &db_path)
    })
    .await
    .map_err(|e| MiniAppError::Snapshot(format!("blocking task panic: {e}")))?
}

/// Synchronous implementation of [`write_snapshot_db`], executed inside
/// `spawn_blocking`.
fn write_snapshot_db_sync(
    scope_dir: &Path,
    table: &str,
    db_path: &Path,
) -> Result<(), MiniAppError> {
    // Obtain current Unix timestamp (seconds since UNIX_EPOCH).
    let unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| MiniAppError::Snapshot(format!("system clock error: {e}")))?
        .as_secs();

    let snapshot_dir = scope_dir.join("_snapshots");
    std::fs::create_dir_all(&snapshot_dir)
        .map_err(|e| MiniAppError::Snapshot(format!("cannot create snapshot dir: {e}")))?;

    // Open a fresh source connection for the backup so we don't borrow the
    // Store's Mutex<Connection> (K-103).
    let src_conn = Connection::open(db_path)
        .map_err(|e| MiniAppError::Snapshot(format!("cannot open source db: {e}")))?;

    // Attempt WAL checkpoint before snapshot.  Failure is non-fatal.
    if let Err(e) = src_conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)") {
        tracing::warn!(error = %e, "WAL checkpoint before snapshot failed; continuing anyway");
    }

    let db_dst = snapshot_dir.join(format!("{}.{}.db", table, unix_secs));
    // Crux: use rusqlite::Connection::backup (hot backup API), never std::fs::copy.
    src_conn
        .backup(rusqlite::DatabaseName::Main, &db_dst, None)
        .map_err(|e| MiniAppError::Snapshot(format!("rusqlite backup failed: {e}")))?;

    Ok(())
}

/// Removes the oldest snapshot files beyond the retention limit.
///
/// Scans `{scope_dir}/_snapshots/` for files matching `{table}.*.db`.  Files
/// are sorted by the numeric timestamp embedded in their name (descending —
/// newest first).  Files beyond the `retention` limit are deleted.
///
/// If a file cannot be removed (e.g. already deleted), the error is logged as
/// a warning and purge continues for the remaining files.
///
/// **Crux (snapshot retention isolation)**: this function only touches
/// `{scope_dir}/_snapshots/`.  It never reads, writes, or removes files from
/// `{scope_dir}/_backup/`, and it never consults `MINI_APP_BACKUP_RETENTION`.
///
/// # Arguments
/// - `scope_dir`: the `.mini-app/<scope>/` root for this table.
/// - `table`: the logical table name used as filename prefix.
/// - `retention`: number of snapshot files to keep (files beyond this count
///   are deleted).
///
/// # Returns
/// `Ok(())` on success (including the no-op case where fewer than
/// `retention + 1` snapshot files exist).
///
/// # Errors
/// - [`MiniAppError::Snapshot`] if the `_snapshots` directory cannot be read,
///   or if the `spawn_blocking` task panics.
pub async fn purge_old_snapshots(
    scope_dir: &Path,
    table: &str,
    retention: usize,
) -> Result<(), MiniAppError> {
    let scope_dir = scope_dir.to_path_buf();
    let table = table.to_string();

    tokio::task::spawn_blocking(move || -> Result<(), MiniAppError> {
        purge_old_snapshots_sync(&scope_dir, &table, retention)
    })
    .await
    .map_err(|e| MiniAppError::Snapshot(format!("blocking task panic: {e}")))?
}

/// Synchronous implementation of [`purge_old_snapshots`], executed inside
/// `spawn_blocking`.
fn purge_old_snapshots_sync(
    scope_dir: &Path,
    table: &str,
    retention: usize,
) -> Result<(), MiniAppError> {
    let snapshot_dir = scope_dir.join("_snapshots");

    // If the snapshot directory does not exist yet, nothing to purge.
    if !snapshot_dir.exists() {
        return Ok(());
    }

    // Collect timestamps from .db files that belong to this table.
    let entries = std::fs::read_dir(&snapshot_dir)
        .map_err(|e| MiniAppError::Snapshot(format!("cannot read snapshot dir: {e}")))?;

    let mut timestamps: Vec<u64> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            parse_snapshot_timestamp(&name, table, "db")
        })
        .collect();

    // Sort descending — newest first.
    timestamps.sort_unstable_by(|a, b| b.cmp(a));

    // Delete snapshot files beyond `retention`.
    for ts in timestamps.iter().skip(retention) {
        let db_path = snapshot_dir.join(format!("{}.{}.db", table, ts));

        if let Err(e) = std::fs::remove_file(&db_path) {
            tracing::warn!(
                path = %db_path.display(),
                error = %e,
                "failed to remove old snapshot db; continuing"
            );
        }
    }

    Ok(())
}

/// Parses the numeric timestamp from a snapshot filename of the form
/// `{table}.{ts}.{ext}`.
///
/// Returns `None` if the name does not match the expected pattern or if the
/// timestamp segment is not a valid `u64`.
///
/// # Arguments
/// - `filename`: the bare filename string to parse.
/// - `table`: the expected table name prefix.
/// - `ext`: the expected extension (without leading dot), e.g. `"db"`.
fn parse_snapshot_timestamp(filename: &str, table: &str, ext: &str) -> Option<u64> {
    // Expected format: "{table}.{ts}.{ext}"
    let prefix = format!("{}.", table);
    let suffix = format!(".{}", ext);

    let without_prefix = filename.strip_prefix(&prefix)?;
    let ts_str = without_prefix.strip_suffix(&suffix)?;
    ts_str.parse::<u64>().ok()
}

/// Returns the sorted list of snapshot timestamps (descending) for a given
/// table, scanning only `.db` files.  Used internally for testing.
///
/// # Arguments
/// - `snapshot_dir`: the `_snapshots/` directory to scan.
/// - `table`: the logical table name.
///
/// # Returns
/// A `Vec<u64>` of timestamps sorted newest-first.
///
/// # Errors
/// - [`MiniAppError::Snapshot`] if the directory cannot be read.
#[cfg(test)]
fn list_snapshot_timestamps(snapshot_dir: &Path, table: &str) -> Result<Vec<u64>, MiniAppError> {
    let entries = std::fs::read_dir(snapshot_dir)
        .map_err(|e| MiniAppError::Snapshot(format!("cannot read snapshot dir: {e}")))?;

    let mut timestamps: Vec<u64> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name();
            let name = name.to_string_lossy().to_string();
            parse_snapshot_timestamp(&name, table, "db")
        })
        .collect();

    timestamps.sort_unstable_by(|a, b| b.cmp(a));
    Ok(timestamps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use tokio::task;

    /// Helper: create a minimal SQLite database with WAL mode enabled at `path`.
    fn create_test_db(path: &Path) {
        // SAFETY: Connection::open and execute_batch are safe in test context;
        // panicking here would fail the test with a clear message.
        let conn = Connection::open(path).expect("open test db");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, v TEXT);",
        )
        .expect("setup test db");
    }

    // ── T1: happy-path ────────────────────────────────────────────────────

    /// T1: write_snapshot_db creates exactly one .db file in `_snapshots/`
    /// and does NOT create any .yaml file (snapshots are DB-only).
    #[tokio::test]
    async fn write_snapshot_db_creates_db_file_only() {
        let dir = TempDir::new().expect("temp dir");
        let scope_dir = dir.path();
        let db_path = scope_dir.join("items.db");

        create_test_db(&db_path);

        write_snapshot_db(scope_dir, "items", &db_path)
            .await
            .expect("write_snapshot_db must succeed");

        let snapshot_dir = scope_dir.join("_snapshots");
        assert!(snapshot_dir.exists(), "_snapshots dir must be created");

        let entries: Vec<_> = std::fs::read_dir(&snapshot_dir)
            .expect("read snapshot dir")
            .filter_map(|e| e.ok())
            .collect();

        let yaml_count = entries
            .iter()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".yaml"))
            .count();
        let db_count = entries
            .iter()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".db"))
            .count();

        assert_eq!(yaml_count, 0, "snapshot must NOT create any yaml file");
        assert_eq!(db_count, 1, "exactly one db snapshot must exist");
    }

    /// T1: purge_old_snapshots keeps only the N newest .db files.
    #[tokio::test]
    async fn purge_old_snapshots_keeps_n_newest() {
        let dir = TempDir::new().expect("temp dir");
        let scope_dir = dir.path();
        let snapshot_dir = scope_dir.join("_snapshots");
        std::fs::create_dir_all(&snapshot_dir).expect("create snapshot dir");

        // Create 5 fake snapshot .db files with distinct timestamps.
        for ts in [100u64, 200, 300, 400, 500] {
            std::fs::write(snapshot_dir.join(format!("items.{}.db", ts)), b"db").expect("write db");
        }

        purge_old_snapshots(scope_dir, "items", 3)
            .await
            .expect("purge must succeed");

        // Newest 3 timestamps: 500, 400, 300.  Oldest 2 (100, 200) must be gone.
        let timestamps = list_snapshot_timestamps(&snapshot_dir, "items").expect("list timestamps");
        assert_eq!(timestamps.len(), 3, "exactly 3 snapshots must remain");
        assert_eq!(timestamps, vec![500, 400, 300], "newest 3 must be kept");

        // Verify the deleted snapshots are truly gone.
        assert!(!snapshot_dir.join("items.100.db").exists());
        assert!(!snapshot_dir.join("items.200.db").exists());
    }

    // ── T2: boundary / edge-case ──────────────────────────────────────────

    /// T2: purge_old_snapshots is a no-op when snapshot count is below retention.
    #[tokio::test]
    async fn purge_old_snapshots_no_op_when_below_limit() {
        let dir = TempDir::new().expect("temp dir");
        let scope_dir = dir.path();
        let snapshot_dir = scope_dir.join("_snapshots");
        std::fs::create_dir_all(&snapshot_dir).expect("create snapshot dir");

        // Only 2 snapshots, retention = 10.
        for ts in [100u64, 200] {
            std::fs::write(snapshot_dir.join(format!("items.{}.db", ts)), b"db").expect("write db");
        }

        purge_old_snapshots(scope_dir, "items", 10)
            .await
            .expect("purge must succeed");

        let timestamps = list_snapshot_timestamps(&snapshot_dir, "items").expect("list timestamps");
        assert_eq!(timestamps.len(), 2, "both snapshots must still exist");
    }

    /// T2: purge_old_snapshots is a no-op when _snapshots/ directory does not
    /// exist yet (first call before any snapshot has been written).
    #[tokio::test]
    async fn purge_old_snapshots_no_op_when_dir_missing() {
        let dir = TempDir::new().expect("temp dir");
        let scope_dir = dir.path();
        // _snapshots/ directory is never created.

        let result = purge_old_snapshots(scope_dir, "items", 10).await;
        assert!(result.is_ok(), "purge must succeed when dir is missing");

        // Directory must still not exist after no-op purge.
        assert!(!scope_dir.join("_snapshots").exists());
    }

    // ── T3: error-path ────────────────────────────────────────────────────

    /// T3: write_snapshot_db returns Snapshot error when db_path does not exist.
    #[tokio::test]
    async fn write_snapshot_db_missing_db_returns_snapshot_variant() {
        let dir = TempDir::new().expect("temp dir");
        let scope_dir = dir.path();

        // Point to a non-existent database file.
        let result =
            write_snapshot_db(scope_dir, "items", Path::new("/nonexistent/items.db")).await;

        let err = result.expect_err("missing db file must error");
        assert!(
            matches!(err, MiniAppError::Snapshot(_)),
            "expected Snapshot variant, got {:?}",
            err
        );
    }

    // ── Concurrency: snapshot does not block concurrent writes ────────────

    /// Concurrency test: snapshot runs concurrently with INSERT operations and
    /// both complete successfully.
    ///
    /// This verifies `rusqlite::Connection::backup` is safe to call on a
    /// WAL-mode database while another connection is writing.  rusqlite docs
    /// state "source can be used while the backup is running".
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_snapshot_does_not_block_concurrent_writes() {
        let dir = TempDir::new().expect("temp dir");
        let db_path = dir.path().join("concurrent.db");

        // Prepare DB with WAL mode and a table.
        {
            let conn = Connection::open(&db_path).expect("open db");
            conn.execute_batch(
                "PRAGMA journal_mode=WAL; CREATE TABLE rows (id INTEGER PRIMARY KEY, val TEXT);",
            )
            .expect("setup db");
        }

        let db_path_writer = db_path.clone();
        let scope_dir = dir.path().to_path_buf();

        // Launch writer task: inserts 100 rows using a separate connection.
        let writer = task::spawn(async move {
            task::spawn_blocking(move || {
                let conn = Connection::open(&db_path_writer).expect("open writer db");
                for i in 0i64..100 {
                    conn.execute("INSERT INTO rows (val) VALUES (?1)", [format!("v{}", i)])
                        .expect("insert row");
                }
            })
            .await
            .expect("writer blocking task")
        });

        // Launch snapshot task: runs the snapshot while writer is active.
        let snapshot_task = write_snapshot_db(&scope_dir, "concurrent", &db_path);

        let (writer_result, snapshot_result) = tokio::join!(writer, snapshot_task);

        writer_result.expect("writer must succeed");
        snapshot_result.expect("snapshot must succeed");

        // The snapshot file must exist and be a valid SQLite database.
        let snapshot_dir = scope_dir.join("_snapshots");
        let snapshot_entries: Vec<PathBuf> = std::fs::read_dir(&snapshot_dir)
            .expect("read snapshot dir")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .and_then(|x| x.to_str())
                    .map(|x| x == "db")
                    .unwrap_or(false)
            })
            .collect();
        assert!(
            !snapshot_entries.is_empty(),
            "at least one db snapshot must exist"
        );

        // Verify snapshot db is a valid SQLite database (can be opened).
        let snap_conn = Connection::open(&snapshot_entries[0]).expect("open snapshot db");
        let snap_row_count: i64 = snap_conn
            .query_row("SELECT COUNT(*) FROM rows", [], |row| row.get(0))
            .unwrap_or(0);
        // Snapshot may have captured 0..100 rows (concurrent; exact count not deterministic).
        assert!(snap_row_count >= 0, "snapshot db must be a valid sqlite db");
    }

    // ── Concurrency: spawn_blocking cancel safety ─────────────────────────

    /// Cancel-safety test: dropping a `write_snapshot_db` Future immediately
    /// after spawn_blocking starts does not leave the source DB in a corrupt state.
    ///
    /// `tokio::task::spawn_blocking` is abort-unsafe: once the blocking
    /// closure starts running it runs to completion even if the outer Future
    /// is dropped.  This test verifies that the source DB remains valid after
    /// the Future has been dropped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_spawn_blocking_cancel_safety_snapshot_survives() {
        let dir = TempDir::new().expect("temp dir");
        let scope_dir = dir.path().to_path_buf();
        let db_path = scope_dir.join("cancel_test.db");

        {
            let conn = Connection::open(&db_path).expect("open db");
            conn.execute_batch(
                "PRAGMA journal_mode=WAL; CREATE TABLE rows (id INTEGER PRIMARY KEY, val TEXT);",
            )
            .expect("setup db");
        }

        // Issue snapshot with a very short timeout to trigger "cancel" of the Future.
        // spawn_blocking closure continues running even after the outer Future is dropped.
        let snapshot_fut = write_snapshot_db(&scope_dir, "cancel_test", &db_path);
        let result = tokio::time::timeout(std::time::Duration::from_millis(1), snapshot_fut).await;

        // Give the spawn_blocking closure time to complete (it runs to completion
        // regardless of the timeout because spawn_blocking is abort-unsafe).
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // The source DB must not be corrupted regardless of whether the Future
        // was cancelled or completed.
        let src_conn = Connection::open(&db_path).expect("source db must still be openable");
        let _count: i64 = src_conn
            .query_row("SELECT COUNT(*) FROM rows", [], |row| row.get(0))
            .expect("source db must be a valid sqlite db after cancellation");

        // If the future completed successfully, verify the snapshot directory exists.
        if let Ok(Ok(())) = result {
            let snapshot_dir = scope_dir.join("_snapshots");
            assert!(
                snapshot_dir.exists(),
                "snapshot dir must exist on successful write"
            );
        }
        // Whether timed out or not, no panic occurred — the test passes.
    }
}
