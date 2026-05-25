/// SQLite-backed row store for mini-app-mcp.
///
/// The [`Store`] type provides async CRUD operations over a single SQLite
/// table.  All field-level semantics (required fields, type coercion) are
/// delegated to [`crate::schema::SchemaConfig::validate`]; the store layer
/// is deliberately schema-agnostic at the DDL level.
///
/// # Crux #1 compliance
/// The `CREATE TABLE` DDL is a static string literal — no column is derived
/// from `schema.yaml` at the SQL level.  The `data` column stores a JSON
/// blob; all field validation happens in application code via
/// [`SchemaConfig::validate`].
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use rusqlite::{OptionalExtension, params_from_iter};

use crate::error::MiniAppError;
use crate::filter::ListFilter;
use crate::schema::SchemaConfig;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single stored row returned by CRUD operations.
///
/// The `data` field contains the raw JSON object that was supplied at
/// creation / update time.  `created_at` and `updated_at` are Unix epoch
/// seconds.
#[derive(Debug, Clone, Serialize)]
pub struct RowRecord {
    /// Unique row identifier (UUID v4 string).
    pub id: String,
    /// The validated JSON payload stored for this row.
    pub data: serde_json::Value,
    /// Unix epoch seconds at the time the row was created.
    pub created_at: i64,
    /// Unix epoch seconds at the time the row was last updated.
    pub updated_at: i64,
}

/// Update semantics for [`Store::update`].
///
/// - `Merge` (default): RFC 7396 shallow merge. Absent fields are preserved
///   from the stored row. A `null` patch value deletes the field when
///   `required = false`; it returns a [`MiniAppError::Validation`] error when
///   `required = true`. A full schema validation runs on the merged result
///   before persisting.
/// - `Replace`: Full replacement — identical to the pre-breaking-change default
///   behavior. The stored row is overwritten byte-for-byte with the supplied
///   `value` after schema validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum UpdateMode {
    /// RFC 7396 shallow merge (default).
    #[default]
    Merge,
    /// Full replacement (legacy behavior).
    Replace,
}

/// Async CRUD store backed by a single SQLite table.
///
/// The store wraps a `rusqlite::Connection` in an `Arc<Mutex<_>>` so it can
/// be cloned and shared across async tasks.  [`rusqlite::Connection`] is
/// `Send` but `!Sync`; the `Mutex` provides the required exclusive access.
///
/// All database operations execute inside `tokio::task::spawn_blocking` so
/// the tokio runtime thread-pool is never blocked.
pub struct Store {
    conn: Arc<Mutex<rusqlite::Connection>>,
    schema: SchemaConfig,
}

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

/// Fixed DDL.  Schema-yaml columns are **never** added here (Crux #1).
const CREATE_TABLE_SQL: &str = "
    CREATE TABLE IF NOT EXISTS rows (
        id          TEXT    PRIMARY KEY,
        data        TEXT    NOT NULL,
        created_at  INTEGER NOT NULL,
        updated_at  INTEGER NOT NULL
    )
";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns the current time as Unix epoch seconds.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Parse a JSON text column back into `serde_json::Value`.
fn parse_data(json_str: &str) -> Result<serde_json::Value, MiniAppError> {
    serde_json::from_str(json_str).map_err(|e| MiniAppError::Schema(format!("data column: {e}")))
}

/// RFC 7396 shallow merge: apply `patch` on top of `current`, consulting
/// `schema` for required-field null-deletion checks.
///
/// Rules:
/// - `patch` must be a JSON object; otherwise `Err(Validation { field: "(root)", .. })`.
/// - For each `(key, value)` in `patch`:
///   - If `value` is `null`: look up the field in `schema`.
///     - `required = true` → `Err(Validation { field: key, reason: "required field cannot be deleted via null" })`.
///     - Otherwise → remove the key from `current` (physical deletion from the Map).
///   - If `value` is non-null: overwrite `current[key]` with `value` (nested
///     objects are replaced wholesale — no deep merge).
/// - Fields not mentioned in `patch` are untouched in `current`.
/// - Returns the merged `serde_json::Value` (always an Object).
///
/// The caller is responsible for running `schema.validate(&merged)` after this
/// call to enforce post-merge type/required constraints.
fn shallow_merge(
    mut current: serde_json::Value,
    patch: serde_json::Value,
    schema: &SchemaConfig,
) -> Result<serde_json::Value, MiniAppError> {
    let patch_map = patch.as_object().ok_or_else(|| MiniAppError::Validation {
        field: "(root)".to_string(),
        reason: "patch must be a JSON object".to_string(),
    })?;

    let current_map = current
        .as_object_mut()
        .ok_or_else(|| MiniAppError::Validation {
            field: "(root)".to_string(),
            reason: "stored row is not a JSON object".to_string(),
        })?;

    for (key, value) in patch_map {
        if value.is_null() {
            // Null means "delete this field" per RFC 7396.
            let is_required = schema
                .fields
                .iter()
                .find(|f| &f.name == key)
                .map(|f| f.required)
                .unwrap_or(false);

            if is_required {
                return Err(MiniAppError::Validation {
                    field: key.clone(),
                    reason: "required field cannot be deleted via null".to_string(),
                });
            }
            current_map.remove(key);
        } else {
            current_map.insert(key.clone(), value.clone());
        }
    }

    Ok(current)
}

// ---------------------------------------------------------------------------
// Store impl
// ---------------------------------------------------------------------------

impl Store {
    /// Open the SQLite database at `db_path` and run `CREATE TABLE IF NOT EXISTS rows`.
    ///
    /// # WAL journal mode
    /// The connection is opened with `PRAGMA journal_mode = WAL` to enable safe
    /// coexistence of old and new [`Store`] instances during schema hot-reload
    /// (see `crux-card.md` Crux #1). WAL mode allows one writer and many readers
    /// concurrently, preventing lock conflicts when dual registries are held.
    /// Sidecar files `<db>.db-wal` and `<db>.db-shm` are created next to the
    /// main DB file; this is expected and safe.
    ///
    /// # Concurrency
    /// Returns a [`Store`] that wraps `Arc<Mutex<rusqlite::Connection>>` and is
    /// `Send + Sync`. [`rusqlite::Connection`] is `Send` but `!Sync`; the
    /// `std::sync::Mutex` provides exclusive access. All subsequent CRUD calls
    /// acquire the lock inside `spawn_blocking` closures and drop it before any
    /// `.await` point — holding a `MutexGuard` across `.await` is never permitted.
    ///
    /// If `schema.dump.sync` is `Some(SyncMode::Bidirectional)`, a
    /// `tracing::warn!` is emitted once here; the store behaves as write-only
    /// until bidirectional sync is implemented.
    ///
    /// # Cancel Safety
    /// Not cancel-safe. Once the `spawn_blocking` closure has started (DDL
    /// execution), calling `abort` on the `JoinHandle` or dropping the returned
    /// `Future` has no effect — the DDL completes on the blocking thread pool.
    ///
    /// # Errors
    /// - [`MiniAppError::Storage`] — `Connection::open`, WAL pragma, or DDL execute failure.
    /// - [`MiniAppError::Schema`] — blocking thread panicked (JoinError).
    ///
    /// # Panic
    /// Does not panic.
    pub async fn open(db_path: &Path, schema: SchemaConfig) -> Result<Self, MiniAppError> {
        // Warn if bidirectional sync is configured but not yet implemented.
        if let Some(crate::dump::SyncMode::Bidirectional) =
            schema.dump.as_ref().and_then(|d| d.sync.as_ref())
        {
            tracing::warn!(
                target: "mini_app_mcp::dump",
                "sync=bidirectional configured but not implemented yet; behaving as write-only"
            );
        }

        let db_path = db_path.to_path_buf();
        let conn =
            tokio::task::spawn_blocking(move || -> Result<rusqlite::Connection, MiniAppError> {
                let c = rusqlite::Connection::open(&db_path)?;
                // Enable WAL journal mode before DDL. WAL allows concurrent readers
                // and one writer, which is essential for Crux #1 dual-registry safety.
                c.pragma_update(None, "journal_mode", "WAL")?;
                // Read back the actual mode: SQLite silently falls back to non-WAL
                // on `:memory:`, NFS, or read-only filesystems.  A mismatch does not
                // prevent startup but means concurrent reload may hit SQLITE_BUSY.
                let actual_mode: String = c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
                if actual_mode.to_lowercase() != "wal" {
                    tracing::warn!(
                        actual_mode = %actual_mode,
                        "PRAGMA journal_mode=WAL fell back to non-WAL mode; \
                         concurrent reload may hit SQLITE_BUSY"
                    );
                }
                c.execute_batch(CREATE_TABLE_SQL)?;
                Ok(c)
            })
            .await
            .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")))??;

        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
            schema,
        })
    }

    /// Validate `value` against the schema and insert a new row with a
    /// generated UUID primary key.
    ///
    /// # Concurrency
    /// The rusqlite `INSERT` executes inside `tokio::task::spawn_blocking`.
    /// `Arc<Mutex<Connection>>` is cloned before entering the blocking closure;
    /// the [`std::sync::MutexGuard`] is acquired and dropped entirely within the
    /// blocking closure — never held across an `.await` point.
    ///
    /// After the `spawn_blocking` future resolves, `dump::on_change` is called
    /// at the `.await` point. The `MutexGuard` is already dropped at this stage.
    /// If `dump::on_change` fails (e.g. disk full), the error is propagated via
    /// `?` and the caller receives `Err(MiniAppError::Io(_))`; the row remains
    /// in the database (DB and file may be transiently inconsistent until the
    /// next successful write).
    ///
    /// # Cancel Safety
    /// Not cancel-safe. Once the `spawn_blocking` closure has started, the
    /// `INSERT` completes regardless of `Future` cancellation. If the caller
    /// drops this `Future` after the INSERT but before `dump::on_change`
    /// completes, the file may not be materialized while the DB row exists.
    ///
    /// # Errors
    /// - [`MiniAppError::Validation`] — required field absent or type mismatch.
    /// - [`MiniAppError::Storage`] — rusqlite error (constraint violation, I/O).
    /// - [`MiniAppError::Schema`] — blocking thread panicked (JoinError).
    /// - [`MiniAppError::Io`] — dump file write failure (only when `dump` is configured).
    ///
    /// # Panic
    /// Does not panic. Mutex poisoning is propagated as `Err(MiniAppError::Storage(_))`.
    pub async fn create(&self, value: serde_json::Value) -> Result<RowRecord, MiniAppError> {
        self.schema.validate(&value)?;

        let id = uuid::Uuid::new_v4().to_string();
        let now = now_secs();
        let data_str =
            serde_json::to_string(&value).expect("serde_json::Value serialization is infallible");

        let conn = self.conn.clone();
        let id_inner = id.clone();
        let record = tokio::task::spawn_blocking(move || -> Result<RowRecord, MiniAppError> {
            let conn = conn
                .lock()
                .map_err(|_| MiniAppError::Schema("mutex poisoned".to_string()))?;
            conn.execute(
                "INSERT INTO rows (id, data, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![id_inner, data_str, now, now],
            )?;
            Ok(RowRecord {
                id: id_inner,
                data: value,
                created_at: now,
                updated_at: now,
            })
        })
        .await
        .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")))??;

        // MutexGuard is already dropped (held only inside the spawn_blocking closure above).
        crate::dump::on_change(&self.schema, &record).await?;

        Ok(record)
    }

    /// Fetch the row with the given `id`.
    ///
    /// # Concurrency
    /// The `SELECT` executes inside `tokio::task::spawn_blocking`. The
    /// [`std::sync::MutexGuard`] is acquired and released within the blocking
    /// closure; no lock is held across `.await`.
    ///
    /// # Cancel Safety
    /// Once the blocking closure has started the `SELECT` will complete
    /// regardless of `Future` cancellation.
    ///
    /// # Errors
    /// - [`MiniAppError::NotFound`] — no row with the given `id`.
    /// - [`MiniAppError::Storage`] — rusqlite error.
    /// - [`MiniAppError::Schema`] — blocking thread panicked (JoinError).
    ///
    /// # Panic
    /// Does not panic.
    pub async fn get(&self, id: &str) -> Result<RowRecord, MiniAppError> {
        let conn = self.conn.clone();
        let id = id.to_string();

        tokio::task::spawn_blocking(move || -> Result<RowRecord, MiniAppError> {
            let conn = conn
                .lock()
                .map_err(|_| MiniAppError::Schema("mutex poisoned".to_string()))?;
            let mut stmt =
                conn.prepare("SELECT id, data, created_at, updated_at FROM rows WHERE id = ?1")?;
            let row = stmt
                .query_row(rusqlite::params![id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                })
                .optional()?
                .ok_or_else(|| MiniAppError::NotFound { id: id.clone() })?;

            let data = parse_data(&row.1)?;
            Ok(RowRecord {
                id: row.0,
                data,
                created_at: row.2,
                updated_at: row.3,
            })
        })
        .await
        .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")))?
    }

    /// Return rows ordered by `created_at DESC`.
    ///
    /// `limit` defaults to `100` (max `1000`). `offset` defaults to `0`.
    ///
    /// # Concurrency
    /// The `SELECT` executes inside `tokio::task::spawn_blocking`. The
    /// [`std::sync::MutexGuard`] is held only within the blocking closure.
    ///
    /// # Cancel Safety
    /// Once the blocking closure has started the query runs to completion
    /// regardless of `Future` cancellation.
    ///
    /// # Errors
    /// - [`MiniAppError::Storage`] — rusqlite error.
    /// - [`MiniAppError::Schema`] — blocking thread panicked (JoinError).
    /// - [`MiniAppError::Validation`] — `build_sql` on `filter` fails
    ///   (defensive; callers should call `filter.validate()` first).
    ///
    /// # Panic
    /// Does not panic.
    pub async fn list(
        &self,
        limit: Option<u32>,
        offset: Option<u32>,
        filter: Option<ListFilter>,
    ) -> Result<Vec<RowRecord>, MiniAppError> {
        let conn = self.conn.clone();
        let limit = limit.unwrap_or(100).min(1000) as i64;
        let offset = offset.unwrap_or(0) as i64;

        // Build WHERE clause + params from filter (before spawning the blocking task).
        let (where_clause, filter_params) = match filter {
            None => (String::new(), Vec::new()),
            Some(f) => {
                let (fragment, params) = f.build_sql()?;
                (format!(" WHERE {fragment}"), params)
            }
        };

        tokio::task::spawn_blocking(move || -> Result<Vec<RowRecord>, MiniAppError> {
            let conn = conn
                .lock()
                .map_err(|_| MiniAppError::Schema("mutex poisoned".to_string()))?;
            let sql = format!(
                "SELECT id, data, created_at, updated_at FROM rows{where_clause} \
                 ORDER BY created_at DESC LIMIT ? OFFSET ?"
            );
            // Combine filter params with LIMIT/OFFSET params in order.
            let mut all_params: Vec<Box<dyn rusqlite::ToSql>> = filter_params
                .into_iter()
                .map(|p| -> Box<dyn rusqlite::ToSql> { Box::new(p) })
                .collect();
            all_params.push(Box::new(limit));
            all_params.push(Box::new(offset));

            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(
                    params_from_iter(all_params.iter().map(|p| p.as_ref())),
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    },
                )?
                .map(|r| {
                    r.map_err(MiniAppError::Storage).and_then(|row| {
                        let data = parse_data(&row.1)?;
                        Ok(RowRecord {
                            id: row.0,
                            data,
                            created_at: row.2,
                            updated_at: row.3,
                        })
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")))?
    }

    /// Count all rows in the table.
    ///
    /// Used by `schema_delete` in `dry_run` mode to report how many rows
    /// would be orphaned when the schema is removed.
    ///
    /// # Returns
    /// The total row count as `u64`.
    ///
    /// # Errors
    /// - [`MiniAppError::Schema`] — if the mutex is poisoned or the blocking
    ///   task panics.
    /// - [`MiniAppError::Storage`] — if the SQL query fails.
    pub async fn row_count(&self) -> Result<u64, MiniAppError> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<u64, MiniAppError> {
            let conn = conn
                .lock()
                .map_err(|_| MiniAppError::Schema("mutex poisoned".to_string()))?;
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM rows", [], |row| row.get(0))?;
            Ok(count.max(0) as u64)
        })
        .await
        .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")))?
    }

    /// Validate `value` and update the row identified by `id`.
    /// `updated_at` is refreshed; `created_at` is unchanged.
    ///
    /// # Concurrency
    /// The `UPDATE` executes inside `tokio::task::spawn_blocking`. The
    /// [`std::sync::MutexGuard`] is held only within the blocking closure and is
    /// dropped before any `.await` point. Concurrent calls with the same `id`
    /// are serialized by the `Mutex`.
    ///
    /// After the `spawn_blocking` future resolves, `dump::on_change` is called
    /// at the `.await` point. The `MutexGuard` is already dropped at this stage.
    /// If `dump::on_change` fails (e.g. disk full), the error is propagated via
    /// `?` and the caller receives `Err(MiniAppError::Io(_))`; the row update
    /// remains in the database (DB and file may be transiently inconsistent
    /// until the next successful write).
    ///
    /// **Same-id concurrent update is not order-preserving with respect to
    /// file content.** The DB `UPDATE` is serialised by the connection
    /// `Mutex`, but `dump::on_change` runs *outside* the lock. Two concurrent
    /// `update(id, A)` / `update(id, B)` calls may finalise the DB row as B
    /// while the dump file ends up holding A's content (whichever
    /// `spawn_blocking` write completes last wins on disk). Callers that
    /// require strict file-DB ordering must serialise updates by `id` at the
    /// caller side.
    ///
    /// # Cancel Safety
    /// Not cancel-safe. Once the blocking closure has started the `UPDATE` will
    /// complete regardless of `Future` cancellation. Idempotent at the SQL
    /// level: calling with the same `id` and `value` results in the same final
    /// DB state. If the caller drops this `Future` after the UPDATE but before
    /// `dump::on_change` completes, the file may not be re-materialized while
    /// the DB row already reflects the new value.
    ///
    /// # Errors
    /// - [`MiniAppError::NotFound`] — no row with the given `id`.
    /// - [`MiniAppError::Validation`] — required field absent or type mismatch, or
    ///   a null patch value targets a required field (Merge mode only).
    /// - [`MiniAppError::Storage`] — rusqlite error.
    /// - [`MiniAppError::Schema`] — blocking thread panicked (JoinError).
    /// - [`MiniAppError::Io`] — dump file write failure (only when `dump` is configured).
    ///
    /// # Panic
    /// Does not panic.
    pub async fn update(
        &self,
        id: &str,
        value: serde_json::Value,
        mode: UpdateMode,
    ) -> Result<RowRecord, MiniAppError> {
        let now = now_secs();
        let conn = self.conn.clone();
        let id_str = id.to_string();
        let schema = self.schema.clone();

        let record = tokio::task::spawn_blocking(move || -> Result<RowRecord, MiniAppError> {
            let conn = conn
                .lock()
                .map_err(|_| MiniAppError::Schema("mutex poisoned".to_string()))?;

            // Fetch both data and created_at in one query.
            // For Replace mode, the data column is read but unused; this keeps
            // the SQL identical across modes and avoids a second lock acquisition.
            let row_data: Option<(String, i64)> = conn
                .query_row(
                    "SELECT data, created_at FROM rows WHERE id = ?1",
                    rusqlite::params![id_str],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()?;

            let (current_data_str, created_at) =
                row_data.ok_or_else(|| MiniAppError::NotFound { id: id_str.clone() })?;

            let merged = match mode {
                UpdateMode::Merge => {
                    let current: serde_json::Value = parse_data(&current_data_str)?;
                    let merged = shallow_merge(current, value, &schema)?;
                    // Post-merge full schema validation (Crux #1: must run after merge).
                    schema.validate(&merged)?;
                    merged
                }
                UpdateMode::Replace => {
                    // Replace: validate first, then store as-is (byte-for-byte identical
                    // to pre-breaking-change behavior — Crux #2).
                    schema.validate(&value)?;
                    value
                }
            };

            let merged_str = serde_json::to_string(&merged)
                .expect("serde_json::Value serialization is infallible");

            conn.execute(
                "UPDATE rows SET data = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![merged_str, now, id_str],
            )?;

            Ok(RowRecord {
                id: id_str,
                data: merged,
                created_at,
                updated_at: now,
            })
        })
        .await
        .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")))??;

        // MutexGuard is already dropped (held only inside the spawn_blocking closure above).
        crate::dump::on_change(&self.schema, &record).await?;

        Ok(record)
    }

    /// Execute a closure under a SQLite SAVEPOINT for all-or-nothing semantics.
    ///
    /// The closure receives `&mut rusqlite::Savepoint<'_>` and may run arbitrary
    /// SQL inside the SAVEPOINT.  On success, the SAVEPOINT is committed.  On
    /// failure, the SAVEPOINT is rolled back automatically when it is dropped
    /// (enforced via `set_drop_behavior(DropBehavior::Rollback)`).
    ///
    /// # Crux compliance
    /// This method is the implementation backing `schema_batch`'s
    /// `schema_batch SAVEPOINT atomicity` Crux constraint.  All ops inside a
    /// batch share the same SAVEPOINT; any failure causes the SAVEPOINT to
    /// roll back, leaving the DB unchanged.
    ///
    /// # Concurrency
    /// The Mutex is acquired and the entire SAVEPOINT + ops execute inside a
    /// single `tokio::task::spawn_blocking` closure.  `Savepoint<'_>` borrows
    /// the `Connection`, so both must remain in the same closure scope — they
    /// cannot straddle an `.await` point (K-103, K-110).
    ///
    /// # Cancel Safety
    /// Not cancel-safe.  Once the `spawn_blocking` closure has started, the
    /// SAVEPOINT runs to completion (commit or rollback) regardless of `Future`
    /// cancellation.
    ///
    /// # Type Parameters
    /// - `F`: closure `FnOnce(&mut rusqlite::Savepoint<'_>) -> Result<R, MiniAppError> + Send + 'static`.
    /// - `R`: return value, must be `Send + 'static`.
    ///
    /// # Errors
    /// - [`MiniAppError::Schema`] — Mutex poisoned or blocking thread panicked.
    /// - [`MiniAppError::Storage`] — rusqlite SAVEPOINT creation or commit failed.
    /// - Any error returned by the closure `f`.
    ///
    /// # Panic
    /// Does not panic.
    pub async fn execute_under_savepoint<F, R>(&self, f: F) -> Result<R, MiniAppError>
    where
        F: FnOnce(&mut rusqlite::Savepoint<'_>) -> Result<R, MiniAppError> + Send + 'static,
        R: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<R, MiniAppError> {
            let mut guard = conn
                .lock()
                .map_err(|_| MiniAppError::Schema("mutex poisoned".to_string()))?;
            let mut sp = guard.savepoint()?;
            // Ensure rollback on Drop so any early-return via `?` cleans up.
            sp.set_drop_behavior(rusqlite::DropBehavior::Rollback);
            let result = f(&mut sp)?;
            sp.commit()?;
            Ok(result)
        })
        .await
        .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")))?
    }

    /// Delete the row identified by `id`.
    ///
    /// # Concurrency
    /// The `DELETE` executes inside `tokio::task::spawn_blocking`. The
    /// [`std::sync::MutexGuard`] is held only within the blocking closure and
    /// is dropped before any `.await` point. Idempotent at the SQL level:
    /// deleting a non-existent `id` returns [`MiniAppError::NotFound`], so
    /// calling twice with the same `id` returns `Err(MiniAppError::NotFound)`
    /// on the second call.
    ///
    /// After the `spawn_blocking` future resolves, `dump::on_delete` is called
    /// at the `.await` point. The `MutexGuard` is already dropped at this stage.
    /// In the current implementation `on_delete` is a no-op (`Ok(())`) and the
    /// dump file is preserved on disk by default. The `Result<(), MiniAppError>`
    /// signature is retained because a future schema flag (e.g.
    /// `dump.on_delete: keep | remove`) may switch this to an actual file
    /// removal that can fail with [`MiniAppError::Io`]. Today the value-level
    /// behaviour is infallible, but the type-level contract (and the
    /// `?`-propagation site in `Store::delete`) is preserved so that flipping
    /// the future flag does not require changing the call site.
    ///
    /// # Cancel Safety
    /// Not cancel-safe with respect to the `spawn_blocking` portion: once the
    /// blocking closure has started the `DELETE` runs to completion regardless
    /// of `Future` cancellation. The current `on_delete` no-op is itself
    /// cancel-safe (no `.await`, no I/O), so dropping this `Future` after the
    /// DELETE has no observable file-system effect today. When `on_delete`
    /// gains real file removal in the future, this paragraph must be updated
    /// in lockstep with the new contract.
    ///
    /// # Errors
    /// - [`MiniAppError::NotFound`] — no row with the given `id`.
    /// - [`MiniAppError::Storage`] — rusqlite error.
    /// - [`MiniAppError::Schema`] — blocking thread panicked (JoinError).
    /// - [`MiniAppError::Io`] — reserved for a future `on_delete` implementation
    ///   that performs file removal (currently never returned, but the variant
    ///   is part of the public contract so the call site does not need to
    ///   change when the flag is added).
    ///
    /// # Panic
    /// Does not panic.
    pub async fn delete(&self, id: &str) -> Result<(), MiniAppError> {
        let conn = self.conn.clone();
        // Clone id for use after spawn_blocking (id is moved into the closure).
        let id_for_hook = id.to_string();
        let id = id.to_string();

        tokio::task::spawn_blocking(move || -> Result<(), MiniAppError> {
            let conn = conn
                .lock()
                .map_err(|_| MiniAppError::Schema("mutex poisoned".to_string()))?;
            let n = conn.execute("DELETE FROM rows WHERE id = ?1", rusqlite::params![id])?;
            if n == 0 {
                return Err(MiniAppError::NotFound { id });
            }
            Ok(())
        })
        .await
        .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")))??;

        // MutexGuard is already dropped (held only inside the spawn_blocking closure above).
        crate::dump::on_delete(&self.schema, &id_for_hook).await?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::schema::{FieldDef, FieldType};

    async fn make_test_store() -> Store {
        let schema = SchemaConfig {
            table: "test".into(),
            title: None,
            description: None,
            fields: vec![
                FieldDef {
                    name: "title".into(),
                    ty: FieldType::String,
                    required: true,
                    description: None,
                },
                FieldDef {
                    name: "state".into(),
                    ty: FieldType::String,
                    required: false,
                    description: None,
                },
            ],
            dump: None,
        };
        Store::open(Path::new(":memory:"), schema).await.unwrap()
    }

    /// Build a test store with dump directed to `dir`.
    async fn make_test_store_with_dump(dir: &Path) -> Store {
        use crate::dump::{DumpConfig, SyncMode};
        let schema = SchemaConfig {
            table: "test".into(),
            title: None,
            description: None,
            fields: vec![
                FieldDef {
                    name: "title".into(),
                    ty: FieldType::String,
                    required: true,
                    description: None,
                },
                FieldDef {
                    name: "body".into(),
                    ty: FieldType::String,
                    required: false,
                    description: None,
                },
            ],
            dump: Some(DumpConfig {
                dir: Some(dir.to_path_buf()),
                title_field: None,
                body_field: None,
                sync: Some(SyncMode::WriteOnly),
            }),
        };
        Store::open(Path::new(":memory:"), schema).await.unwrap()
    }

    // --- Basic CRUD ---

    #[tokio::test]
    async fn test_create_and_get_roundtrip() {
        let store = make_test_store().await;
        let value = serde_json::json!({"title": "hello", "state": "open"});
        let row = store.create(value.clone()).await.unwrap();
        let fetched = store.get(&row.id).await.unwrap();
        assert_eq!(fetched.id, row.id);
        assert_eq!(fetched.data, value);
    }

    #[tokio::test]
    async fn test_create_then_list() {
        let store = make_test_store().await;
        store
            .create(serde_json::json!({"title": "t1"}))
            .await
            .unwrap();
        let rows = store.list(None, None, None).await.unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn test_list_limit_offset() {
        let store = make_test_store().await;
        for i in 0..5 {
            store
                .create(serde_json::json!({"title": format!("item-{i}")}))
                .await
                .unwrap();
        }
        let page1 = store.list(Some(2), Some(0), None).await.unwrap();
        assert_eq!(page1.len(), 2);
        let page2 = store.list(Some(2), Some(2), None).await.unwrap();
        assert_eq!(page2.len(), 2);
        let page3 = store.list(Some(2), Some(4), None).await.unwrap();
        assert_eq!(page3.len(), 1);
    }

    #[tokio::test]
    async fn test_update_timestamps() {
        let store = make_test_store().await;
        let row = store
            .create(serde_json::json!({"title": "original"}))
            .await
            .unwrap();
        // Sleep a tiny bit so updated_at can differ from created_at.
        // (In practice both are epoch seconds, so same-second updates produce
        //  the same value — the test verifies created_at is preserved.)
        let updated = store
            .update(
                &row.id,
                serde_json::json!({"title": "changed"}),
                UpdateMode::Replace,
            )
            .await
            .unwrap();
        assert_eq!(updated.created_at, row.created_at);
        assert_eq!(updated.id, row.id);
        assert_eq!(updated.data["title"], "changed");
    }

    #[tokio::test]
    async fn test_create_delete_get_not_found() {
        let store = make_test_store().await;
        let row = store
            .create(serde_json::json!({"title": "to-delete"}))
            .await
            .unwrap();
        store.delete(&row.id).await.unwrap();
        let err = store.get(&row.id).await.unwrap_err();
        assert!(matches!(err, MiniAppError::NotFound { .. }));
    }

    #[tokio::test]
    async fn test_get_unknown_id_not_found() {
        let store = make_test_store().await;
        let err = store.get("nonexistent-id").await.unwrap_err();
        assert!(matches!(err, MiniAppError::NotFound { .. }));
    }

    #[tokio::test]
    async fn test_update_unknown_id_not_found() {
        let store = make_test_store().await;
        let err = store
            .update(
                "nonexistent-id",
                serde_json::json!({"title": "x"}),
                UpdateMode::Replace,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, MiniAppError::NotFound { .. }));
    }

    #[tokio::test]
    async fn test_delete_unknown_id_not_found() {
        let store = make_test_store().await;
        let err = store.delete("nonexistent-id").await.unwrap_err();
        assert!(matches!(err, MiniAppError::NotFound { .. }));
    }

    #[tokio::test]
    async fn test_create_missing_required_field_validation_error() {
        let store = make_test_store().await;
        // `title` is required but absent.
        let err = store
            .create(serde_json::json!({"state": "open"}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, MiniAppError::Validation { .. }),
            "expected Validation, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn test_create_type_mismatch_validation_error() {
        let store = make_test_store().await;
        // `title` must be a string; passing a number should fail.
        let err = store
            .create(serde_json::json!({"title": 42}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, MiniAppError::Validation { .. }),
            "expected Validation, got: {err:?}"
        );
    }

    // --- Concurrency tests (from concurrency-analysis.md §2) ---

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_store_create_concurrent() {
        let store = Arc::new(make_test_store().await);
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let s = store.clone();
                tokio::spawn(async move {
                    s.create(serde_json::json!({"title": format!("task-{i}"), "state": "open"}))
                        .await
                })
            })
            .collect();
        let results: Vec<_> = futures::future::join_all(handles).await;
        assert!(results.iter().all(|r| r.as_ref().unwrap().is_ok()));
        let rows = store.list(None, None, None).await.unwrap();
        assert_eq!(rows.len(), 4);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_store_mutex_no_await_holding_lock() {
        let store = Arc::new(make_test_store().await);
        let id = store
            .create(serde_json::json!({"title": "init", "state": "open"}))
            .await
            .unwrap()
            .id;
        let s1 = store.clone();
        let id1 = id.clone();
        let h1 = tokio::spawn(async move { s1.get(&id1).await });
        let s2 = store.clone();
        let id2 = id.clone();
        let h2 = tokio::spawn(async move {
            s2.update(
                &id2,
                serde_json::json!({"title": "updated", "state": "closed"}),
                UpdateMode::Replace,
            )
            .await
        });
        let (r1, r2) = tokio::join!(h1, h2);
        assert!(r1.unwrap().is_ok());
        assert!(r2.unwrap().is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn test_store_arc_clone_across_tasks() {
        let store = Arc::new(make_test_store().await);
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let s = Arc::clone(&store);
                tokio::spawn(async move {
                    s.create(serde_json::json!({"title": format!("row-{i}"), "state": "open"}))
                        .await
                })
            })
            .collect();
        futures::future::join_all(handles).await;
        assert_eq!(store.list(None, None, None).await.unwrap().len(), 8);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_spawn_blocking_join_error_propagation() {
        let result: Result<(), _> = tokio::task::spawn_blocking(|| panic!("intentional"))
            .await
            .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")));
        assert!(matches!(result, Err(MiniAppError::Schema(_))));
    }

    // --- Dump integration tests ---

    #[tokio::test]
    async fn create_triggers_dump_when_configured() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = make_test_store_with_dump(tmp.path()).await;
        let row = store
            .create(serde_json::json!({"title": "My Issue", "body": "Details"}))
            .await
            .expect("create ok");
        let dump_file = tmp.path().join(format!("{}.md", row.id));
        assert!(dump_file.exists(), "dump file must be created after create");
        let content = std::fs::read_to_string(&dump_file).expect("read dump file");
        assert!(content.starts_with("# My Issue\n"));
        assert!(content.contains("Details"));
    }

    #[tokio::test]
    async fn update_overwrites_dump_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = make_test_store_with_dump(tmp.path()).await;
        let row = store
            .create(serde_json::json!({"title": "Original", "body": "v1"}))
            .await
            .expect("create ok");

        store
            .update(
                &row.id,
                serde_json::json!({"title": "Updated", "body": "v2"}),
                UpdateMode::Replace,
            )
            .await
            .expect("update ok");

        let dump_file = tmp.path().join(format!("{}.md", row.id));
        let content = std::fs::read_to_string(&dump_file).expect("read dump file");
        assert!(
            content.starts_with("# Updated\n"),
            "dump file must reflect updated title"
        );
        assert!(
            content.contains("v2"),
            "dump file must reflect updated body"
        );
    }

    #[tokio::test]
    async fn delete_keeps_dump_file_by_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = make_test_store_with_dump(tmp.path()).await;
        let row = store
            .create(serde_json::json!({"title": "Keep Me", "body": ""}))
            .await
            .expect("create ok");

        let dump_file = tmp.path().join(format!("{}.md", row.id));
        assert!(dump_file.exists(), "dump file must exist after create");

        store.delete(&row.id).await.expect("delete ok");
        assert!(
            dump_file.exists(),
            "dump file must remain after delete (default: keep)"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_store_create_concurrent_dump_writes_all_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(make_test_store_with_dump(tmp.path()).await);

        let handles: Vec<_> = (0..4)
            .map(|i| {
                let s = store.clone();
                tokio::spawn(async move {
                    s.create(serde_json::json!({
                        "title": format!("concurrent-{i}"),
                        "body": format!("body-{i}"),
                    }))
                    .await
                })
            })
            .collect();

        let results: Vec<_> = futures::future::join_all(handles).await;
        // All creates must succeed
        let rows: Vec<_> = results
            .into_iter()
            .map(|r| r.expect("spawn ok").expect("create ok"))
            .collect();

        // Each row must have a corresponding dump file
        for row in &rows {
            let path = tmp.path().join(format!("{}.md", row.id));
            assert!(path.exists(), "dump file must exist for row {}", row.id);
        }
        assert_eq!(rows.len(), 4);
    }

    #[tokio::test]
    async fn store_open_with_bidirectional_sync_returns_ok() {
        use crate::dump::{DumpConfig, SyncMode};
        // Store::open must succeed and emit warn (we verify Ok return here;
        // warn log capture is out of scope per Acceptance Criteria §7).
        let schema = SchemaConfig {
            table: "test".into(),
            title: None,
            description: None,
            fields: vec![FieldDef {
                name: "title".into(),
                ty: FieldType::String,
                required: false,
                description: None,
            }],
            dump: Some(DumpConfig {
                dir: None,
                title_field: None,
                body_field: None,
                sync: Some(SyncMode::Bidirectional),
            }),
        };
        let store = Store::open(Path::new(":memory:"), schema).await;
        assert!(
            store.is_ok(),
            "Store::open must succeed even with bidirectional sync configured"
        );
    }

    // --- SAVEPOINT / concurrency tests (ST3 additions) ---

    /// Test that execute_under_savepoint rolls back all ops on failure.
    /// Crux must_not_simplify 1: single SAVEPOINT, all-or-nothing semantics.
    ///
    /// Sequence: INSERT via SAVEPOINT → force error inside SAVEPOINT →
    /// assert SAVEPOINT rolled back → DB row count = 0.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_savepoint_atomic_rollback_on_op_failure() {
        let store = make_test_store().await;

        // A closure that does one INSERT then returns an error.
        let result: Result<(), MiniAppError> = store
            .execute_under_savepoint(|sp| {
                sp.execute(
                    "INSERT INTO rows (id, data, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params!["sp-test-id", r#"{"title":"t"}"#, 1000_i64, 1000_i64],
                )?;
                // Force failure after the INSERT — SAVEPOINT must roll back.
                Err(MiniAppError::Validation {
                    field: "test".into(),
                    reason: "forced rollback".into(),
                })
            })
            .await;

        assert!(
            result.is_err(),
            "execute_under_savepoint must propagate the closure error"
        );
        assert!(
            matches!(result.unwrap_err(), MiniAppError::Validation { .. }),
            "error variant must be preserved"
        );

        // After rollback: the row must not exist.
        let rows = store.list(Some(1000), None, None).await.unwrap();
        assert_eq!(
            rows.len(),
            0,
            "SAVEPOINT rollback must revert the INSERT (Crux: SAVEPOINT atomicity)"
        );

        // Verify the SAVEPOINT is gone and normal ops still work.
        store
            .create(serde_json::json!({"title": "after-rollback"}))
            .await
            .expect("store must be usable after SAVEPOINT rollback");
        assert_eq!(store.list(None, None, None).await.unwrap().len(), 1);
    }

    /// Test that execute_under_savepoint commits on success.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_savepoint_commit_on_success() {
        let store = make_test_store().await;

        let result = store
            .execute_under_savepoint(|sp| {
                sp.execute(
                    "INSERT INTO rows (id, data, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params!["sp-ok-id", r#"{"title":"committed"}"#, 1000_i64, 1000_i64],
                )?;
                Ok(42_u32)
            })
            .await;

        assert_eq!(
            result.unwrap(),
            42_u32,
            "successful SAVEPOINT must return value"
        );

        // The INSERT must be committed.
        let rows = store.list(Some(10), None, None).await.unwrap();
        assert_eq!(rows.len(), 1, "committed INSERT must persist");
    }

    /// Concurrency regression: 8 tasks × 100 creates on same Store,
    /// total 800 rows expected, no deadlock or panic.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_store_concurrent_create() {
        let store = Arc::new(make_test_store().await);
        let task_count = 8_usize;
        let rows_per_task = 100_usize;

        let handles: Vec<_> = (0..task_count)
            .map(|task_id| {
                let s = Arc::clone(&store);
                tokio::spawn(async move {
                    for i in 0..rows_per_task {
                        s.create(serde_json::json!({"title": format!("task-{task_id}-row-{i}")}))
                            .await
                            .expect("concurrent create must succeed");
                    }
                })
            })
            .collect();

        futures::future::join_all(handles)
            .await
            .into_iter()
            .for_each(|r| r.expect("task must not panic"));

        let total = store.list(Some(1000), None, None).await.unwrap().len();
        assert_eq!(
            total,
            task_count * rows_per_task,
            "all {total} rows must be present; expected {}",
            task_count * rows_per_task
        );
    }

    /// Concurrency regression: 4 tasks × 50 same-id updates, no deadlock.
    /// Final DB row must be one of the valid values; no Mutex poison.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_store_concurrent_update_same_id() {
        let store = Arc::new(make_test_store().await);

        // Insert the row to update.
        let row = store
            .create(serde_json::json!({"title": "initial"}))
            .await
            .unwrap();
        let id = row.id.clone();

        let task_count = 4_usize;
        let updates_per_task = 50_usize;

        let handles: Vec<_> = (0..task_count)
            .map(|task_id| {
                let s = Arc::clone(&store);
                let row_id = id.clone();
                tokio::spawn(async move {
                    for i in 0..updates_per_task {
                        s.update(
                            &row_id,
                            serde_json::json!({"title": format!("task-{task_id}-update-{i}")}),
                            UpdateMode::Replace,
                        )
                        .await
                        .expect("concurrent update must succeed");
                    }
                })
            })
            .collect();

        futures::future::join_all(handles)
            .await
            .into_iter()
            .for_each(|r| r.expect("task must not panic"));

        // Final state: exactly 1 row, title is one of the last writes.
        let rows = store.list(None, None, None).await.unwrap();
        assert_eq!(rows.len(), 1, "update must not insert extra rows");
        assert!(
            rows[0].data["title"].is_string(),
            "title must be a string after concurrent updates"
        );
    }

    /// Concurrency: Mutex poison propagated as MiniAppError::Schema("mutex poisoned").
    ///
    /// Spawns a blocking task that acquires the Mutex and panics (poisoning it),
    /// then asserts that the next store.get() call returns Schema("mutex poisoned").
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_store_mutex_poison_propagated_as_error() {
        let store = Arc::new(make_test_store().await);

        // Poison the Mutex by panicking inside spawn_blocking while holding the lock.
        let conn = store.conn.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let _guard = conn.lock().unwrap(); // acquire lock
            panic!("intentional poison"); // poison the Mutex
        })
        .await; // JoinError expected — ignore it

        // The Mutex is now poisoned. Any Store operation must return Schema("mutex poisoned").
        let err = store.get("any-id").await.unwrap_err();
        assert!(
            matches!(&err, MiniAppError::Schema(msg) if msg.contains("mutex poisoned")),
            "expected Schema(\"mutex poisoned\"), got: {err:?}"
        );
    }

    /// Crux #1 verification: Store::open must set journal_mode to WAL on a real
    /// file-based database. `:memory:` databases do not support WAL; this test
    /// uses a tempdir to open an actual file and asserts the pragma value.
    #[tokio::test]
    async fn store_open_sets_wal_journal_mode() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("test.db");

        let schema = SchemaConfig {
            table: "test".into(),
            title: None,
            description: None,
            fields: vec![FieldDef {
                name: "title".into(),
                ty: FieldType::String,
                required: false,
                description: None,
            }],
            dump: None,
        };
        let store = Store::open(&db_path, schema)
            .await
            .expect("Store::open should succeed");

        // Query journal_mode through the Store connection to verify WAL was set.
        let mode = {
            let conn = store.conn.lock().expect("lock");
            conn.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
                .expect("PRAGMA journal_mode query")
        };
        assert_eq!(
            mode.to_lowercase(),
            "wal",
            "Store::open must set journal_mode = WAL for dual-registry safety (Crux #1)"
        );
    }

    // --- shallow_merge unit tests (Subtask 1, Crux #1) ---

    /// Helper: build a minimal SchemaConfig with the given fields.
    fn make_schema(fields: Vec<FieldDef>) -> SchemaConfig {
        SchemaConfig {
            table: "test".into(),
            title: None,
            description: None,
            fields,
            dump: None,
        }
    }

    /// AC #3-a: absent fields in the patch are preserved from current.
    #[test]
    fn shallow_merge_preserves_absent_fields() {
        let schema = make_schema(vec![
            FieldDef {
                name: "a".into(),
                ty: FieldType::Number,
                required: true,
                description: None,
            },
            FieldDef {
                name: "b".into(),
                ty: FieldType::Number,
                required: false,
                description: None,
            },
        ]);
        let current = serde_json::json!({"a": 1, "b": 2});
        let patch = serde_json::json!({"a": 9});
        let merged = shallow_merge(current, patch, &schema).expect("merge ok");
        assert_eq!(merged["a"], 9, "patched field must be updated");
        assert_eq!(
            merged["b"], 2,
            "absent patch field must be preserved from current"
        );
    }

    /// AC #3-b: null value for an optional field physically removes it from the merged object.
    #[test]
    fn shallow_merge_deletes_null_for_optional_field() {
        let schema = make_schema(vec![
            FieldDef {
                name: "a".into(),
                ty: FieldType::Number,
                required: true,
                description: None,
            },
            FieldDef {
                name: "b".into(),
                ty: FieldType::Number,
                required: false,
                description: None,
            },
        ]);
        let current = serde_json::json!({"a": 1, "b": 2});
        let patch = serde_json::json!({"b": null});
        let merged = shallow_merge(current, patch, &schema).expect("merge ok");
        assert_eq!(merged["a"], 1);
        assert!(
            merged.get("b").is_none(),
            "null-patched optional field must be physically removed (not set to null)"
        );
    }

    /// AC #3-c: null value for a required field returns a Validation error.
    #[test]
    fn shallow_merge_errors_on_null_for_required_field() {
        let schema = make_schema(vec![FieldDef {
            name: "title".into(),
            ty: FieldType::String,
            required: true,
            description: None,
        }]);
        let current = serde_json::json!({"title": "hello"});
        let patch = serde_json::json!({"title": null});
        let err = shallow_merge(current, patch, &schema).expect_err("must error");
        match err {
            MiniAppError::Validation { field, reason } => {
                assert_eq!(field, "title");
                assert!(
                    reason.contains("required field cannot be deleted via null"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("expected Validation error, got: {other:?}"),
        }
    }

    /// AC #3-d: nested objects are replaced wholesale, not deep-merged.
    #[test]
    fn shallow_merge_replaces_nested_object_wholesale() {
        let schema = make_schema(vec![FieldDef {
            name: "cfg".into(),
            ty: FieldType::Object,
            required: false,
            description: None,
        }]);
        let current = serde_json::json!({"cfg": {"x": 1, "y": 2}});
        let patch = serde_json::json!({"cfg": {"x": 9}});
        let merged = shallow_merge(current, patch, &schema).expect("merge ok");
        assert_eq!(merged["cfg"]["x"], 9, "x must be updated");
        assert!(
            merged["cfg"].get("y").is_none(),
            "y must be absent (nested object replaced wholesale, not deep-merged)"
        );
    }

    /// AC #3-e: non-object patch (array / number / string) returns Validation error.
    #[test]
    fn shallow_merge_rejects_non_object_patch() {
        let schema = make_schema(vec![]);
        let current = serde_json::json!({"a": 1});

        for bad_patch in [
            serde_json::json!([1, 2, 3]),
            serde_json::json!(42),
            serde_json::json!("string"),
        ] {
            let err = shallow_merge(current.clone(), bad_patch, &schema)
                .expect_err("non-object patch must be rejected");
            match err {
                MiniAppError::Validation { field, .. } => {
                    assert_eq!(field, "(root)", "error field must be '(root)'");
                }
                other => panic!("expected Validation error, got: {other:?}"),
            }
        }
    }

    /// AC #3-f: post-merge schema validation catches type mismatches in the merged result.
    /// Tests the full Store::update Merge path (not just shallow_merge in isolation).
    #[tokio::test]
    async fn store_update_merge_runs_post_merge_validation() {
        let store = make_test_store().await;
        let row = store
            .create(serde_json::json!({"title": "x", "state": "open"}))
            .await
            .unwrap();

        // Patch `state` with a number — type mismatch must be caught by post-merge validate.
        let err = store
            .update(&row.id, serde_json::json!({"state": 42}), UpdateMode::Merge)
            .await
            .expect_err("type mismatch must fail post-merge validation");

        assert!(
            matches!(err, MiniAppError::Validation { .. }),
            "expected Validation error, got: {err:?}"
        );
    }
}
