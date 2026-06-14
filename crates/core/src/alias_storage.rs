//! Global alias storage — single-source-of-truth for named queries that
//! span [`SourceSpec::Single`] / [`SourceSpec::Multi`] / [`SourceSpec::Pattern`]
//! table sources.
//!
//! # Phase 2 Storage Layout
//!
//! - **Project scope**: `<project_dir>/_global.db` (created on demand)
//! - **User scope**:    `<user_dir>/_global.db`    (created on demand)
//! - **Lookup precedence**: Project → User. A Project alias with the same
//!   `name` overrides the User alias of the same name.
//! - Both scopes are independent SQLite files sharing the
//!   [`CREATE_GLOBAL_ALIASES_SQL`] schema.
//!
//! `_global.db` is intentionally separate from per-table `<table>.db`
//! files so that:
//! 1. one alias can reference multiple tables (Multi / Pattern sources)
//!    without owning a per-table SoT;
//! 2. user-wide BP aliases survive project deletion;
//! 3. project-specific aliases override user defaults transparently.
//!
//! # Migration from Per-Table `_aliases`
//!
//! [`GlobalAliasStorage::migrate_from_per_table`] performs a lossless,
//! idempotent transfer of legacy 5-field rows from per-table `_aliases`
//! into the project-scope `_global_aliases`, with `sources` filled in as
//! `Single(<table_name>)` and `aggregator = None`. Rows already present
//! in project storage are skipped (`INSERT OR IGNORE` semantics) so the
//! migration may safely run on every registry open.
//!
//! See `crates/core/src/aggregator.rs` for the [`SourceSpec`] /
//! [`AliasAggregator`] primitives this storage persists.

use crate::aggregator::{AliasAggregator, SourceSpec};
use crate::error::MiniAppError;
use rusqlite::OptionalExtension;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Scope determines which `_global.db` (project or user) is written to.
/// Lookup (`alias_get` / `alias_list`) always reads both with project
/// taking precedence on name collisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasScope {
    /// Project-local `_global.db` (default for new aliases).
    Project,
    /// User-wide `_global.db` (shared across projects).
    User,
}

/// A single global alias record.
///
/// `filter` is stored verbatim — either a serialised
/// [`crate::filter::ListFilter`] JSON string, or a MiniJinja template
/// string when `params_schema` is `Some`. `sources` / `aggregator` are
/// serialised via `serde_json` at the storage boundary.
#[derive(Debug, Clone)]
pub struct AliasRecord {
    /// Alias name (PRIMARY KEY within each scope's `_global_aliases`).
    pub name: String,
    /// Source-table specifier (Single / Multi / Pattern).
    pub sources: SourceSpec,
    /// Optional aggregator (None means a plain `Rows` alias).
    pub aggregator: Option<AliasAggregator>,
    /// Serialised filter JSON or MiniJinja template string.
    pub filter: String,
    /// Optional default limit to apply when `alias_run` does not supply one.
    pub default_limit: Option<u32>,
    /// Optional human-readable description.
    pub description: Option<String>,
    /// Optional JSON array of parameter name strings (e.g. `["project","owner"]`).
    /// `None` means the alias takes no parameters.
    pub params_schema: Option<String>,
    /// Origin scope of this record after `alias_get` / `alias_list`.
    /// `None` for newly-constructed records that have not yet been
    /// persisted or loaded.
    pub scope: Option<AliasScope>,
}

impl AliasRecord {
    /// Construct a new record with `scope = None`. Persistence assigns
    /// the scope via [`GlobalAliasStorage::alias_create`].
    pub fn new(
        name: impl Into<String>,
        sources: SourceSpec,
        aggregator: Option<AliasAggregator>,
        filter: impl Into<String>,
        default_limit: Option<u32>,
        description: Option<String>,
        params_schema: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            sources,
            aggregator,
            filter: filter.into(),
            default_limit,
            description,
            params_schema,
            scope: None,
        }
    }
}

/// SQLite DDL for the global alias table. Stored once per `_global.db`
/// (project + user).
const CREATE_GLOBAL_ALIASES_SQL: &str = "
    CREATE TABLE IF NOT EXISTS _global_aliases (
        name            TEXT    PRIMARY KEY,
        sources_json    TEXT    NOT NULL,
        aggregator_json TEXT,
        filter          TEXT    NOT NULL,
        default_limit   INTEGER,
        description     TEXT,
        params_schema   TEXT
    )
";

/// Per-table legacy DDL — exposed for migration sites that re-create the
/// `_aliases` table on a fresh connection in tests.
pub const LEGACY_PER_TABLE_ALIASES_SQL: &str = "
    CREATE TABLE IF NOT EXISTS _aliases (
        name           TEXT    PRIMARY KEY,
        filter         TEXT    NOT NULL,
        default_limit  INTEGER,
        description    TEXT,
        params_schema  TEXT
    )
";

/// Tuple shape returned when scanning a per-table `_aliases` table during
/// [`GlobalAliasStorage::migrate_from_per_table`]. The columns map 1:1 to
/// the legacy 5-field schema (`name`, `filter`, `default_limit`,
/// `description`, `params_schema`).
type LegacyAliasRow = (String, String, Option<u32>, Option<String>, Option<String>);

/// Global alias storage handle. Holds at most two SQLite connections
/// (project + user), both wrapped in [`Arc<Mutex<_>>`] so the
/// `spawn_blocking` body can lock + execute without violating
/// [`Send`] / [`Sync`] bounds (rusqlite::Connection is `!Send`).
pub struct GlobalAliasStorage {
    project_conn: Option<Arc<Mutex<rusqlite::Connection>>>,
    user_conn: Option<Arc<Mutex<rusqlite::Connection>>>,
    project_path: Option<PathBuf>,
    user_path: Option<PathBuf>,
}

impl std::fmt::Debug for GlobalAliasStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // rusqlite::Connection is !Debug, so we project only the visible
        // metadata (path + scope presence) — sufficient for assert-output
        // diagnostics.
        f.debug_struct("GlobalAliasStorage")
            .field("project_path", &self.project_path)
            .field("user_path", &self.user_path)
            .field("project_mounted", &self.project_conn.is_some())
            .field("user_mounted", &self.user_conn.is_some())
            .finish()
    }
}

impl GlobalAliasStorage {
    /// Open both project and user `_global.db` files. Either argument may
    /// be `None` (scope skipped). At least one MUST be `Some`, otherwise
    /// the resulting storage has nothing to read or write.
    ///
    /// Each provided directory is created on demand. The `_global.db`
    /// file is created with the [`CREATE_GLOBAL_ALIASES_SQL`] schema if
    /// absent. Existing files are opened as-is (no DDL migration).
    ///
    /// # Errors
    /// - [`MiniAppError::Config`] when both arguments are `None`.
    /// - [`MiniAppError::Io`] when directory creation fails.
    /// - [`MiniAppError::Storage`] when SQLite open / DDL execute fails.
    pub fn open(project_dir: Option<&Path>, user_dir: Option<&Path>) -> Result<Self, MiniAppError> {
        if project_dir.is_none() && user_dir.is_none() {
            return Err(MiniAppError::Config(
                "GlobalAliasStorage::open requires at least one of project_dir / user_dir".into(),
            ));
        }
        let project = project_dir.map(open_scope_db).transpose()?;
        let user = user_dir.map(open_scope_db).transpose()?;
        Ok(Self {
            project_conn: project.as_ref().map(|(c, _)| Arc::clone(c)),
            user_conn: user.as_ref().map(|(c, _)| Arc::clone(c)),
            project_path: project.map(|(_, p)| p),
            user_path: user.map(|(_, p)| p),
        })
    }

    /// Open an in-memory storage for tests. Project scope only.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, MiniAppError> {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(CREATE_GLOBAL_ALIASES_SQL)?;
        Ok(Self {
            project_conn: Some(Arc::new(Mutex::new(conn))),
            user_conn: None,
            project_path: None,
            user_path: None,
        })
    }

    /// Returns the resolved `_global.db` path for a scope, or `None` if
    /// that scope is unmounted (or the storage was opened in-memory).
    pub fn path_for_scope(&self, scope: AliasScope) -> Option<&Path> {
        match scope {
            AliasScope::Project => self.project_path.as_deref(),
            AliasScope::User => self.user_path.as_deref(),
        }
    }

    fn conn_for_scope(
        &self,
        scope: AliasScope,
    ) -> Result<Arc<Mutex<rusqlite::Connection>>, MiniAppError> {
        let opt = match scope {
            AliasScope::Project => self.project_conn.as_ref(),
            AliasScope::User => self.user_conn.as_ref(),
        };
        opt.map(Arc::clone).ok_or_else(|| {
            MiniAppError::Config(format!("GlobalAliasStorage scope {scope:?} is not mounted"))
        })
    }

    /// Insert a new alias into the specified scope's storage.
    ///
    /// # Errors
    /// - [`MiniAppError::AliasAlreadyExists`] when an alias with the same
    ///   `name` already exists *in that scope* (cross-scope collisions are
    ///   permitted — project overrides user at lookup time).
    /// - [`MiniAppError::Config`] when the scope is unmounted.
    /// - [`MiniAppError::Storage`] on rusqlite failure.
    pub async fn alias_create(
        &self,
        scope: AliasScope,
        record: AliasRecord,
    ) -> Result<(), MiniAppError> {
        let conn = self.conn_for_scope(scope)?;
        let sources_json = serde_json::to_string(&record.sources).map_err(|e| {
            MiniAppError::Schema(format!(
                "serialise sources for alias '{}': {e}",
                record.name
            ))
        })?;
        let aggregator_json = match &record.aggregator {
            Some(agg) => Some(serde_json::to_string(agg).map_err(|e| {
                MiniAppError::Schema(format!(
                    "serialise aggregator for alias '{}': {e}",
                    record.name
                ))
            })?),
            None => None,
        };
        let name = record.name.clone();
        let filter = record.filter.clone();
        let default_limit = record.default_limit;
        let description = record.description.clone();
        let params_schema = record.params_schema.clone();
        tokio::task::spawn_blocking(move || -> Result<(), MiniAppError> {
            let conn = conn
                .lock()
                .map_err(|_| MiniAppError::Schema("mutex poisoned".into()))?;
            conn.execute(
                "INSERT OR IGNORE INTO _global_aliases \
                 (name, sources_json, aggregator_json, filter, default_limit, description, params_schema) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    name,
                    sources_json,
                    aggregator_json,
                    filter,
                    default_limit,
                    description,
                    params_schema,
                ],
            )?;
            if conn.changes() == 0 {
                return Err(MiniAppError::AliasAlreadyExists { name });
            }
            Ok(())
        })
        .await
        .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")))?
    }

    /// Get an alias by name. Project storage is consulted first; on miss
    /// the user storage is consulted. Returns [`MiniAppError::AliasNotFound`]
    /// when neither scope has the alias.
    pub async fn alias_get(&self, name: &str) -> Result<AliasRecord, MiniAppError> {
        if let Some(rec) = self.alias_get_scope(AliasScope::Project, name).await? {
            return Ok(rec);
        }
        if let Some(rec) = self.alias_get_scope(AliasScope::User, name).await? {
            return Ok(rec);
        }
        Err(MiniAppError::AliasNotFound {
            name: name.to_string(),
        })
    }

    /// Get an alias from a *specific* scope. Returns `Ok(None)` when the
    /// alias is absent (so the merged [`Self::alias_get`] can fall back
    /// to the next scope without distinguishing missing scope from
    /// missing row).
    pub async fn alias_get_scope(
        &self,
        scope: AliasScope,
        name: &str,
    ) -> Result<Option<AliasRecord>, MiniAppError> {
        let conn = match scope {
            AliasScope::Project => self.project_conn.as_ref(),
            AliasScope::User => self.user_conn.as_ref(),
        };
        let Some(conn) = conn.map(Arc::clone) else {
            return Ok(None);
        };
        let name_owned = name.to_string();
        tokio::task::spawn_blocking(move || -> Result<Option<AliasRecord>, MiniAppError> {
            let conn = conn
                .lock()
                .map_err(|_| MiniAppError::Schema("mutex poisoned".into()))?;
            let mut stmt = conn.prepare(
                "SELECT name, sources_json, aggregator_json, filter, default_limit, description, params_schema \
                 FROM _global_aliases WHERE name = ?1",
            )?;
            let row = stmt
                .query_row(rusqlite::params![name_owned], extract_row)
                .optional()?;
            match row {
                Some(mut rec) => {
                    rec.scope = Some(scope);
                    Ok(Some(rec))
                }
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")))?
    }

    /// List all aliases across both scopes, sorted ascending by name.
    /// On name collision the Project entry is retained; the User entry
    /// is silently discarded (precedence rule).
    pub async fn alias_list(&self) -> Result<Vec<AliasRecord>, MiniAppError> {
        let project = match self.project_conn.as_ref() {
            Some(c) => list_scope(Arc::clone(c), AliasScope::Project).await?,
            None => Vec::new(),
        };
        let user = match self.user_conn.as_ref() {
            Some(c) => list_scope(Arc::clone(c), AliasScope::User).await?,
            None => Vec::new(),
        };
        let mut merged: std::collections::BTreeMap<String, AliasRecord> =
            std::collections::BTreeMap::new();
        // Insert user first, then project — project entries overwrite on
        // collision, satisfying the precedence rule.
        for rec in user {
            merged.insert(rec.name.clone(), rec);
        }
        for rec in project {
            merged.insert(rec.name.clone(), rec);
        }
        Ok(merged.into_values().collect())
    }

    /// Delete an alias from a specific scope.
    ///
    /// # Errors
    /// - [`MiniAppError::AliasNotFound`] when the alias is absent in that
    ///   scope.
    /// - [`MiniAppError::Config`] when the scope is unmounted.
    /// - [`MiniAppError::Storage`] on rusqlite failure.
    pub async fn alias_delete(&self, scope: AliasScope, name: &str) -> Result<(), MiniAppError> {
        let conn = self.conn_for_scope(scope)?;
        let name_owned = name.to_string();
        tokio::task::spawn_blocking(move || -> Result<(), MiniAppError> {
            let conn = conn
                .lock()
                .map_err(|_| MiniAppError::Schema("mutex poisoned".into()))?;
            let affected = conn.execute(
                "DELETE FROM _global_aliases WHERE name = ?1",
                rusqlite::params![name_owned],
            )?;
            if affected == 0 {
                return Err(MiniAppError::AliasNotFound { name: name_owned });
            }
            Ok(())
        })
        .await
        .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")))?
    }

    /// Lossless, idempotent migration of legacy per-table `_aliases` rows
    /// into a chosen scope's `_global_aliases`.
    ///
    /// For each `(table_name, per_table_conn)` pair, every row from
    /// the per-table `_aliases` table is loaded and inserted into
    /// `target_scope` storage with `sources = Single(<table_name>)` and
    /// `aggregator = None`. Rows whose `name` already exists in that
    /// scope are skipped (`INSERT OR IGNORE`), so the migration may
    /// safely run on every registry open.
    ///
    /// Returns the number of rows newly written (skipped collisions are
    /// not counted).
    ///
    /// # Errors
    /// - [`MiniAppError::Config`] when `target_scope` is unmounted.
    /// - [`MiniAppError::Storage`] on rusqlite failure (per-table read
    ///   or destination insert).
    pub async fn migrate_from_per_table(
        &self,
        target_scope: AliasScope,
        per_table: Vec<(String, Arc<Mutex<rusqlite::Connection>>)>,
    ) -> Result<usize, MiniAppError> {
        let dest = self.conn_for_scope(target_scope).map_err(|_| {
            MiniAppError::Config(format!(
                "GlobalAliasStorage::migrate_from_per_table requires {target_scope:?} scope to be mounted"
            ))
        })?;
        tokio::task::spawn_blocking(move || -> Result<usize, MiniAppError> {
            let mut migrated = 0usize;
            for (table_name, src_conn) in per_table {
                let rows: Vec<LegacyAliasRow> = {
                    let src = src_conn
                        .lock()
                        .map_err(|_| MiniAppError::Schema("source mutex poisoned".into()))?;
                    let mut stmt = src.prepare(
                        "SELECT name, filter, default_limit, description, params_schema \
                         FROM _aliases ORDER BY name ASC",
                    )?;
                    stmt.query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<u32>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, Option<String>>(4)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?
                };
                if rows.is_empty() {
                    continue;
                }
                let sources_json = serde_json::to_string(&SourceSpec::Single(table_name.clone()))
                    .map_err(|e| {
                    MiniAppError::Schema(format!(
                        "serialise Single source for table '{table_name}' during migration: {e}"
                    ))
                })?;
                let dst = dest
                    .lock()
                    .map_err(|_| MiniAppError::Schema("dest mutex poisoned".into()))?;
                for (name, filter, default_limit, description, params_schema) in rows {
                    dst.execute(
                        "INSERT OR IGNORE INTO _global_aliases \
                         (name, sources_json, aggregator_json, filter, default_limit, description, params_schema) \
                         VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6)",
                        rusqlite::params![
                            name,
                            sources_json,
                            filter,
                            default_limit,
                            description,
                            params_schema,
                        ],
                    )?;
                    if dst.changes() > 0 {
                        migrated += 1;
                    }
                }
            }
            Ok(migrated)
        })
        .await
        .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")))?
    }
}

fn open_scope_db(dir: &Path) -> Result<(Arc<Mutex<rusqlite::Connection>>, PathBuf), MiniAppError> {
    std::fs::create_dir_all(dir)?;
    let db_path = dir.join("_global.db");
    let conn = rusqlite::Connection::open(&db_path)?;
    // Enable WAL journal mode so concurrent connections to the same
    // `_global.db` file (e.g. across `rebuild_registry` ArcSwap windows
    // when the previous storage handle is still held by in-flight
    // tasks) do not serialise on SQLITE_BUSY. Mirrors `Store::open`'s
    // WAL setup for dual-registry safety. The PRAGMA returns a single
    // "wal" row; rusqlite errors are propagated as `MiniAppError::Storage`.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute_batch(CREATE_GLOBAL_ALIASES_SQL)?;
    Ok((Arc::new(Mutex::new(conn)), db_path))
}

fn extract_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AliasRecord> {
    let name: String = row.get(0)?;
    let sources_json: String = row.get(1)?;
    let aggregator_json: Option<String> = row.get(2)?;
    let filter: String = row.get(3)?;
    let default_limit: Option<u32> = row.get(4)?;
    let description: Option<String> = row.get(5)?;
    let params_schema: Option<String> = row.get(6)?;
    let sources: SourceSpec = serde_json::from_str(&sources_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(format!(
                "deserialise sources_json: {e}"
            ))),
        )
    })?;
    let aggregator: Option<AliasAggregator> = match aggregator_json {
        Some(s) => Some(serde_json::from_str(&s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other(format!(
                    "deserialise aggregator_json: {e}"
                ))),
            )
        })?),
        None => None,
    };
    Ok(AliasRecord {
        name,
        sources,
        aggregator,
        filter,
        default_limit,
        description,
        params_schema,
        scope: None,
    })
}

async fn list_scope(
    conn: Arc<Mutex<rusqlite::Connection>>,
    scope: AliasScope,
) -> Result<Vec<AliasRecord>, MiniAppError> {
    tokio::task::spawn_blocking(move || -> Result<Vec<AliasRecord>, MiniAppError> {
        let conn = conn
            .lock()
            .map_err(|_| MiniAppError::Schema("mutex poisoned".into()))?;
        let mut stmt = conn.prepare(
            "SELECT name, sources_json, aggregator_json, filter, default_limit, description, params_schema \
             FROM _global_aliases ORDER BY name ASC",
        )?;
        let rows = stmt
            .query_map([], extract_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .map(|mut r| {
                r.scope = Some(scope);
                r
            })
            .collect())
    })
    .await
    .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")))?
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator::AliasAggregator;
    use tempfile::TempDir;

    fn sample_record(name: &str) -> AliasRecord {
        AliasRecord::new(
            name,
            SourceSpec::Single("rows".into()),
            None,
            r#"{"type":"eq","field":"status","value":"open"}"#,
            Some(20),
            Some("sample".into()),
            None,
        )
    }

    #[tokio::test]
    async fn create_get_roundtrip_in_memory() {
        let storage = GlobalAliasStorage::open_in_memory().unwrap();
        storage
            .alias_create(AliasScope::Project, sample_record("foo"))
            .await
            .unwrap();
        let got = storage.alias_get("foo").await.unwrap();
        assert_eq!(got.name, "foo");
        assert!(matches!(got.sources, SourceSpec::Single(ref t) if t == "rows"));
        assert!(got.aggregator.is_none());
        assert_eq!(got.default_limit, Some(20));
        assert_eq!(got.description.as_deref(), Some("sample"));
        assert_eq!(got.scope, Some(AliasScope::Project));
    }

    #[tokio::test]
    async fn create_persists_sources_multi_and_aggregator_groupby() {
        let storage = GlobalAliasStorage::open_in_memory().unwrap();
        let rec = AliasRecord::new(
            "by_tag",
            SourceSpec::Multi(vec!["a".into(), "b".into()]),
            Some(AliasAggregator::GroupBy {
                by_field: "tag".into(),
                having: None,
                inner: Some(Box::new(AliasAggregator::Sum {
                    field: "value".into(),
                })),
            }),
            "{}".to_string(),
            None,
            None,
            None,
        );
        storage
            .alias_create(AliasScope::Project, rec)
            .await
            .unwrap();
        let got = storage.alias_get("by_tag").await.unwrap();
        match got.sources {
            SourceSpec::Multi(v) => assert_eq!(v, vec!["a".to_string(), "b".to_string()]),
            other => panic!("expected Multi, got {other:?}"),
        }
        match got.aggregator {
            Some(AliasAggregator::GroupBy {
                by_field,
                inner: Some(inner),
                ..
            }) => {
                assert_eq!(by_field, "tag");
                assert!(matches!(*inner, AliasAggregator::Sum { ref field } if field == "value"));
            }
            other => panic!("expected GroupBy+Sum, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_persists_pattern_source() {
        let storage = GlobalAliasStorage::open_in_memory().unwrap();
        let rec = AliasRecord::new(
            "shi_all",
            SourceSpec::Pattern("shi_*".into()),
            Some(AliasAggregator::Count),
            "{}".to_string(),
            None,
            None,
            None,
        );
        storage
            .alias_create(AliasScope::Project, rec)
            .await
            .unwrap();
        let got = storage.alias_get("shi_all").await.unwrap();
        match got.sources {
            SourceSpec::Pattern(p) => assert_eq!(p, "shi_*"),
            other => panic!("expected Pattern, got {other:?}"),
        }
        assert!(matches!(got.aggregator, Some(AliasAggregator::Count)));
    }

    #[tokio::test]
    async fn create_duplicate_returns_already_exists() {
        let storage = GlobalAliasStorage::open_in_memory().unwrap();
        storage
            .alias_create(AliasScope::Project, sample_record("dup"))
            .await
            .unwrap();
        let err = storage
            .alias_create(AliasScope::Project, sample_record("dup"))
            .await
            .expect_err("expected AliasAlreadyExists");
        assert_eq!(err.code(), crate::error::codes::ALIAS_ALREADY_EXISTS);
    }

    #[tokio::test]
    async fn get_unknown_returns_not_found() {
        let storage = GlobalAliasStorage::open_in_memory().unwrap();
        let err = storage
            .alias_get("nope")
            .await
            .expect_err("expected AliasNotFound");
        assert_eq!(err.code(), crate::error::codes::ALIAS_NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_round_trip_then_not_found() {
        let storage = GlobalAliasStorage::open_in_memory().unwrap();
        storage
            .alias_create(AliasScope::Project, sample_record("to_delete"))
            .await
            .unwrap();
        storage
            .alias_delete(AliasScope::Project, "to_delete")
            .await
            .unwrap();
        let err = storage
            .alias_delete(AliasScope::Project, "to_delete")
            .await
            .expect_err("second delete should fail");
        assert_eq!(err.code(), crate::error::codes::ALIAS_NOT_FOUND);
    }

    #[tokio::test]
    async fn list_returns_sorted_ascending_by_name() {
        let storage = GlobalAliasStorage::open_in_memory().unwrap();
        for n in ["c", "a", "b"] {
            storage
                .alias_create(AliasScope::Project, sample_record(n))
                .await
                .unwrap();
        }
        let names: Vec<String> = storage
            .alias_list()
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn project_overrides_user_on_name_collision() {
        let project_dir = TempDir::new().unwrap();
        let user_dir = TempDir::new().unwrap();
        let storage =
            GlobalAliasStorage::open(Some(project_dir.path()), Some(user_dir.path())).unwrap();
        let mut user_rec = sample_record("shared");
        user_rec.description = Some("user-version".into());
        storage
            .alias_create(AliasScope::User, user_rec)
            .await
            .unwrap();
        let mut project_rec = sample_record("shared");
        project_rec.description = Some("project-version".into());
        storage
            .alias_create(AliasScope::Project, project_rec)
            .await
            .unwrap();

        // alias_get → project takes precedence
        let got = storage.alias_get("shared").await.unwrap();
        assert_eq!(got.description.as_deref(), Some("project-version"));
        assert_eq!(got.scope, Some(AliasScope::Project));

        // alias_list → merged, project wins for collisions, 1 row
        let all = storage.alias_list().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].description.as_deref(), Some("project-version"));
        assert_eq!(all[0].scope, Some(AliasScope::Project));
    }

    #[tokio::test]
    async fn user_only_alias_returned_when_no_project_collision() {
        let project_dir = TempDir::new().unwrap();
        let user_dir = TempDir::new().unwrap();
        let storage =
            GlobalAliasStorage::open(Some(project_dir.path()), Some(user_dir.path())).unwrap();
        let user_only = sample_record("user_only");
        storage
            .alias_create(AliasScope::User, user_only)
            .await
            .unwrap();
        let got = storage.alias_get("user_only").await.unwrap();
        assert_eq!(got.scope, Some(AliasScope::User));
    }

    #[tokio::test]
    async fn open_persists_across_reopen() {
        let project_dir = TempDir::new().unwrap();
        {
            let storage = GlobalAliasStorage::open(Some(project_dir.path()), None).unwrap();
            storage
                .alias_create(AliasScope::Project, sample_record("persisted"))
                .await
                .unwrap();
        }
        let reopened = GlobalAliasStorage::open(Some(project_dir.path()), None).unwrap();
        let got = reopened.alias_get("persisted").await.unwrap();
        assert_eq!(got.name, "persisted");
    }

    #[tokio::test]
    async fn open_requires_at_least_one_scope() {
        let err = GlobalAliasStorage::open(None, None)
            .expect_err("expected Config error when both dirs are None");
        assert_eq!(err.code(), crate::error::codes::CONFIG_ERROR);
    }

    #[tokio::test]
    async fn migrate_from_per_table_lossless_roundtrip() {
        // Set up two per-table _aliases tables (in-memory) with 2 + 1 rows.
        let conn_a = rusqlite::Connection::open_in_memory().unwrap();
        conn_a.execute_batch(LEGACY_PER_TABLE_ALIASES_SQL).unwrap();
        conn_a
            .execute(
                "INSERT INTO _aliases (name, filter, default_limit, description, params_schema) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params!["a_open", "{}", 50i64, "alpha", Option::<String>::None],
            )
            .unwrap();
        conn_a
            .execute(
                "INSERT INTO _aliases (name, filter, default_limit, description, params_schema) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    "a_closed",
                    "{}",
                    Option::<i64>::None,
                    Option::<String>::None,
                    Some("[\"x\"]".to_string())
                ],
            )
            .unwrap();

        let conn_b = rusqlite::Connection::open_in_memory().unwrap();
        conn_b.execute_batch(LEGACY_PER_TABLE_ALIASES_SQL).unwrap();
        conn_b
            .execute(
                "INSERT INTO _aliases (name, filter, default_limit, description, params_schema) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params!["b_recent", "{}", 10i64, "bravo", Option::<String>::None],
            )
            .unwrap();

        let storage = GlobalAliasStorage::open_in_memory().unwrap();
        let migrated = storage
            .migrate_from_per_table(
                AliasScope::Project,
                vec![
                    ("table_a".to_string(), Arc::new(Mutex::new(conn_a))),
                    ("table_b".to_string(), Arc::new(Mutex::new(conn_b))),
                ],
            )
            .await
            .unwrap();
        assert_eq!(migrated, 3);

        // Verify lossless: all 3 rows present in project storage, with
        // sources=Single(<table>) embedded.
        let all = storage.alias_list().await.unwrap();
        assert_eq!(all.len(), 3);
        let a_open = all.iter().find(|r| r.name == "a_open").unwrap();
        assert!(matches!(a_open.sources, SourceSpec::Single(ref t) if t == "table_a"));
        assert!(a_open.aggregator.is_none());
        assert_eq!(a_open.default_limit, Some(50));
        assert_eq!(a_open.description.as_deref(), Some("alpha"));
        assert_eq!(a_open.params_schema, None);

        let a_closed = all.iter().find(|r| r.name == "a_closed").unwrap();
        assert!(matches!(a_closed.sources, SourceSpec::Single(ref t) if t == "table_a"));
        assert_eq!(a_closed.params_schema.as_deref(), Some("[\"x\"]"));

        let b_recent = all.iter().find(|r| r.name == "b_recent").unwrap();
        assert!(matches!(b_recent.sources, SourceSpec::Single(ref t) if t == "table_b"));
        assert_eq!(b_recent.default_limit, Some(10));
    }

    #[tokio::test]
    async fn migrate_from_per_table_idempotent_on_second_run() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(LEGACY_PER_TABLE_ALIASES_SQL).unwrap();
        conn.execute(
            "INSERT INTO _aliases (name, filter, default_limit, description, params_schema) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "x",
                "{}",
                Option::<i64>::None,
                Option::<String>::None,
                Option::<String>::None
            ],
        )
        .unwrap();
        let conn_arc = Arc::new(Mutex::new(conn));

        let storage = GlobalAliasStorage::open_in_memory().unwrap();
        let first = storage
            .migrate_from_per_table(
                AliasScope::Project,
                vec![("t".to_string(), Arc::clone(&conn_arc))],
            )
            .await
            .unwrap();
        let second = storage
            .migrate_from_per_table(
                AliasScope::Project,
                vec![("t".to_string(), Arc::clone(&conn_arc))],
            )
            .await
            .unwrap();
        assert_eq!(first, 1);
        assert_eq!(second, 0);
        let all = storage.alias_list().await.unwrap();
        assert_eq!(all.len(), 1);
    }

    #[tokio::test]
    async fn migrate_from_per_table_skips_collision_with_existing_global() {
        // Project storage already has an alias named "shared" — migration
        // must not overwrite it even when a per-table row of the same
        // name appears in the migration input.
        let storage = GlobalAliasStorage::open_in_memory().unwrap();
        let mut existing = sample_record("shared");
        existing.description = Some("existing-global".into());
        storage
            .alias_create(AliasScope::Project, existing)
            .await
            .unwrap();

        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(LEGACY_PER_TABLE_ALIASES_SQL).unwrap();
        conn.execute(
            "INSERT INTO _aliases (name, filter, default_limit, description, params_schema) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "shared",
                "{}",
                Option::<i64>::None,
                Some("legacy-per-table".to_string()),
                Option::<String>::None
            ],
        )
        .unwrap();
        let migrated = storage
            .migrate_from_per_table(
                AliasScope::Project,
                vec![("ignored_table".to_string(), Arc::new(Mutex::new(conn)))],
            )
            .await
            .unwrap();
        assert_eq!(migrated, 0);
        let got = storage.alias_get("shared").await.unwrap();
        assert_eq!(got.description.as_deref(), Some("existing-global"));
    }
}
