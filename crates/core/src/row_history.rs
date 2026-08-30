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
/// # Two-tier storage (raw + compressed archive)
///
/// History is an accident-recovery device (undo for direct AI/agent writes),
/// not a log store — but in practice it must survive log-style write volume
/// without eating the disk (fly.io incident 2026-08: one 178KB row × 949
/// updates → 222MB of full-copy history = 94% of the DB).  Two mechanisms
/// bound the raw table:
///
/// 1. **No `prev_data_json` duplication**: each entry stores only the
///    post-operation state; the pre-operation state is the previous entry's
///    `data_json` (halves the write volume). The column is retained for
///    reading databases written by older versions.
/// 2. **Archive roll**: when a `(table, row_id)` pair accumulates more than
///    `keep_recent + chunk_min` raw entries, the oldest entries are compacted
///    into a single zstd-compressed JSONL blob in `_row_history_archive` and
///    deleted from the raw table — history is never discarded, only
///    compressed (~x1000 on near-duplicate JSON versions, measured).
///    The most recent `keep_recent` entries always stay raw so point-in-time
///    undo of recent accidents needs no decompression.
///
/// Tunables (read once per process):
/// - `MINI_APP_HISTORY_KEEP_RECENT` (default 16) — raw entries kept per row.
/// - `MINI_APP_HISTORY_CHUNK_MIN` (default 48) — minimum entries rolled per
///   archive chunk (roll triggers at `keep_recent + chunk_min`).
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

const CREATE_HISTORY_ARCHIVE_TABLE_SQL: &str = "
    CREATE TABLE IF NOT EXISTS _row_history_archive (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        table_name      TEXT    NOT NULL,
        row_id          TEXT    NOT NULL,
        version_start   INTEGER NOT NULL,
        version_end     INTEGER NOT NULL,
        recorded_start  INTEGER NOT NULL,
        recorded_end    INTEGER NOT NULL,
        entry_count     INTEGER NOT NULL,
        format          TEXT    NOT NULL DEFAULT 'jsonl-zstd',
        blob            BLOB    NOT NULL
    )
";

const CREATE_HISTORY_ARCHIVE_IDX_SQL: &str = "
    CREATE INDEX IF NOT EXISTS idx_row_history_archive_lookup
        ON _row_history_archive(table_name, row_id, recorded_start DESC)
";

/// zstd compression level for archive chunks. Level 3 keeps the roll cheap;
/// its match window (≥1MB at typical chunk sizes) spans adjacent versions,
/// which is where the real ratio comes from (near-duplicate JSON).
const ARCHIVE_ZSTD_LEVEL: i32 = 3;

fn env_tunable(name: &str, default: u32, cell: &'static std::sync::OnceLock<u32>) -> u32 {
    *cell.get_or_init(|| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(default)
    })
}

/// Raw history entries kept per `(table, row_id)` (undo hot set).
fn keep_recent() -> u32 {
    static CELL: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    env_tunable("MINI_APP_HISTORY_KEEP_RECENT", 16, &CELL)
}

/// Minimum entries per archive chunk (avoids many tiny blobs).
fn chunk_min() -> u32 {
    static CELL: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    env_tunable("MINI_APP_HISTORY_CHUNK_MIN", 48, &CELL)
}

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
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
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
    conn.execute_batch(CREATE_HISTORY_ARCHIVE_TABLE_SQL)?;
    conn.execute_batch(CREATE_HISTORY_ARCHIVE_IDX_SQL)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Archive entry (JSONL line inside a compressed chunk)
// ---------------------------------------------------------------------------

/// One archived history entry as serialised inside an archive chunk blob.
///
/// A chunk blob is zstd-compressed JSONL: one `ArchivedEntry` per line,
/// ordered by `version` ascending. `prev_data_json` is carried only for
/// entries that were written by older versions of this crate (current
/// writers always leave it `None`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArchivedEntry {
    version: i64,
    recorded_at: i64,
    op: HistoryOp,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    data_json: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    prev_data_json: Option<String>,
}

impl ArchivedEntry {
    fn into_record(self, table_name: &str, row_id: &str) -> HistoryRecord {
        HistoryRecord {
            // Archived entries no longer have a raw-table rowid; 0 marks them
            // as archive-sourced (raw ids are AUTOINCREMENT and start at 1).
            id: 0,
            table_name: table_name.to_string(),
            row_id: row_id.to_string(),
            version: self.version,
            recorded_at: self.recorded_at,
            op: self.op,
            data_json: self.data_json,
            prev_data_json: self.prev_data_json,
        }
    }
}

fn decode_chunk(blob: &[u8]) -> Result<Vec<ArchivedEntry>, rusqlite::Error> {
    let jsonl = zstd::decode_all(blob).map_err(|e| {
        rusqlite::Error::ToSqlConversionFailure(
            format!("archive chunk zstd decode failed: {e}").into(),
        )
    })?;
    let text = String::from_utf8(jsonl).map_err(|e| {
        rusqlite::Error::ToSqlConversionFailure(format!("archive chunk not UTF-8: {e}").into())
    })?;
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            serde_json::from_str::<ArchivedEntry>(l).map_err(|e| {
                rusqlite::Error::ToSqlConversionFailure(
                    format!("archive chunk JSONL parse failed: {e}").into(),
                )
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Write path
// ---------------------------------------------------------------------------

/// Insert one history row inside an already-open transaction.
///
/// The `version` is computed as `MAX(version) + 1` (or 1 if no prior entry
/// exists) within the `(table_name, row_id)` namespace — the raw table and
/// the archive are both consulted so versions stay monotonic across rolls.
/// The caller must commit or roll back the transaction after this call.
///
/// `prev_data_json` is intentionally NOT written (the pre-operation state is
/// the previous entry's `data_json`; storing both doubled the history volume
/// — see module docs).  After the insert, the entry count for this row is
/// checked and the oldest entries are rolled into `_row_history_archive`
/// when the raw tier exceeds `keep_recent + chunk_min`.
///
/// # Arguments
///
/// - `tx` — open `rusqlite::Transaction`; NOT committed here.
/// - `table_name` — `SchemaConfig::table` of the store.
/// - `row_id` — the UUID of the row being mutated.
/// - `op` — which operation produced this entry.
/// - `data_json` — serialised JSON of the row *after* the operation, or `None`
///   for pure deletions.
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
    recorded_at: i64,
) -> Result<(), rusqlite::Error> {
    // Compute the next monotonic version for this (table, row) pair.
    // Rolled entries live in the archive, so MAX over both tiers.
    let version: i64 = tx.query_row(
        "SELECT MAX(v) FROM ( \
                 SELECT COALESCE(MAX(version), 0) AS v \
                 FROM _row_history \
                 WHERE table_name = ?1 AND row_id = ?2 \
                 UNION ALL \
                 SELECT COALESCE(MAX(version_end), 0) AS v \
                 FROM _row_history_archive \
                 WHERE table_name = ?1 AND row_id = ?2 \
             )",
        params![table_name, row_id],
        |row| row.get::<_, i64>(0),
    )? + 1;

    tx.execute(
        "INSERT INTO _row_history \
             (table_name, row_id, version, recorded_at, op, data_json, prev_data_json) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
        params![
            table_name,
            row_id,
            version,
            recorded_at,
            op.as_str(),
            data_json
        ],
    )?;

    maybe_roll_into_archive(
        tx,
        table_name,
        row_id,
        keep_recent() as i64,
        chunk_min() as i64,
    )?;

    Ok(())
}

/// Roll the oldest raw entries for `(table_name, row_id)` into a compressed
/// archive chunk when the raw tier exceeds `keep_recent() + chunk_min()`.
///
/// The newest `keep_recent()` entries stay raw (undo hot set); everything
/// older is serialised as JSONL, zstd-compressed into one
/// `_row_history_archive` row, and deleted from `_row_history` — all inside
/// the caller's transaction, so a failure rolls back both the data DML and
/// the compaction atomically.
///
/// At most `chunk_min + 1` entries are rolled per call: a pre-existing
/// backlog (a database written before archive roll existed) drains gradually
/// over subsequent writes instead of being materialised in memory at once —
/// on the incident database (949 × 178KB entries) an unbounded roll would
/// peak near 0.5GB inside a 512MB machine and crash-loop the row.
fn maybe_roll_into_archive(
    tx: &Transaction<'_>,
    table_name: &str,
    row_id: &str,
    keep: i64,
    chunk_min: i64,
) -> Result<(), rusqlite::Error> {
    let count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM _row_history WHERE table_name = ?1 AND row_id = ?2",
        params![table_name, row_id],
        |row| row.get(0),
    )?;
    if count <= keep + chunk_min {
        return Ok(());
    }

    let roll_n = (count - keep).min(chunk_min + 1);
    let mut stmt = tx.prepare(
        "SELECT id, version, recorded_at, op, data_json, prev_data_json \
         FROM _row_history \
         WHERE table_name = ?1 AND row_id = ?2 \
         ORDER BY version ASC \
         LIMIT ?3",
    )?;
    let mut ids: Vec<i64> = Vec::with_capacity(roll_n as usize);
    let mut entries: Vec<ArchivedEntry> = Vec::with_capacity(roll_n as usize);
    let rows = stmt.query_map(params![table_name, row_id, roll_n], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            ArchivedEntry {
                version: row.get(1)?,
                recorded_at: row.get(2)?,
                op: row.get(3)?,
                data_json: row.get(4)?,
                prev_data_json: row.get(5)?,
            },
        ))
    })?;
    for r in rows {
        let (id, entry) = r?;
        ids.push(id);
        entries.push(entry);
    }
    drop(stmt);
    if entries.is_empty() {
        return Ok(());
    }

    let mut jsonl = String::new();
    for e in &entries {
        jsonl.push_str(
            &serde_json::to_string(e).expect("ArchivedEntry serialization is infallible"),
        );
        jsonl.push('\n');
    }
    let blob = zstd::encode_all(jsonl.as_bytes(), ARCHIVE_ZSTD_LEVEL).map_err(|e| {
        rusqlite::Error::ToSqlConversionFailure(
            format!("archive chunk zstd encode failed: {e}").into(),
        )
    })?;

    let first = entries.first().expect("non-empty");
    let last = entries.last().expect("non-empty");
    tx.execute(
        "INSERT INTO _row_history_archive \
             (table_name, row_id, version_start, version_end, \
              recorded_start, recorded_end, entry_count, format, blob) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'jsonl-zstd', ?8)",
        params![
            table_name,
            row_id,
            first.version,
            last.version,
            first.recorded_at,
            last.recorded_at,
            entries.len() as i64,
            blob,
        ],
    )?;

    // Delete rolled entries. ids came from this tx, bounded by roll_n.
    for chunk in ids.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!("DELETE FROM _row_history WHERE id IN ({placeholders})");
        tx.execute(&sql, rusqlite::params_from_iter(chunk.iter()))?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Read path
// ---------------------------------------------------------------------------

/// Fetch the latest history entry where `recorded_at <= at_unix_secs`.
///
/// The raw tier is consulted first; when it has no entry at or before the
/// timestamp, the compressed archive tier is searched (the archive always
/// holds strictly older versions than the raw tier, so a raw hit is always
/// the correct answer).
///
/// Returns `None` when no matching entry exists in either tier (the row had
/// not yet been created at that point in time, or `at_unix_secs` is before
/// any recorded event).
///
/// # Errors
///
/// Propagates [`rusqlite::Error`] from `query_row`, and decode errors for a
/// corrupted archive chunk.
pub fn fetch_at(
    conn: &rusqlite::Connection,
    table_name: &str,
    row_id: &str,
    at_unix_secs: i64,
) -> Result<Option<HistoryRecord>, rusqlite::Error> {
    use rusqlite::OptionalExtension;

    let raw = conn
        .query_row(
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
        .optional()?;
    if raw.is_some() {
        return Ok(raw);
    }

    // Archive tier: the chunk with the greatest recorded_start <= t contains
    // the answer if one exists (its first entry already satisfies <= t).
    let blob: Option<Vec<u8>> = conn
        .query_row(
            "SELECT blob FROM _row_history_archive \
             WHERE table_name = ?1 AND row_id = ?2 AND recorded_start <= ?3 \
             ORDER BY recorded_start DESC, version_end DESC \
             LIMIT 1",
            params![table_name, row_id, at_unix_secs],
            |row| row.get(0),
        )
        .optional()?;
    let Some(blob) = blob else {
        return Ok(None);
    };
    let entries = decode_chunk(&blob)?;
    Ok(entries
        .into_iter()
        .filter(|e| e.recorded_at <= at_unix_secs)
        .max_by_key(|e| (e.recorded_at, e.version))
        .map(|e| e.into_record(table_name, row_id)))
}

/// Return all history entries for `(table_name, row_id)`, oldest first.
///
/// Archived chunks are decompressed and prepended before the raw tier so the
/// result is the complete version sequence.  Note: for rows with a very long
/// history this materialises every archived version in memory — this is the
/// rescue/audit path, not a hot path.
///
/// # Errors
///
/// Propagates [`rusqlite::Error`] from `prepare` / `query_map`, and decode
/// errors for a corrupted archive chunk.
pub fn list_versions(
    conn: &rusqlite::Connection,
    table_name: &str,
    row_id: &str,
) -> Result<Vec<HistoryRecord>, rusqlite::Error> {
    let mut out: Vec<HistoryRecord> = Vec::new();

    let mut astmt = conn.prepare(
        "SELECT blob FROM _row_history_archive \
         WHERE table_name = ?1 AND row_id = ?2 \
         ORDER BY version_start ASC",
    )?;
    let blobs = astmt.query_map(params![table_name, row_id], |row| row.get::<_, Vec<u8>>(0))?;
    for blob in blobs {
        for e in decode_chunk(&blob?)? {
            out.push(e.into_record(table_name, row_id));
        }
    }

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

    for r in rows {
        out.push(r?);
    }
    Ok(out)
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
        record_in_tx(&tx, "t", "r1", HistoryOp::Create, Some("{\"a\":1}"), 1000).unwrap();
        record_in_tx(&tx, "t", "r1", HistoryOp::Update, Some("{\"a\":2}"), 2000).unwrap();
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
            record_in_tx(&tx, "t", "r1", HistoryOp::Create, Some("{\"v\":1}"), 1000).unwrap();
            record_in_tx(&tx, "t", "r1", HistoryOp::Update, Some("{\"v\":2}"), 2000).unwrap();
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
            record_in_tx(&tx, "t", "r1", HistoryOp::Create, Some("{}"), 1000).unwrap();
            // recorded 10 days ago
            let recent = 1000 + 30 * 86_400 + 1;
            record_in_tx(&tx, "t", "r1", HistoryOp::Update, Some("{\"a\":1}"), recent).unwrap();
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
    fn roll_moves_oldest_entries_into_archive_and_reads_through() {
        let conn = open_mem();
        // 10 versions, recorded_at = 1000, 1100, ... 1900.
        {
            let tx = conn.unchecked_transaction().unwrap();
            for i in 0..10_i64 {
                let body = format!("{{\"v\":{}}}", i + 1);
                record_in_tx(
                    &tx,
                    "t",
                    "r1",
                    HistoryOp::Update,
                    Some(&body),
                    1000 + i * 100,
                )
                .unwrap();
            }
            // keep=3, chunk_min=2: per-call roll is capped at chunk_min+1=3,
            // so draining the backlog takes two calls (1-3, then 4-6) and the
            // third is a no-op (4 raw <= keep+chunk_min).
            maybe_roll_into_archive(&tx, "t", "r1", 3, 2).unwrap();
            maybe_roll_into_archive(&tx, "t", "r1", 3, 2).unwrap();
            maybe_roll_into_archive(&tx, "t", "r1", 3, 2).unwrap();
            tx.commit().unwrap();
        }

        // Raw tier keeps the newest 4 (10 - 3 - 3; below threshold now).
        let raw_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _row_history", [], |r| r.get(0))
            .unwrap();
        assert_eq!(raw_count, 4);
        let chunks: Vec<(i64, i64, i64)> = conn
            .prepare(
                "SELECT version_start, version_end, entry_count \
                 FROM _row_history_archive ORDER BY version_start",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(chunks, vec![(1, 3, 3), (4, 6, 3)]);

        // list_versions returns the complete 1..=10 sequence, oldest first.
        let versions = list_versions(&conn, "t", "r1").unwrap();
        assert_eq!(versions.len(), 10);
        assert_eq!(
            versions.iter().map(|v| v.version).collect::<Vec<_>>(),
            (1..=10).collect::<Vec<i64>>()
        );
        // Archived entries are marked with id=0; raw entries keep real ids.
        assert!(versions[..6].iter().all(|v| v.id == 0));
        assert!(versions[6..].iter().all(|v| v.id > 0));

        // fetch_at inside the archived range reads through the chunk.
        let h = fetch_at(&conn, "t", "r1", 1450).unwrap().unwrap();
        assert_eq!(h.version, 5);
        assert_eq!(h.data_json.as_deref(), Some("{\"v\":5}"));
        // fetch_at in the raw range still hits the raw tier.
        let h = fetch_at(&conn, "t", "r1", 1900).unwrap().unwrap();
        assert_eq!(h.version, 10);
        assert!(h.id > 0);
        // Before any entry — None.
        assert!(fetch_at(&conn, "t", "r1", 999).unwrap().is_none());
    }

    #[test]
    fn roll_is_capped_per_call_for_backlog_drain() {
        let conn = open_mem();
        let tx = conn.unchecked_transaction().unwrap();
        // 100-entry backlog (pre-existing DB shape; the for_test helper skips
        // the automatic roll so the backlog actually accumulates), keep=3,
        // chunk_min=2.
        for i in 0..100_i64 {
            record_in_tx_for_test(
                &tx,
                "t",
                "r1",
                HistoryOp::Update,
                "{}",
                None,
                1000 + i,
                i + 1,
            )
            .unwrap();
        }
        maybe_roll_into_archive(&tx, "t", "r1", 3, 2).unwrap();
        tx.commit().unwrap();

        // One call rolls at most chunk_min+1=3 entries, not the whole backlog.
        let archived: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(entry_count), 0) FROM _row_history_archive",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(archived, 3);
        let raw: i64 = conn
            .query_row("SELECT COUNT(*) FROM _row_history", [], |r| r.get(0))
            .unwrap();
        assert_eq!(raw, 97);
    }

    #[test]
    fn version_stays_monotonic_across_roll() {
        let conn = open_mem();
        let tx = conn.unchecked_transaction().unwrap();
        for i in 0..10_i64 {
            record_in_tx(&tx, "t", "r1", HistoryOp::Update, Some("{}"), 1000 + i).unwrap();
        }
        maybe_roll_into_archive(&tx, "t", "r1", 3, 2).unwrap();
        // Next insert must continue at 11 even though raw MAX(version) sees
        // only the kept tail — the archive tier participates in MAX.
        record_in_tx(&tx, "t", "r1", HistoryOp::Update, Some("{}"), 2000).unwrap();
        tx.commit().unwrap();

        let versions = list_versions(&conn, "t", "r1").unwrap();
        assert_eq!(versions.last().unwrap().version, 11);
    }

    #[test]
    fn roll_below_threshold_is_noop() {
        let conn = open_mem();
        let tx = conn.unchecked_transaction().unwrap();
        for i in 0..5_i64 {
            record_in_tx(&tx, "t", "r1", HistoryOp::Update, Some("{}"), 1000 + i).unwrap();
        }
        maybe_roll_into_archive(&tx, "t", "r1", 3, 2).unwrap();
        tx.commit().unwrap();

        let archive_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _row_history_archive", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(archive_count, 0, "5 <= keep(3)+chunk_min(2) must not roll");
    }

    #[test]
    fn prev_data_json_is_not_written() {
        let conn = open_mem();
        let tx = conn.unchecked_transaction().unwrap();
        record_in_tx(&tx, "t", "r1", HistoryOp::Create, Some("{\"a\":1}"), 1000).unwrap();
        record_in_tx(&tx, "t", "r1", HistoryOp::Update, Some("{\"a\":2}"), 2000).unwrap();
        tx.commit().unwrap();

        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _row_history WHERE prev_data_json IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
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
