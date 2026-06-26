/// Row-level change history for mini-app-mcp.
///
/// Every `create`, `update`, and `delete` operation on a store row is
/// recorded atomically (in the same SQLite transaction as the data DML)
/// into the `_row_history` table.  This module provides:
///
/// - [`ensure_history_table`] — idempotent DDL, called once per [`Store::open`].
/// - [`record_in_tx`] — write one history entry inside an open `Transaction`.
/// - [`fetch_at`] — point-in-time lookup: latest version at or before a Unix
///   timestamp.
/// - [`list_versions`] — all history entries for a (table, row_id) pair.
/// - [`purge_old_history`] — retention-based cleanup.
///
/// # Atomicity guarantee
///
/// `record_in_tx` takes a reference to an already-open `rusqlite::Transaction`;
/// callers are responsible for `tx.commit()` or `tx.rollback()`.  The history
/// INSERT and the data DML share the same transaction, so either both persist
/// or neither does.
///
/// # Schema
///
/// ```sql
/// CREATE TABLE IF NOT EXISTS _row_history (
///     id              INTEGER PRIMARY KEY AUTOINCREMENT,
///     table_name      TEXT    NOT NULL,
///     row_id          TEXT    NOT NULL,
///     version         INTEGER NOT NULL,
///     recorded_at     INTEGER NOT NULL,
///     op              TEXT    NOT NULL CHECK (op IN ('create','update','delete')),
///     data_json       TEXT,
///     prev_data_json  TEXT
/// );
/// CREATE UNIQUE INDEX IF NOT EXISTS idx_row_history_version
///     ON _row_history(table_name, row_id, version);
/// CREATE INDEX IF NOT EXISTS idx_row_history_lookup
///     ON _row_history(table_name, row_id, recorded_at DESC);
/// ```
use rusqlite::{Transaction, params};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

const CREATE_HISTORY_TABLE_SQL: &str = "
    CREATE TABLE IF NOT EXISTS _row_history (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        table_name      TEXT    NOT NULL,
        row_id          TEXT    NOT NULL,
        version         INTEGER NOT NULL,
        recorded_at     INTEGER NOT NULL,
        op              TEXT    NOT NULL CHECK (op IN ('create','update','delete')),
        data_json       TEXT,
        prev_data_json  TEXT
    )
";

const CREATE_HISTORY_IDX_VERSION_SQL: &str = "
    CREATE UNIQUE INDEX IF NOT EXISTS idx_row_history_version
        ON _row_history(table_name, row_id, version)
";

const CREATE_HISTORY_IDX_LOOKUP_SQL: &str = "
    CREATE INDEX IF NOT EXISTS idx_row_history_lookup
        ON _row_history(table_name, row_id, recorded_at DESC)
";

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The operation that triggered a history entry.
///
/// Only three values are permitted by the `_row_history.op` CHECK constraint:
/// `create`, `update`, `delete`.  Point-in-time restores of deleted rows are
/// recorded as `Create` (same-id re-insertion); restores of existing rows go
/// through `Store::update(mode=replace)` and are recorded as `Update`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HistoryOp {
    /// Row was created (`Store::create`) or restored after deletion (`Store::restore_row`).
    Create,
    /// Row was updated (`Store::update`), including point-in-time rollback of a live row.
    Update,
    /// Row was deleted (`Store::delete`).
    Delete,
}

impl HistoryOp {
    /// Returns the string representation stored in SQLite.
    ///
    /// All three values satisfy `CHECK (op IN ('create','update','delete'))`.
    pub fn as_str(self) -> &'static str {
        match self {
            HistoryOp::Create => "create",
            HistoryOp::Update => "update",
            HistoryOp::Delete => "delete",
        }
    }
}

impl rusqlite::types::FromSql for HistoryOp {
    fn column_result(
        value: rusqlite::types::ValueRef<'_>,
    ) -> rusqlite::types::FromSqlResult<Self> {
        let s = String::column_result(value)?;
        match s.as_str() {
            "create" => Ok(HistoryOp::Create),
            "update" => Ok(HistoryOp::Update),
            "delete" => Ok(HistoryOp::Delete),
            _ => Err(rusqlite::types::FromSqlError::Other(
                format!("unknown HistoryOp: {s}").into(),
            )),
        }
    }
}

/// A single entry in the `_row_history` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryRecord {
    /// Auto-increment primary key.
    pub id: i64,
    /// The logical table name from `SchemaConfig::table`.
    pub table_name: String,
    /// The UUID of the data row.
    pub row_id: String,
    /// Monotonically increasing version number within `(table_name, row_id)`.
    pub version: i64,
    /// Unix epoch seconds when this entry was recorded.
    pub recorded_at: i64,
    /// The operation that produced this entry.
    pub op: HistoryOp,
    /// Full JSON of the row *after* the operation (None for deletes).
    pub data_json: Option<String>,
    /// Full JSON of the row *before* the operation (None for creates).
    pub prev_data_json: Option<String>,
}

// ---------------------------------------------------------------------------
// DDL helpers
// ---------------------------------------------------------------------------

/// Create the `_row_history` table and its indexes if they do not already exist.
///
/// This is idempotent — safe to call on every [`Store::open`].  All three SQL
/// statements use `IF NOT EXISTS` so re-opening an existing database is a no-op.
///
/// # Errors
///
/// Propagates any [`rusqlite::Error`] from `execute_batch`.
pub fn ensure_history_table(conn: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(CREATE_HISTORY_TABLE_SQL)?;
    conn.execute_batch(CREATE_HISTORY_IDX_VERSION_SQL)?;
    conn.execute_batch(CREATE_HISTORY_IDX_LOOKUP_SQL)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Write path
// ---------------------------------------------------------------------------

/// Insert one history row inside an already-open transaction.
///
/// The `version` is computed as `MAX(version) + 1` (or 1 if no prior entry
/// exists) within the `(table_name, row_id)` namespace.  The caller must
/// commit or roll back the transaction after this call.
///
/// # Arguments
///
/// - `tx` — open `rusqlite::Transaction`; NOT committed here.
/// - `table_name` — `SchemaConfig::table` of the store.
/// - `row_id` — the UUID of the row being mutated.
/// - `op` — which operation produced this entry.
/// - `data_json` — serialised JSON of the row *after* the operation, or `None`
///   for pure deletions.
/// - `prev_data_json` — serialised JSON of the row *before* the operation, or
///   `None` for creates.
/// - `recorded_at` — Unix epoch seconds for the entry timestamp (supplied by
///   the caller so that tests can use literal values instead of `Instant::now`).
///
/// # Errors
///
/// Propagates [`rusqlite::Error`] from `query_row` or `execute`.
pub fn record_in_tx(
    tx: &Transaction<'_>,
    table_name: &str,
    row_id: &str,
    op: HistoryOp,
    data_json: Option<&str>,
    prev_data_json: Option<&str>,
    recorded_at: i64,
) -> Result<(), rusqlite::Error> {
    // Compute the next monotonic version for this (table, row) pair.
    let version: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(version), 0) + 1 \
             FROM _row_history \
             WHERE table_name = ?1 AND row_id = ?2",
            params![table_name, row_id],
            |row| row.get(0),
        )
        .unwrap_or(1);

    tx.execute(
        "INSERT INTO _row_history \
             (table_name, row_id, version, recorded_at, op, data_json, prev_data_json) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            table_name,
            row_id,
            version,
            recorded_at,
            op.as_str(),
            data_json,
            prev_data_json,
        ],
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Read path
// ---------------------------------------------------------------------------

/// Fetch the latest history entry where `recorded_at <= at_unix_secs`.
///
/// Returns `None` when no matching entry exists (the row had not yet been
/// created at that point in time, or `at_unix_secs` is before any recorded
/// event).
///
/// # Errors
///
/// Propagates [`rusqlite::Error`] from `query_row`.
pub fn fetch_at(
    conn: &rusqlite::Connection,
    table_name: &str,
    row_id: &str,
    at_unix_secs: i64,
) -> Result<Option<HistoryRecord>, rusqlite::Error> {
    use rusqlite::OptionalExtension;

    conn.query_row(
        "SELECT id, table_name, row_id, version, recorded_at, op, data_json, prev_data_json \
         FROM _row_history \
         WHERE table_name = ?1 AND row_id = ?2 AND recorded_at <= ?3 \
         ORDER BY recorded_at DESC, version DESC \
         LIMIT 1",
        params![table_name, row_id, at_unix_secs],
        |row| {
            Ok(HistoryRecord {
                id: row.get(0)?,
                table_name: row.get(1)?,
                row_id: row.get(2)?,
                version: row.get(3)?,
                recorded_at: row.get(4)?,
                op: row.get(5)?,
                data_json: row.get(6)?,
                prev_data_json: row.get(7)?,
            })
        },
    )
    .optional()
}

/// Return all history entries for `(table_name, row_id)`, oldest first.
///
/// # Errors
///
/// Propagates [`rusqlite::Error`] from `prepare` / `query_map`.
pub fn list_versions(
    conn: &rusqlite::Connection,
    table_name: &str,
    row_id: &str,
) -> Result<Vec<HistoryRecord>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, table_name, row_id, version, recorded_at, op, data_json, prev_data_json \
         FROM _row_history \
         WHERE table_name = ?1 AND row_id = ?2 \
         ORDER BY version ASC",
    )?;

    let rows = stmt.query_map(params![table_name, row_id], |row| {
        Ok(HistoryRecord {
            id: row.get(0)?,
            table_name: row.get(1)?,
            row_id: row.get(2)?,
            version: row.get(3)?,
            recorded_at: row.get(4)?,
            op: row.get(5)?,
            data_json: row.get(6)?,
            prev_data_json: row.get(7)?,
        })
    })?;

    rows.collect()
}

// ---------------------------------------------------------------------------
// Purge
// ---------------------------------------------------------------------------

/// Delete history entries that fall outside the retention policy.
///
/// Two independent limits are enforced:
///
/// 1. **Age limit** (`retention_days > 0`): any entry whose `recorded_at` is
///    older than `retention_days * 86400` seconds before `now_secs` is deleted.
/// 2. **Version count limit** (`max_per_row > 0`): for each `(table_name,
///    row_id)` pair, only the newest `max_per_row` entries are kept; older
///    ones are deleted.
///
/// Either limit may be disabled by passing `0`.  If both are `0` nothing is
/// deleted.
///
/// This function executes synchronously; callers must run it inside
/// `tokio::task::spawn_blocking`.
///
/// # Arguments
///
/// - `conn` — exclusive `&mut` reference to the connection (not in a
///   transaction; the function runs two independent `DELETE` statements).
/// - `retention_days` — maximum age of entries in days; 0 = unlimited.
/// - `max_per_row` — maximum number of history entries kept per row; 0 = unlimited.
/// - `now_secs` — current Unix epoch seconds (passed explicitly so tests can
///   use literal values).
///
/// # Errors
///
/// Propagates [`rusqlite::Error`] from any SQL statement.
pub fn purge_old_history(
    conn: &rusqlite::Connection,
    retention_days: u32,
    max_per_row: u32,
    now_secs: i64,
) -> Result<(), rusqlite::Error> {
    // ------------------------------------------------------------------
    // 1. Age-based purge
    // ------------------------------------------------------------------
    if retention_days > 0 {
        let cutoff = now_secs - (retention_days as i64) * 86_400;
        conn.execute(
            "DELETE FROM _row_history WHERE recorded_at < ?1",
            params![cutoff],
        )?;
    }

    // ------------------------------------------------------------------
    // 2. Per-row version count purge
    //
    // For each (table_name, row_id) pair, delete every entry whose
    // `version` rank (newest-first) exceeds `max_per_row`.  We use a
    // correlated subquery to find the version threshold.
    // ------------------------------------------------------------------
    if max_per_row > 0 {
        // Collect distinct (table_name, row_id) pairs that have more entries
        // than max_per_row, then delete the oldest excess entries.
        //
        // SQLite window functions (ROW_NUMBER) are available since 3.25.0
        // (2018-09-15); we use a correlated subquery for broader compatibility.
        conn.execute(
            "DELETE FROM _row_history \
             WHERE id IN ( \
                 SELECT h.id \
                 FROM _row_history h \
                 WHERE ( \
                     SELECT COUNT(*) \
                     FROM _row_history h2 \
                     WHERE h2.table_name = h.table_name \
                       AND h2.row_id     = h.row_id \
                       AND h2.version   >= h.version \
                 ) > ?1 \
             )",
            params![max_per_row],
        )?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Low-level insert into `_row_history` within an existing transaction.
///
/// Used by e2e tests that need to inject history rows with fully controlled
/// `recorded_at` / `version` values without touching `Instant::now`.
#[allow(clippy::too_many_arguments)]
pub fn record_in_tx_for_test(
    tx: &rusqlite::Transaction<'_>,
    table_name: &str,
    row_id: &str,
    op: HistoryOp,
    data_json: &str,
    prev_data_json: Option<&str>,
    recorded_at: i64,
    version: i64,
) -> Result<(), rusqlite::Error> {
    tx.execute(
        "INSERT INTO _row_history \
             (table_name, row_id, version, recorded_at, op, data_json, prev_data_json) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            table_name,
            row_id,
            version,
            recorded_at,
            op.as_str(),
            data_json,
            prev_data_json,
        ],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests (unit)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn open_mem() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        ensure_history_table(&conn).unwrap();
        conn
    }

    #[test]
    fn ensure_table_is_idempotent() {
        let conn = open_mem();
        // Calling again must not error
        ensure_history_table(&conn).unwrap();
    }

    #[test]
    fn record_and_list_versions() {
        let conn = open_mem();
        let tx = conn.unchecked_transaction().unwrap();
        record_in_tx(&tx, "t", "r1", HistoryOp::Create, Some("{\"a\":1}"), None, 1000).unwrap();
        record_in_tx(
            &tx,
            "t",
            "r1",
            HistoryOp::Update,
            Some("{\"a\":2}"),
            Some("{\"a\":1}"),
            2000,
        )
        .unwrap();
        tx.commit().unwrap();

        let versions = list_versions(&conn, "t", "r1").unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].version, 1);
        assert_eq!(versions[0].op, HistoryOp::Create);
        assert_eq!(versions[1].version, 2);
        assert_eq!(versions[1].op, HistoryOp::Update);
    }

    #[test]
    fn fetch_at_returns_correct_snapshot() {
        let conn = open_mem();
        {
            let tx = conn.unchecked_transaction().unwrap();
            record_in_tx(&tx, "t", "r1", HistoryOp::Create, Some("{\"v\":1}"), None, 1000)
                .unwrap();
            record_in_tx(
                &tx,
                "t",
                "r1",
                HistoryOp::Update,
                Some("{\"v\":2}"),
                Some("{\"v\":1}"),
                2000,
            )
            .unwrap();
            tx.commit().unwrap();
        }

        // Exactly at version 1 timestamp
        let h = fetch_at(&conn, "t", "r1", 1000).unwrap().unwrap();
        assert_eq!(h.version, 1);
        assert_eq!(h.data_json.as_deref(), Some("{\"v\":1}"));

        // Between versions — should return version 1
        let h = fetch_at(&conn, "t", "r1", 1500).unwrap().unwrap();
        assert_eq!(h.version, 1);

        // At or after version 2
        let h = fetch_at(&conn, "t", "r1", 2000).unwrap().unwrap();
        assert_eq!(h.version, 2);

        // Before any entry — None
        let h = fetch_at(&conn, "t", "r1", 999).unwrap();
        assert!(h.is_none());
    }

    #[test]
    fn purge_by_age() {
        let conn = open_mem();
        {
            let tx = conn.unchecked_transaction().unwrap();
            // recorded 40 days ago
            record_in_tx(&tx, "t", "r1", HistoryOp::Create, Some("{}"), None, 1000).unwrap();
            // recorded 10 days ago
            let recent = 1000 + 30 * 86_400 + 1;
            record_in_tx(
                &tx,
                "t",
                "r1",
                HistoryOp::Update,
                Some("{\"a\":1}"),
                Some("{}"),
                recent,
            )
            .unwrap();
            tx.commit().unwrap();
        }

        // Purge entries older than 30 days; now_secs = 1000 + 40*86400
        let now = 1000 + 40 * 86_400_i64;
        purge_old_history(&conn, 30, 0, now).unwrap();

        let versions = list_versions(&conn, "t", "r1").unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].op, HistoryOp::Update);
    }

    #[test]
    fn purge_by_max_per_row() {
        let conn = open_mem();
        {
            let tx = conn.unchecked_transaction().unwrap();
            for i in 0..5_i64 {
                record_in_tx(
                    &tx,
                    "t",
                    "r1",
                    HistoryOp::Update,
                    Some("{}"),
                    Some("{}"),
                    1000 + i * 100,
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }

        purge_old_history(&conn, 0, 3, 9999).unwrap();

        let versions = list_versions(&conn, "t", "r1").unwrap();
        assert_eq!(versions.len(), 3, "only newest 3 should remain");
        // Versions 3,4,5 (newest) should remain
        assert_eq!(versions[0].version, 3);
        assert_eq!(versions[2].version, 5);
    }
}
