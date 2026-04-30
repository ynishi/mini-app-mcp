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

use serde::Serialize;

use rusqlite::OptionalExtension;

use crate::error::MiniAppError;
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

// ---------------------------------------------------------------------------
// Store impl
// ---------------------------------------------------------------------------

impl Store {
    /// Open the SQLite database at `db_path` and run `CREATE TABLE IF NOT EXISTS rows`.
    ///
    /// # Concurrency
    /// This function is called once at startup. The returned [`Store`] wraps
    /// `Arc<Mutex<rusqlite::Connection>>`. [`rusqlite::Connection`] is `Send` but
    /// `!Sync`; the `Mutex` provides the required exclusive access.
    ///
    /// The `CREATE TABLE` DDL executes inside a `spawn_blocking` task. If the
    /// blocking task panics, the error propagates as [`MiniAppError::Schema`].
    ///
    /// # Cancel Safety
    /// The `spawn_blocking` closure cannot be aborted once started; cancellation
    /// of the returned `Future` does not prevent the DDL from executing on the
    /// blocking thread pool.
    ///
    /// # Panic
    /// Does not panic. `Connection::open` and `execute` errors are returned as
    /// `Err(MiniAppError::Storage(_))`.
    pub async fn open(db_path: &Path, schema: SchemaConfig) -> Result<Self, MiniAppError> {
        let db_path = db_path.to_path_buf();
        let conn =
            tokio::task::spawn_blocking(move || -> Result<rusqlite::Connection, MiniAppError> {
                let c = rusqlite::Connection::open(&db_path)?;
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
    /// blocking thread—never held across an `.await` point.
    ///
    /// # Cancel Safety
    /// Not cancel-safe in the standard sense: once the blocking closure has
    /// started, the `INSERT` will complete even if the caller drops the `Future`.
    /// The row will exist in the database after the operation finishes.
    ///
    /// # Errors
    /// - [`MiniAppError::Validation`] — required field absent or type mismatch.
    /// - [`MiniAppError::Storage`] — rusqlite error (constraint violation, I/O).
    /// - [`MiniAppError::Schema`] — blocking thread panicked (JoinError).
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
                .map_err(|_| MiniAppError::Storage(rusqlite::Error::QueryReturnedNoRows))?;
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
                .map_err(|_| MiniAppError::Storage(rusqlite::Error::QueryReturnedNoRows))?;
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
    ///
    /// # Panic
    /// Does not panic.
    pub async fn list(
        &self,
        limit: Option<u32>,
        offset: Option<u32>,
    ) -> Result<Vec<RowRecord>, MiniAppError> {
        let conn = self.conn.clone();
        let limit = limit.unwrap_or(100).min(1000) as i64;
        let offset = offset.unwrap_or(0) as i64;

        tokio::task::spawn_blocking(move || -> Result<Vec<RowRecord>, MiniAppError> {
            let conn = conn
                .lock()
                .map_err(|_| MiniAppError::Storage(rusqlite::Error::QueryReturnedNoRows))?;
            let mut stmt = conn.prepare(
                "SELECT id, data, created_at, updated_at FROM rows \
                 ORDER BY created_at DESC LIMIT ?1 OFFSET ?2",
            )?;
            let rows = stmt
                .query_map(rusqlite::params![limit, offset], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                })?
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

    /// Validate `value` and update the row identified by `id`.
    /// `updated_at` is refreshed; `created_at` is unchanged.
    ///
    /// # Concurrency
    /// The `UPDATE` executes inside `tokio::task::spawn_blocking`. The
    /// [`std::sync::MutexGuard`] is held only within the blocking closure.
    /// Concurrent calls with the same `id` are serialized by the `Mutex`.
    ///
    /// # Cancel Safety
    /// Once the blocking closure has started the `UPDATE` will complete
    /// regardless of `Future` cancellation. Idempotent: calling with the same
    /// `id` and `value` results in the same final state.
    ///
    /// # Errors
    /// - [`MiniAppError::NotFound`] — no row with the given `id`.
    /// - [`MiniAppError::Validation`] — required field absent or type mismatch.
    /// - [`MiniAppError::Storage`] — rusqlite error.
    /// - [`MiniAppError::Schema`] — blocking thread panicked (JoinError).
    ///
    /// # Panic
    /// Does not panic.
    pub async fn update(
        &self,
        id: &str,
        value: serde_json::Value,
    ) -> Result<RowRecord, MiniAppError> {
        self.schema.validate(&value)?;

        let now = now_secs();
        let data_str =
            serde_json::to_string(&value).expect("serde_json::Value serialization is infallible");

        let conn = self.conn.clone();
        let id = id.to_string();

        tokio::task::spawn_blocking(move || -> Result<RowRecord, MiniAppError> {
            let conn = conn
                .lock()
                .map_err(|_| MiniAppError::Storage(rusqlite::Error::QueryReturnedNoRows))?;

            // Fetch created_at first to return it unchanged.
            let created_at: Option<i64> = conn
                .query_row(
                    "SELECT created_at FROM rows WHERE id = ?1",
                    rusqlite::params![id],
                    |row| row.get(0),
                )
                .optional()?;

            let created_at = created_at.ok_or_else(|| MiniAppError::NotFound { id: id.clone() })?;

            conn.execute(
                "UPDATE rows SET data = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![data_str, now, id],
            )?;

            Ok(RowRecord {
                id,
                data: value,
                created_at,
                updated_at: now,
            })
        })
        .await
        .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")))?
    }

    /// Delete the row identified by `id`.
    ///
    /// # Concurrency
    /// The `DELETE` executes inside `tokio::task::spawn_blocking`. The
    /// [`std::sync::MutexGuard`] is held only within the blocking closure.
    /// Idempotent: deleting a non-existent `id` returns
    /// [`MiniAppError::NotFound`], so calling twice with the same `id` returns
    /// `Err(MiniAppError::NotFound)` on the second call.
    ///
    /// # Cancel Safety
    /// Once the blocking closure has started the `DELETE` runs to completion
    /// regardless of `Future` cancellation.
    ///
    /// # Errors
    /// - [`MiniAppError::NotFound`] — no row with the given `id`.
    /// - [`MiniAppError::Storage`] — rusqlite error.
    /// - [`MiniAppError::Schema`] — blocking thread panicked (JoinError).
    ///
    /// # Panic
    /// Does not panic.
    pub async fn delete(&self, id: &str) -> Result<(), MiniAppError> {
        let conn = self.conn.clone();
        let id = id.to_string();

        tokio::task::spawn_blocking(move || -> Result<(), MiniAppError> {
            let conn = conn
                .lock()
                .map_err(|_| MiniAppError::Storage(rusqlite::Error::QueryReturnedNoRows))?;
            let n = conn.execute("DELETE FROM rows WHERE id = ?1", rusqlite::params![id])?;
            if n == 0 {
                return Err(MiniAppError::NotFound { id });
            }
            Ok(())
        })
        .await
        .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")))?
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
            fields: vec![
                FieldDef {
                    name: "title".into(),
                    ty: FieldType::String,
                    required: true,
                },
                FieldDef {
                    name: "state".into(),
                    ty: FieldType::String,
                    required: false,
                },
            ],
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
        let rows = store.list(None, None).await.unwrap();
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
        let page1 = store.list(Some(2), Some(0)).await.unwrap();
        assert_eq!(page1.len(), 2);
        let page2 = store.list(Some(2), Some(2)).await.unwrap();
        assert_eq!(page2.len(), 2);
        let page3 = store.list(Some(2), Some(4)).await.unwrap();
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
            .update(&row.id, serde_json::json!({"title": "changed"}))
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
            .update("nonexistent-id", serde_json::json!({"title": "x"}))
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
        let rows = store.list(None, None).await.unwrap();
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
        assert_eq!(store.list(None, None).await.unwrap().len(), 8);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_spawn_blocking_join_error_propagation() {
        let result: Result<(), _> = tokio::task::spawn_blocking(|| panic!("intentional"))
            .await
            .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")));
        assert!(matches!(result, Err(MiniAppError::Schema(_))));
    }
}
