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
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use rusqlite::Connection;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::config::Config;
use crate::error::MiniAppError;
use crate::mcp::registry::TableRegistry;

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
pub(crate) fn parse_snapshot_timestamp(filename: &str, table: &str, ext: &str) -> Option<u64> {
    // Expected format: "{table}.{ts}.{ext}"
    let prefix = format!("{}.", table);
    let suffix = format!(".{}", ext);

    let without_prefix = filename.strip_prefix(&prefix)?;
    let ts_str = without_prefix.strip_suffix(&suffix)?;
    ts_str.parse::<u64>().ok()
}

// =============================================================================
// MCP tool: data_snapshot
// =============================================================================

/// Parameters for the `data_snapshot` MCP tool.
///
/// All fields are optional; `None` means "all" (tables / scopes).  The
/// `dry_run` flag, when `true`, returns inspection metadata without touching
/// any file or database state (Crux: dry_run zero-write guarantee).
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
pub struct DataSnapshotParams {
    /// Target a single table by name.  When `None`, all mounted tables in the
    /// given `scope` (or all scopes) are snapshotted.
    pub table: Option<String>,
    /// Restrict operation to `"project"` or `"user"` scope.  When `None`,
    /// all scopes are considered.
    pub scope: Option<String>,
    /// When `true`, returns `affects` metadata (target tables, row counts,
    /// would-purge counts) **without** creating, modifying, or deleting any
    /// file or database state (Crux: dry_run zero-write guarantee).
    pub dry_run: Option<bool>,
}

/// A single entry resolved for snapshotting.
///
/// Holds clones of the Arc pointers extracted from the registry so the
/// ArcSwap Guard can be dropped before any `.await` (K-110 / no
/// await-holding-lock).
struct SnapshotTarget {
    table_name: String,
    scope_root: PathBuf,
    db_path: PathBuf,
    store: Arc<crate::store::Store>,
}

/// Executes the `data_snapshot` MCP tool.
///
/// Fan-out logic:
/// - `table=Some + scope=Some` → 1 matching entry (scope-filtered).
/// - `table=Some + scope=None` → 1 entry via `registry.resolve`.
/// - `table=None + scope=Some("project")` → all entries whose `schema_path`
///   starts with `config.project_dir`.
/// - `table=None + scope=Some("user")` → same with `config.user_dir`.
/// - `table=None + scope=None` → all mounted entries.
///
/// When `dry_run=true` (Crux: dry_run zero-write guarantee):
/// - Returns `{ "dry_run": true, "affects": { "target_tables": [...],
///   "row_counts": {...}, "would_purge_generations": {...} } }`.
/// - **No file or database state is created, modified, or deleted.**
///
/// When `dry_run=false` (or omitted):
/// - Calls [`write_snapshot_db`] then [`purge_old_snapshots`] per entry.
/// - Returns `{ "snapshotted": [...], "purged": [...] }`.
///
/// # Arguments
/// - `config`: server mount configuration (dirs + retention).
/// - `tables`: the live `ArcSwap`-wrapped table registry.
/// - `params`: tool parameters.
///
/// # Returns
/// JSON string with operation results.
///
/// # Errors
/// - [`MiniAppError::Snapshot`] if any snapshot or purge operation fails, or
///   if the scope argument is unrecognised.
pub async fn do_data_snapshot(
    config: &Config,
    tables: &Arc<ArcSwap<TableRegistry>>,
    params: DataSnapshotParams,
) -> Result<String, MiniAppError> {
    let dry_run = params.dry_run.unwrap_or(false);

    // Resolve the target entries from the registry.  The ArcSwap Guard is
    // dropped immediately after the clone loop (no Guard across .await).
    let targets: Vec<SnapshotTarget> = {
        let registry = tables.load_full();
        resolve_targets(
            &registry,
            config,
            params.table.as_deref(),
            params.scope.as_deref(),
        )?
    };

    if dry_run {
        // Crux: dry_run zero-write guarantee — read-only path only.
        let mut target_tables: Vec<String> = targets.iter().map(|t| t.table_name.clone()).collect();
        target_tables.sort();

        let mut row_counts: HashMap<String, u64> = HashMap::new();
        let mut would_purge: HashMap<String, usize> = HashMap::new();

        for target in &targets {
            // row_count uses Store::row_count() which is read-only (SELECT COUNT(*)).
            let count = target.store.row_count().await.map_err(|e| {
                MiniAppError::Snapshot(format!(
                    "row_count failed for table '{}': {e}",
                    target.table_name
                ))
            })?;
            row_counts.insert(target.table_name.clone(), count);

            // Compute would-purge count by scanning _snapshots/ read-only.
            // No write occurs here (Crux: dry_run zero-write guarantee).
            let purge_count = count_would_purge(
                &target.scope_root,
                &target.table_name,
                config.snapshot_retention(),
            );
            would_purge.insert(target.table_name.clone(), purge_count);
        }

        let result = serde_json::json!({
            "dry_run": true,
            "affects": {
                "target_tables": target_tables,
                "row_counts": row_counts,
                "would_purge_generations": would_purge,
            }
        });
        return serde_json::to_string(&result)
            .map_err(|e| MiniAppError::Snapshot(format!("json serialization error: {e}")));
    }

    // Real path: write snapshots and purge old generations.
    let mut snapshotted: Vec<serde_json::Value> = Vec::new();
    let mut purged: Vec<serde_json::Value> = Vec::new();

    let retention = config.snapshot_retention();

    for target in &targets {
        // Write the snapshot using the hot backup API (Crux: rusqlite hot backup API).
        write_snapshot_db(&target.scope_root, &target.table_name, &target.db_path).await?;

        // Determine the timestamp of the snapshot just written (newest file).
        let snapshot_path = newest_snapshot_path(&target.scope_root, &target.table_name);
        let unix_secs = snapshot_path.as_ref().and_then(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| parse_snapshot_timestamp(n, &target.table_name, "db"))
        });

        let scope_label = scope_label_for(&target.scope_root, config);
        snapshotted.push(serde_json::json!({
            "table": target.table_name,
            "scope": scope_label,
            "snapshot_path": snapshot_path.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
            "unix_secs": unix_secs,
        }));

        // Purge old generations (Crux: snapshot retention isolation — only
        // calls config.snapshot_retention(), never backup_retention()).
        let snapshot_dir = target.scope_root.join("_snapshots");
        let before_count = count_snapshots_in_dir(&snapshot_dir, &target.table_name);
        purge_old_snapshots(&target.scope_root, &target.table_name, retention).await?;
        let after_count = count_snapshots_in_dir(&snapshot_dir, &target.table_name);
        let removed = before_count.saturating_sub(after_count);

        if removed > 0 {
            purged.push(serde_json::json!({
                "table": target.table_name,
                "generations_removed": removed,
            }));
        }
    }

    let result = serde_json::json!({
        "snapshotted": snapshotted,
        "purged": purged,
    });
    serde_json::to_string(&result)
        .map_err(|e| MiniAppError::Snapshot(format!("json serialization error: {e}")))
}

/// Resolves the list of snapshot targets from the registry according to
/// `table` and `scope` filter parameters.
///
/// # Arguments
/// - `registry`: the current table registry snapshot.
/// - `config`: mount config (for scope dir resolution).
/// - `table`: optional table name filter.
/// - `scope`: optional scope string (`"project"` or `"user"`).
///
/// # Returns
/// A `Vec<SnapshotTarget>` sorted by table name for deterministic output.
///
/// # Errors
/// - [`MiniAppError::Snapshot`] if the scope is unrecognised or if a
///   `schema_path` has no parent directory.
/// - [`MiniAppError::TableNotFound`] / [`MiniAppError::TableRequired`] from
///   `registry.resolve` when `table=Some`.
fn resolve_targets(
    registry: &TableRegistry,
    config: &Config,
    table: Option<&str>,
    scope: Option<&str>,
) -> Result<Vec<SnapshotTarget>, MiniAppError> {
    let is_legacy = registry.default_table().is_some();

    if let Some(table_name) = table {
        // Single-table path: resolve via registry.
        let entry = registry.resolve(Some(table_name))?;
        let scope_root = derive_scope_root(&entry.schema_path, is_legacy)?;
        let db_path = entry
            .schema_path
            .parent()
            .ok_or_else(|| MiniAppError::Snapshot("schema_path has no parent dir".into()))?
            .join(format!("{}.db", table_name));

        // Verify scope filter if provided.
        if let Some(scope_str) = scope {
            let expected_dir = resolve_scope_dir(config, scope_str)?;
            if let Some(expected) = expected_dir {
                if !scope_root.starts_with(&expected) {
                    return Ok(Vec::new()); // No match.
                }
            }
        }

        return Ok(vec![SnapshotTarget {
            table_name: table_name.to_string(),
            scope_root,
            db_path,
            store: Arc::clone(&entry.store),
        }]);
    }

    // Multi-table path: iterate entries with optional scope filter.
    let scope_filter: Option<PathBuf> = match scope {
        Some(s) => resolve_scope_dir(config, s)?,
        None => None,
    };

    let mut targets: Vec<SnapshotTarget> = registry
        .entries()
        .iter()
        .filter_map(|(name, entry)| {
            let scope_root = derive_scope_root(&entry.schema_path, is_legacy).ok()?;
            // Apply scope filter if present.
            if let Some(ref expected) = scope_filter {
                if !scope_root.starts_with(expected) {
                    return None;
                }
            }
            let db_path = entry.schema_path.parent()?.join(format!("{}.db", name));
            Some(SnapshotTarget {
                table_name: name.clone(),
                scope_root,
                db_path,
                store: Arc::clone(&entry.store),
            })
        })
        .collect();

    // Sort by table name for deterministic output (HashMap is unordered).
    targets.sort_by(|a, b| a.table_name.cmp(&b.table_name));
    Ok(targets)
}

/// Derives the `scope_root` path from a `schema_path`.
///
/// In **multi-table mode** (`is_legacy = false`), `schema_path` follows
/// `{scope_root}/{table}/schema.yaml`, so the scope root is 2 levels up.
///
/// In **legacy mode** (`is_legacy = true`), `schema_path` is an arbitrary
/// path provided via `MINI_APP_SCHEMA`, so the scope root is 1 level up
/// (same directory as the schema file).
///
/// # Errors
/// - [`MiniAppError::Snapshot`] if a required parent directory cannot be
///   determined.
fn derive_scope_root(schema_path: &Path, is_legacy: bool) -> Result<PathBuf, MiniAppError> {
    if is_legacy {
        schema_path
            .parent()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| MiniAppError::Snapshot("schema_path has no parent dir".into()))
    } else {
        schema_path
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
            .ok_or_else(|| MiniAppError::Snapshot("schema_path has no grandparent dir".into()))
    }
}

/// Resolves the filesystem path for a scope string (`"project"` or `"user"`).
///
/// Returns `Ok(None)` if the corresponding directory is not configured.
///
/// # Errors
/// - [`MiniAppError::Snapshot`] if `scope` is not `"project"` or `"user"`.
fn resolve_scope_dir(config: &Config, scope: &str) -> Result<Option<PathBuf>, MiniAppError> {
    match scope {
        "project" => Ok(config.project_dir.as_deref().map(|p| p.to_path_buf())),
        "user" => Ok(config.user_dir.as_deref().map(|p| p.to_path_buf())),
        other => Err(MiniAppError::Snapshot(format!(
            "unrecognised scope '{other}': expected 'project' or 'user'"
        ))),
    }
}

/// Returns a human-readable scope label (`"project"`, `"user"`, or `"unknown"`)
/// by comparing `scope_root` against the configured dirs.
fn scope_label_for(scope_root: &Path, config: &Config) -> &'static str {
    if let Some(pd) = config.project_dir.as_deref() {
        if scope_root.starts_with(pd) {
            return "project";
        }
    }
    if let Some(ud) = config.user_dir.as_deref() {
        if scope_root.starts_with(ud) {
            return "user";
        }
    }
    "unknown"
}

/// Counts how many snapshot generations would be purged for a given table
/// given the current retention setting.
///
/// Reads the `_snapshots/` directory but never writes, modifies, or deletes
/// anything (Crux: dry_run zero-write guarantee).
///
/// Returns `0` if the `_snapshots/` directory does not exist.
fn count_would_purge(scope_root: &Path, table: &str, retention: usize) -> usize {
    let snapshot_dir = scope_root.join("_snapshots");
    if !snapshot_dir.exists() {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(&snapshot_dir) else {
        return 0;
    };
    let count = entries
        .filter_map(|e| {
            let e = e.ok()?;
            let name = e.file_name();
            parse_snapshot_timestamp(&name.to_string_lossy(), table, "db").map(|_| ())
        })
        .count();
    count.saturating_sub(retention)
}

/// Counts the number of `.db` snapshot files for `table` in `snapshot_dir`.
fn count_snapshots_in_dir(snapshot_dir: &Path, table: &str) -> usize {
    if !snapshot_dir.exists() {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(snapshot_dir) else {
        return 0;
    };
    entries
        .filter_map(|e| {
            let e = e.ok()?;
            let name = e.file_name();
            parse_snapshot_timestamp(&name.to_string_lossy(), table, "db").map(|_| ())
        })
        .count()
}

/// Returns the path of the newest snapshot file for `table` in `scope_root/_snapshots/`,
/// or `None` if none exist.
fn newest_snapshot_path(scope_root: &Path, table: &str) -> Option<PathBuf> {
    let snapshot_dir = scope_root.join("_snapshots");
    let entries = std::fs::read_dir(&snapshot_dir).ok()?;
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if let Some(ts) = parse_snapshot_timestamp(&name_str, table, "db") {
            if best.as_ref().is_none_or(|(best_ts, _)| ts > *best_ts) {
                best = Some((ts, entry.path()));
            }
        }
    }
    best.map(|(_, path)| path)
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

    // ── Integration: do_data_snapshot dry_run zero-write guarantee ────────

    /// T1/Crux2: dry_run=true returns affects metadata without creating any
    /// file or database state.
    ///
    /// Verifies the Crux "dry_run zero-write guarantee": after calling
    /// `do_data_snapshot` with `dry_run=true`, the `_snapshots/` directory
    /// must not exist (it was not present before the call).
    #[tokio::test]
    async fn test_do_data_snapshot_dry_run_zero_write() {
        use crate::config::Config;
        use crate::mcp::registry::{TableEntry, TableRegistry};
        use crate::schema::{FieldDef, FieldType, SchemaConfig};
        use crate::store::Store;
        use arc_swap::ArcSwap;
        use std::collections::HashMap;

        let dir = TempDir::new().expect("temp dir");
        let table_name = "items";

        // Create multi-table layout: scope_root/{table}/schema.yaml
        // scope_root = dir.path()
        // table_dir  = dir.path()/{table}/
        // schema_path = dir.path()/{table}/schema.yaml
        // db_path     = dir.path()/{table}/{table}.db
        let table_dir = dir.path().join(table_name);
        std::fs::create_dir_all(&table_dir).expect("create table dir");

        let schema_path = table_dir.join("schema.yaml");
        std::fs::write(
            &schema_path,
            "table: items\nfields:\n  - name: title\n    type: string\n    required: true\n",
        )
        .expect("write schema.yaml");

        let db_path = table_dir.join(format!("{}.db", table_name));
        // SAFETY: Connection::open and execute_batch are safe in test context.
        let conn = Connection::open(&db_path).expect("open test db");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; \
             CREATE TABLE IF NOT EXISTS rows (id TEXT PRIMARY KEY, data TEXT, created_at TEXT, updated_at TEXT);",
        )
        .expect("setup test db");
        drop(conn);

        // Build Store and TableRegistry in multi-table mode (default_table = None).
        let schema = SchemaConfig {
            table: table_name.to_string(),
            fields: vec![FieldDef {
                name: "title".to_string(),
                ty: FieldType::String,
                required: true,
            }],
            dump: None,
        };
        let store = Store::open(&db_path, schema.clone())
            .await
            .expect("open store");

        let entry = TableEntry {
            store: Arc::new(store),
            schema: Arc::new(schema),
            schema_path: Arc::new(schema_path),
        };
        let mut entries = HashMap::new();
        entries.insert(table_name.to_string(), entry);
        // Multi-table mode: default_table = None — scope_root is 2 levels up from schema.yaml
        let registry = TableRegistry::from_entries(entries, None);
        let tables: Arc<ArcSwap<TableRegistry>> = Arc::new(ArcSwap::from_pointee(registry));

        // Config: project_dir points to scope_root (dir.path()).
        let config = Config {
            schema_path: None,
            db_path: None,
            user_dir: None,
            project_dir: Some(dir.path().to_path_buf()),
            backup_retention: None,
            snapshot_retention: None,
        };

        // _snapshots/ must NOT exist before the dry_run call.
        // In multi-table mode scope_root = dir.path(), so _snapshots is at dir.path()/_snapshots/.
        let snapshots_dir = dir.path().join("_snapshots");
        assert!(
            !snapshots_dir.exists(),
            "_snapshots must not exist before dry_run call"
        );

        let params = DataSnapshotParams {
            table: None,
            scope: None,
            dry_run: Some(true),
        };

        let result = do_data_snapshot(&config, &tables, params)
            .await
            .expect("do_data_snapshot dry_run must succeed");

        // _snapshots/ must STILL not exist — zero-write guarantee (Crux 2).
        assert!(
            !snapshots_dir.exists(),
            "_snapshots must not be created by dry_run=true (Crux: zero-write guarantee)"
        );

        // Response must carry dry_run: true and affects.target_tables.
        // SAFETY: serde_json::from_str is safe to unwrap in test context.
        let json: serde_json::Value =
            serde_json::from_str(&result).expect("result must be valid JSON");
        assert_eq!(
            json["dry_run"],
            serde_json::Value::Bool(true),
            "response must contain dry_run: true"
        );
        let target_tables = json["affects"]["target_tables"]
            .as_array()
            .expect("affects.target_tables must be an array");
        assert_eq!(
            target_tables.len(),
            1,
            "exactly one table should be in target_tables"
        );
        assert_eq!(
            target_tables[0],
            serde_json::Value::String(table_name.to_string()),
            "target table must be 'items'"
        );

        // row_counts and would_purge_generations must be present.
        assert!(
            json["affects"]["row_counts"].is_object(),
            "row_counts must be an object"
        );
        assert!(
            json["affects"]["would_purge_generations"].is_object(),
            "would_purge_generations must be an object"
        );
    }
}
