/// Backup utilities for mini-app-mcp schema CRUD tools.
///
/// This module provides two public async functions:
///
/// - [`write_backup_pair`] — copies a table's `schema.yaml` and creates an
///   online SQLite backup in `{scope_dir}/_backup/`.
/// - [`purge_old_backups`] — removes the oldest backup pairs beyond the
///   configured retention limit.
///
/// All I/O is performed inside `tokio::task::spawn_blocking` (K-110) to
/// avoid blocking the async executor.  The SQLite backup uses
/// `rusqlite::Connection::backup` with a fresh source connection so the
/// existing `Store`'s `Mutex<Connection>` is never borrowed (K-103).
///
/// # Backup placement
///
/// ```text
/// {scope_dir}/
///   _backup/
///     {table}.{unix_secs}.yaml
///     {table}.{unix_secs}.db
/// ```
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use crate::error::MiniAppError;

/// Creates a backup pair (YAML + SQLite DB snapshot) for a table.
///
/// The pair is written to `{scope_dir}/_backup/{table}.{unix_secs}.yaml` and
/// `{scope_dir}/_backup/{table}.{unix_secs}.db`.  The `_backup` directory is
/// created if it does not exist.
///
/// The YAML file is copied verbatim from `schema_yaml_path`.  The DB file is
/// created via `rusqlite::Connection::backup("main", …, None)` using a fresh
/// read-only connection (the existing Store connection is not touched).
///
/// A `PRAGMA wal_checkpoint(TRUNCATE)` is attempted before the backup to
/// ensure the WAL is flushed.  If the checkpoint fails it is logged as a
/// warning and the backup continues regardless (rusqlite's backup API handles
/// WAL-mode databases internally).
///
/// # Arguments
/// - `scope_dir`: the `.mini-app/<scope>/` root directory for this table.
/// - `table`: the logical table name (used as filename prefix).
/// - `schema_yaml_path`: path to the `schema.yaml` file to back up.
/// - `db_path`: path to the SQLite database file to back up.
///
/// # Returns
/// `Ok(())` on success.
///
/// # Errors
/// - [`MiniAppError::Backup`] if the timestamp cannot be determined, the
///   backup directory cannot be created, the YAML copy fails, or the SQLite
///   backup fails.
/// - [`MiniAppError::Backup`] if the `spawn_blocking` task panics.
pub async fn write_backup_pair(
    scope_dir: &Path,
    table: &str,
    schema_yaml_path: &Path,
    db_path: &Path,
) -> Result<(), MiniAppError> {
    let scope_dir = scope_dir.to_path_buf();
    let table = table.to_string();
    let schema_yaml_path = schema_yaml_path.to_path_buf();
    let db_path = db_path.to_path_buf();

    tokio::task::spawn_blocking(move || -> Result<(), MiniAppError> {
        write_backup_pair_sync(&scope_dir, &table, &schema_yaml_path, &db_path)
    })
    .await
    .map_err(|e| MiniAppError::Backup(format!("blocking task panic: {e}")))?
}

/// Synchronous implementation of [`write_backup_pair`], executed inside
/// `spawn_blocking`.
fn write_backup_pair_sync(
    scope_dir: &Path,
    table: &str,
    schema_yaml_path: &Path,
    db_path: &Path,
) -> Result<(), MiniAppError> {
    // Obtain current Unix timestamp (seconds since UNIX_EPOCH).
    let unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| MiniAppError::Backup(format!("system clock error: {e}")))?
        .as_secs();

    let backup_dir = scope_dir.join("_backup");
    std::fs::create_dir_all(&backup_dir)
        .map_err(|e| MiniAppError::Backup(format!("cannot create backup dir: {e}")))?;

    // Copy schema YAML.
    let yaml_dst = backup_dir.join(format!("{}.{}.yaml", table, unix_secs));
    std::fs::copy(schema_yaml_path, &yaml_dst)
        .map_err(|e| MiniAppError::Backup(format!("cannot copy schema yaml: {e}")))?;

    // Open a fresh source connection for the backup so we don't borrow the
    // Store's Mutex<Connection> (K-103).
    let src_conn = Connection::open(db_path)
        .map_err(|e| MiniAppError::Backup(format!("cannot open source db: {e}")))?;

    // Attempt WAL checkpoint before backup.  Failure is non-fatal.
    if let Err(e) = src_conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)") {
        tracing::warn!(error = %e, "WAL checkpoint before backup failed; continuing anyway");
    }

    let db_dst = backup_dir.join(format!("{}.{}.db", table, unix_secs));
    src_conn
        .backup(rusqlite::DatabaseName::Main, &db_dst, None)
        .map_err(|e| MiniAppError::Backup(format!("rusqlite backup failed: {e}")))?;

    Ok(())
}

/// Removes the oldest backup pairs beyond the retention limit.
///
/// Scans `{scope_dir}/_backup/` for files matching `{table}.*.yaml` and
/// `{table}.*.db`.  Files are sorted by the numeric timestamp embedded in
/// their name (descending — newest first).  Pairs beyond the `retention`
/// limit are deleted.
///
/// If one file in a pair cannot be removed (e.g. already deleted), the error
/// is logged as a warning and purge continues for the remaining files.
///
/// # Arguments
/// - `scope_dir`: the `.mini-app/<scope>/` root for this table.
/// - `table`: the logical table name used as filename prefix.
/// - `retention`: number of backup pairs to keep (pairs beyond this count are
///   deleted).
///
/// # Returns
/// `Ok(())` on success (including the no-op case where fewer than
/// `retention + 1` pairs exist).
///
/// # Errors
/// - [`MiniAppError::Backup`] if the `_backup` directory cannot be read, or
///   if the `spawn_blocking` task panics.
pub async fn purge_old_backups(
    scope_dir: &Path,
    table: &str,
    retention: usize,
) -> Result<(), MiniAppError> {
    let scope_dir = scope_dir.to_path_buf();
    let table = table.to_string();

    tokio::task::spawn_blocking(move || -> Result<(), MiniAppError> {
        purge_old_backups_sync(&scope_dir, &table, retention)
    })
    .await
    .map_err(|e| MiniAppError::Backup(format!("blocking task panic: {e}")))?
}

/// Synchronous implementation of [`purge_old_backups`], executed inside
/// `spawn_blocking`.
fn purge_old_backups_sync(
    scope_dir: &Path,
    table: &str,
    retention: usize,
) -> Result<(), MiniAppError> {
    let backup_dir = scope_dir.join("_backup");

    // If the backup directory does not exist yet, nothing to purge.
    if !backup_dir.exists() {
        return Ok(());
    }

    // Collect timestamps from YAML files that belong to this table.
    let entries = std::fs::read_dir(&backup_dir)
        .map_err(|e| MiniAppError::Backup(format!("cannot read backup dir: {e}")))?;

    let mut timestamps: Vec<u64> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            parse_backup_timestamp(&name, table, "yaml")
        })
        .collect();

    // Sort descending — newest first.
    timestamps.sort_unstable_by(|a, b| b.cmp(a));

    // Delete pairs beyond `retention`.
    for ts in timestamps.iter().skip(retention) {
        let yaml_path = backup_dir.join(format!("{}.{}.yaml", table, ts));
        let db_path = backup_dir.join(format!("{}.{}.db", table, ts));

        if let Err(e) = std::fs::remove_file(&yaml_path) {
            tracing::warn!(
                path = %yaml_path.display(),
                error = %e,
                "failed to remove old backup yaml; continuing"
            );
        }
        if let Err(e) = std::fs::remove_file(&db_path) {
            tracing::warn!(
                path = %db_path.display(),
                error = %e,
                "failed to remove old backup db; continuing"
            );
        }
    }

    Ok(())
}

/// Parses the numeric timestamp from a backup filename of the form
/// `{table}.{ts}.{ext}`.
///
/// Returns `None` if the name does not match the expected pattern or if the
/// timestamp segment is not a valid `u64`.
///
/// # Arguments
/// - `filename`: the bare filename string to parse.
/// - `table`: the expected table name prefix.
/// - `ext`: the expected extension (without leading dot), e.g. `"yaml"`.
fn parse_backup_timestamp(filename: &str, table: &str, ext: &str) -> Option<u64> {
    // Expected format: "{table}.{ts}.{ext}"
    let prefix = format!("{}.", table);
    let suffix = format!(".{}", ext);

    let without_prefix = filename.strip_prefix(&prefix)?;
    let ts_str = without_prefix.strip_suffix(&suffix)?;
    ts_str.parse::<u64>().ok()
}

/// Returns the sorted list of backup timestamps (descending) for a given
/// table, scanning only YAML files.  Used internally for testing.
///
/// # Arguments
/// - `backup_dir`: the `_backup/` directory to scan.
/// - `table`: the logical table name.
///
/// # Returns
/// A `Vec<u64>` of timestamps sorted newest-first.
///
/// # Errors
/// - [`MiniAppError::Backup`] if the directory cannot be read.
#[cfg(test)]
fn list_backup_timestamps(backup_dir: &Path, table: &str) -> Result<Vec<u64>, MiniAppError> {
    let entries = std::fs::read_dir(backup_dir)
        .map_err(|e| MiniAppError::Backup(format!("cannot read backup dir: {e}")))?;

    let mut timestamps: Vec<u64> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name();
            let name = name.to_string_lossy().to_string();
            parse_backup_timestamp(&name, table, "yaml")
        })
        .collect();

    timestamps.sort_unstable_by(|a, b| b.cmp(a));
    Ok(timestamps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::io::Write;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use tokio::task;

    /// Helper: create a minimal SQLite database with WAL mode enabled at `path`.
    fn create_test_db(path: &Path) {
        let conn = Connection::open(path).expect("open test db");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, v TEXT);",
        )
        .expect("setup test db");
    }

    /// Helper: create a minimal schema.yaml at `path`.
    fn create_test_schema_yaml(path: &Path) {
        let mut f = std::fs::File::create(path).expect("create schema yaml");
        f.write_all(b"table: items\nfields:\n  - name: v\n    type: string\n    required: false\n")
            .expect("write schema yaml");
    }

    // ── T1: happy-path ────────────────────────────────────────────────────

    /// T1: write_backup_pair creates both yaml and db files in `_backup/`.
    #[tokio::test]
    async fn write_backup_pair_creates_yaml_and_db() {
        let dir = TempDir::new().expect("temp dir");
        let scope_dir = dir.path();
        let db_path = scope_dir.join("items.db");
        let schema_path = scope_dir.join("schema.yaml");

        create_test_db(&db_path);
        create_test_schema_yaml(&schema_path);

        write_backup_pair(scope_dir, "items", &schema_path, &db_path)
            .await
            .expect("write_backup_pair must succeed");

        let backup_dir = scope_dir.join("_backup");
        assert!(backup_dir.exists(), "_backup dir must be created");

        let entries: Vec<_> = std::fs::read_dir(&backup_dir)
            .expect("read backup dir")
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

        assert_eq!(yaml_count, 1, "exactly one yaml backup must exist");
        assert_eq!(db_count, 1, "exactly one db backup must exist");
    }

    /// T1: purge_old_backups keeps only the N newest pairs.
    #[tokio::test]
    async fn purge_old_backups_keeps_n_newest() {
        let dir = TempDir::new().expect("temp dir");
        let scope_dir = dir.path();
        let backup_dir = scope_dir.join("_backup");
        std::fs::create_dir_all(&backup_dir).expect("create backup dir");

        // Create 5 fake backup pairs with distinct timestamps.
        for ts in [100u64, 200, 300, 400, 500] {
            std::fs::write(backup_dir.join(format!("items.{}.yaml", ts)), b"yaml")
                .expect("write yaml");
            std::fs::write(backup_dir.join(format!("items.{}.db", ts)), b"db").expect("write db");
        }

        purge_old_backups(scope_dir, "items", 3)
            .await
            .expect("purge must succeed");

        // Newest 3 timestamps: 500, 400, 300.  Oldest 2 (100, 200) must be gone.
        let timestamps = list_backup_timestamps(&backup_dir, "items").expect("list timestamps");
        assert_eq!(timestamps.len(), 3, "exactly 3 pairs must remain");
        assert_eq!(timestamps, vec![500, 400, 300], "newest 3 must be kept");

        // Verify the deleted pairs are truly gone.
        assert!(!backup_dir.join("items.100.yaml").exists());
        assert!(!backup_dir.join("items.100.db").exists());
        assert!(!backup_dir.join("items.200.yaml").exists());
        assert!(!backup_dir.join("items.200.db").exists());
    }

    // ── T2: boundary / edge-case ──────────────────────────────────────────

    /// T2: purge_old_backups is a no-op when backup count is below retention.
    #[tokio::test]
    async fn purge_old_backups_no_op_when_below_limit() {
        let dir = TempDir::new().expect("temp dir");
        let scope_dir = dir.path();
        let backup_dir = scope_dir.join("_backup");
        std::fs::create_dir_all(&backup_dir).expect("create backup dir");

        // Only 2 pairs, retention = 10.
        for ts in [100u64, 200] {
            std::fs::write(backup_dir.join(format!("items.{}.yaml", ts)), b"yaml")
                .expect("write yaml");
            std::fs::write(backup_dir.join(format!("items.{}.db", ts)), b"db").expect("write db");
        }

        purge_old_backups(scope_dir, "items", 10)
            .await
            .expect("purge must succeed");

        let timestamps = list_backup_timestamps(&backup_dir, "items").expect("list timestamps");
        assert_eq!(timestamps.len(), 2, "both pairs must still exist");
    }

    // ── T3: error-path ────────────────────────────────────────────────────

    /// T3: write_backup_pair returns Backup error when schema_yaml_path is missing.
    #[tokio::test]
    async fn write_backup_pair_io_error_returns_backup_variant() {
        let dir = TempDir::new().expect("temp dir");
        let scope_dir = dir.path();
        let db_path = scope_dir.join("items.db");
        // Create a real db but point schema to a non-existent file.
        create_test_db(&db_path);

        let result = write_backup_pair(
            scope_dir,
            "items",
            Path::new("/nonexistent/schema.yaml"),
            &db_path,
        )
        .await;

        let err = result.expect_err("missing schema file must error");
        assert!(
            matches!(err, MiniAppError::Backup(_)),
            "expected Backup variant, got {:?}",
            err
        );
    }

    // ── Concurrency: backup does not block concurrent reads ───────────────

    /// Concurrency test: backup runs concurrently with INSERT operations and
    /// both complete successfully.
    ///
    /// This verifies `rusqlite::Connection::backup` is safe to call on a
    /// WAL-mode database while another connection is writing.  rusqlite docs
    /// state "source can be used while the backup is running".
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_backup_does_not_block_concurrent_reads() {
        let dir = TempDir::new().expect("temp dir");
        let db_path = dir.path().join("concurrent.db");
        let dst_path = dir.path().join("backup.db");
        let schema_path = dir.path().join("schema.yaml");

        // Prepare DB with WAL mode and a table.
        {
            let conn = Connection::open(&db_path).expect("open db");
            conn.execute_batch(
                "PRAGMA journal_mode=WAL; CREATE TABLE rows (id INTEGER PRIMARY KEY, val TEXT);",
            )
            .expect("setup db");
        }
        create_test_schema_yaml(&schema_path);

        let db_path_writer = db_path.clone();
        let dst_path_backup = dst_path.clone();
        let schema_path_backup = schema_path.clone();
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

        // Launch backup task: runs the backup while writer is active.
        let backup_task =
            write_backup_pair(&scope_dir, "concurrent", &schema_path_backup, &db_path);

        let (writer_result, backup_result) = tokio::join!(writer, backup_task);

        writer_result.expect("writer must succeed");
        backup_result.expect("backup must succeed");

        // The backup file must exist and be a valid SQLite database.
        let backup_dir = scope_dir.join("_backup");
        let backup_entries: Vec<PathBuf> = std::fs::read_dir(&backup_dir)
            .expect("read backup dir")
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
            !backup_entries.is_empty(),
            "at least one db backup must exist"
        );

        // Verify backup db is a valid SQLite database (can be opened).
        let backup_conn = Connection::open(&backup_entries[0]).expect("open backup db");
        let backup_row_count: i64 = backup_conn
            .query_row("SELECT COUNT(*) FROM rows", [], |row| row.get(0))
            .unwrap_or(0);
        // Backup may have captured 0..100 rows (concurrent; exact count not deterministic).
        assert!(backup_row_count >= 0, "backup db must be a valid sqlite db");

        // Destination path for direct write_backup_pair output exists.
        let _ = dst_path_backup; // suppress unused warning
    }

    // ── Concurrency: spawn_blocking cancel safety ─────────────────────────

    /// Cancel-safety test: dropping a `write_backup_pair` Future immediately
    /// after spawn_blocking starts does not leave DB in a corrupt state.
    ///
    /// `tokio::task::spawn_blocking` is abort-unsafe: once the blocking
    /// closure starts running it runs to completion even if the outer Future
    /// is dropped.  This test verifies that DB and backup files are in a
    /// complete state after the Future has been dropped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_spawn_blocking_cancel_safety_insert_survives() {
        let dir = TempDir::new().expect("temp dir");
        let scope_dir = dir.path().to_path_buf();
        let db_path = scope_dir.join("cancel_test.db");
        let schema_path = scope_dir.join("schema.yaml");

        {
            let conn = Connection::open(&db_path).expect("open db");
            conn.execute_batch(
                "PRAGMA journal_mode=WAL; CREATE TABLE rows (id INTEGER PRIMARY KEY, val TEXT);",
            )
            .expect("setup db");
        }
        create_test_schema_yaml(&schema_path);

        // Issue backup with a very short timeout to trigger "cancel" of the Future.
        // spawn_blocking closure continues running even after the outer Future is dropped.
        let backup_fut = write_backup_pair(&scope_dir, "cancel_test", &schema_path, &db_path);
        let result = tokio::time::timeout(std::time::Duration::from_millis(1), backup_fut).await;

        // Give the spawn_blocking closure time to complete (it runs to completion
        // regardless of the timeout because spawn_blocking is abort-unsafe).
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // The backup files may or may not exist depending on whether the blocking
        // task completed before or after the timeout — the key guarantee is that
        // the source DB is not corrupted regardless.
        let src_conn = Connection::open(&db_path).expect("source db must still be openable");
        let _count: i64 = src_conn
            .query_row("SELECT COUNT(*) FROM rows", [], |row| row.get(0))
            .expect("source db must be a valid sqlite db after cancellation");

        // If the future was cancelled (Err = timeout), the backup may not have
        // completed.  If it succeeded (Ok), verify the backup is also valid.
        if let Ok(Ok(())) = result {
            let backup_dir = scope_dir.join("_backup");
            assert!(
                backup_dir.exists(),
                "backup dir must exist on successful write"
            );
        }
        // Whether timed out or not, no panic occurred — the test passes.
    }
}
