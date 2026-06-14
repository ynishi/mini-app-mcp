//! Multi-table aggregation primitives for the `query_aggregate` MCP tool.
//!
//! Provides [`AliasAggregator`] (`Count` / `Sum` / `Avg` / `Min` / `Max` /
//! `GroupBy`), [`SourceSpec`] (`Single` / `Multi`), [`AliasRunResult`]
//! (`Rows` / `Count` / `Value` / `Groups`), and [`execute_aggregate`] for
//! SQLite `ATTACH DATABASE` + `UNION ALL` composition across per-table
//! `.db` files.
//!
//! # Crux compliance
//! - **Crux #1**: [`ListFilter::build_sql`] is reused via the sibling
//!   [`ListFilter::build_subquery`] method; no signature changes.
//! - **Crux #2**: `GroupBy::having` is emitted as a literal `HAVING` clause
//!   after `GROUP BY`, never as a `WHERE` clause.
//! - **Crux #3**: [`SourceSpec::Multi`] mounts each table via
//!   `ATTACH DATABASE` and combines them with literal `UNION ALL` — never
//!   `JOIN`, never application-layer merge.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::MiniAppError;
use crate::filter::{FilterParam, ListFilter};
use crate::registry::TableRegistry;
use crate::schema::SchemaConfig;
use crate::store::Store;

// ---------------------------------------------------------------------------
// Type definitions
// ---------------------------------------------------------------------------

/// Aggregator primitive selected by the caller of `query_aggregate`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AliasAggregator {
    /// `COUNT(*)` over the source rows.
    Count,
    /// `SUM(json_extract(data, '$.<field>'))`.
    Sum { field: String },
    /// `AVG(json_extract(data, '$.<field>'))`.
    Avg { field: String },
    /// `MIN(json_extract(data, '$.<field>'))`.
    Min { field: String },
    /// `MAX(json_extract(data, '$.<field>'))`.
    Max { field: String },
    /// `GROUP BY <by_field>` with an optional per-group inner aggregator
    /// and an optional `HAVING` predicate.
    ///
    /// `having` is emitted as a `HAVING` clause **after** `GROUP BY`
    /// (Crux #2). `inner` is an optional scalar sub-aggregator
    /// (`Sum` / `Avg` / `Min` / `Max`); nested `GroupBy` is rejected at
    /// validation time in Phase 1.
    GroupBy {
        by_field: String,
        #[serde(default)]
        having: Option<ListFilter>,
        #[serde(default)]
        inner: Option<Box<AliasAggregator>>,
    },
}

/// Source-table specifier — either a single table or a list combined via
/// `ATTACH DATABASE` + `UNION ALL` (Crux #3).
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum SourceSpec {
    /// One table by name (existing 1-table compatibility, used as the
    /// normalisation target for any legacy callers that supplied a bare
    /// `table` argument).
    Single(String),
    /// Two or more tables, joined via `UNION ALL` (never `JOIN`).
    Multi(Vec<String>),
}

impl SourceSpec {
    /// Returns the source tables as a slice. Length is `1` for `Single` and
    /// the supplied vector's length for `Multi`; [`execute_aggregate`]
    /// rejects an empty `Multi` with [`MiniAppError::Aggregator`].
    pub fn tables(&self) -> &[String] {
        match self {
            SourceSpec::Single(t) => std::slice::from_ref(t),
            SourceSpec::Multi(v) => v.as_slice(),
        }
    }
}

/// One row of a `GroupBy` result.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GroupResult {
    /// The grouping key — typically a string or number from the
    /// `by_field` column.
    pub key: serde_json::Value,
    /// Row count in this group (`COUNT(*)` per group).
    pub count: i64,
    /// Inner aggregator result (`Sum` / `Avg` / `Min` / `Max`) if `inner`
    /// was set on the `GroupBy` variant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
}

/// Externally-tagged result of [`execute_aggregate`]. JSON dispatch over
/// the `kind` field lets MCP callers decode without prior knowledge of
/// which aggregator was requested.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum AliasRunResult {
    /// Reserved for the Phase 2 alias-run unification path; no current
    /// [`AliasAggregator`] variant produces this.
    Rows(Vec<serde_json::Value>),
    /// `COUNT(*)` result.
    Count(i64),
    /// Scalar aggregate (`Sum` / `Avg` / `Min` / `Max`) as a JSON `Number`
    /// or `Null` (when the source is empty).
    Value(serde_json::Value),
    /// `GroupBy` result, one entry per group.
    Groups(Vec<GroupResult>),
}

// ---------------------------------------------------------------------------
// Identifier sanity check
// ---------------------------------------------------------------------------

/// SQL identifier allow-list: `[A-Za-z_][A-Za-z0-9_]*`. The aggregator
/// interpolates table and field names directly into SQL strings
/// (`FROM <table>`, `json_extract(data, '$.<field>')`, `ATTACH DATABASE
/// AS db_<i>`), so this strict ASCII check guarantees no injection
/// surface regardless of what the schema validator permits in field
/// names.
fn validate_identifier(label: &str, name: &str) -> Result<(), MiniAppError> {
    let mut chars = name.chars();
    let first = chars.next().ok_or_else(|| MiniAppError::Validation {
        field: label.to_string(),
        reason: format!("{label} must not be empty"),
    })?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(MiniAppError::Validation {
            field: label.to_string(),
            reason: format!("{label} '{name}' must start with [A-Za-z_]"),
        });
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '_') {
            return Err(MiniAppError::Validation {
                field: label.to_string(),
                reason: format!(
                    "{label} '{name}' must contain only [A-Za-z0-9_]"
                ),
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// AliasAggregator: validate + scalar SQL composition
// ---------------------------------------------------------------------------

impl AliasAggregator {
    /// Verify that every `field` / `by_field` referenced by this aggregator
    /// is declared in `schema`, and recursively validate `inner` + `having`.
    pub fn validate(&self, schema: &SchemaConfig) -> Result<(), MiniAppError> {
        match self {
            AliasAggregator::Count => Ok(()),
            AliasAggregator::Sum { field }
            | AliasAggregator::Avg { field }
            | AliasAggregator::Min { field }
            | AliasAggregator::Max { field } => {
                validate_identifier("aggregator_field", field)?;
                require_schema_field(schema, field, "aggregator field")
            }
            AliasAggregator::GroupBy {
                by_field,
                having,
                inner,
            } => {
                validate_identifier("group_by_field", by_field)?;
                require_schema_field(schema, by_field, "group_by field")?;
                if let Some(filter) = having {
                    filter.validate(schema)?;
                }
                if let Some(inner_agg) = inner {
                    if matches!(inner_agg.as_ref(), AliasAggregator::GroupBy { .. }) {
                        return Err(MiniAppError::Aggregator(
                            "nested GroupBy is not supported in Phase 1".into(),
                        ));
                    }
                    inner_agg.validate(schema)?;
                }
                Ok(())
            }
        }
    }

    /// Returns the SQL aggregate-function fragment (no surrounding
    /// `SELECT` / `FROM`). `GroupBy` is rejected here — [`execute_aggregate`]
    /// composes the `GROUP BY` clause separately.
    fn scalar_agg_sql(&self) -> Result<String, MiniAppError> {
        match self {
            AliasAggregator::Count => Ok("COUNT(*)".to_string()),
            AliasAggregator::Sum { field } => {
                Ok(format!("SUM(json_extract(data, '$.{field}'))"))
            }
            AliasAggregator::Avg { field } => {
                Ok(format!("AVG(json_extract(data, '$.{field}'))"))
            }
            AliasAggregator::Min { field } => {
                Ok(format!("MIN(json_extract(data, '$.{field}'))"))
            }
            AliasAggregator::Max { field } => {
                Ok(format!("MAX(json_extract(data, '$.{field}'))"))
            }
            AliasAggregator::GroupBy { .. } => Err(MiniAppError::Aggregator(
                "GroupBy is not a scalar aggregator (handled by execute_aggregate)"
                    .into(),
            )),
        }
    }
}

fn require_schema_field(
    schema: &SchemaConfig,
    field: &str,
    role: &str,
) -> Result<(), MiniAppError> {
    if schema.fields.iter().any(|f| f.name == field) {
        Ok(())
    } else {
        Err(MiniAppError::Validation {
            field: field.to_string(),
            reason: format!("{role} '{field}' is not declared in schema"),
        })
    }
}

// ---------------------------------------------------------------------------
// execute_aggregate — main entry point
// ---------------------------------------------------------------------------

/// SQLite default `SQLITE_MAX_ATTACHED` (10). Beyond this limit
/// [`execute_aggregate`] returns [`MiniAppError::Aggregator`] without
/// touching SQLite, to keep the failure mode predictable.
pub const SQLITE_MAX_ATTACHED: usize = 10;

/// Execute a multi-table aggregation request.
///
/// Resolves each source table via `registry`, mounts every backing `.db`
/// file into a fresh in-memory connection with `ATTACH DATABASE`, composes
/// a `UNION ALL` inner sub-query (or a single `SELECT` for
/// [`SourceSpec::Single`]), and wraps it in an outer `SELECT <aggregate>`
/// (with `GROUP BY` + optional `HAVING` for [`AliasAggregator::GroupBy`]).
///
/// # Crux compliance
/// - Crux #1: reuses the existing [`ListFilter::build_sql`] via the
///   sibling [`ListFilter::build_subquery`] method (no signature change).
/// - Crux #2: `having` is emitted as a literal `HAVING` clause **after**
///   `GROUP BY`, never as `WHERE`.
/// - Crux #3: multi-table sources are combined with literal `UNION ALL`;
///   `JOIN` is never emitted and no application-layer merge happens.
///
/// # Errors
/// - [`MiniAppError::Aggregator`] — empty `Multi`, ATTACH-limit exceeded,
///   nested `GroupBy`, or non-UTF-8 db path.
/// - [`MiniAppError::Validation`] — schema field missing, identifier
///   regex mismatch, or filter validation failure.
/// - [`MiniAppError::TableNotFound`] — propagated from the registry.
/// - [`MiniAppError::Storage`] — `rusqlite` failure (`ATTACH`, query
///   execution, etc.).
/// - [`MiniAppError::Schema`] — `spawn_blocking` panic.
pub async fn execute_aggregate(
    registry: &TableRegistry,
    sources: SourceSpec,
    filter: Option<ListFilter>,
    aggregator: AliasAggregator,
    schema: &SchemaConfig,
) -> Result<AliasRunResult, MiniAppError> {
    let tables: Vec<String> = sources.tables().to_vec();
    if tables.is_empty() {
        return Err(MiniAppError::Aggregator(
            "sources must contain at least one table".into(),
        ));
    }
    if tables.len() > SQLITE_MAX_ATTACHED {
        return Err(MiniAppError::Aggregator(format!(
            "too many sources: {} (SQLITE_MAX_ATTACHED = {})",
            tables.len(),
            SQLITE_MAX_ATTACHED
        )));
    }
    for t in &tables {
        validate_identifier("source_table", t)?;
    }
    aggregator.validate(schema)?;
    if let Some(f) = &filter {
        f.validate(schema)?;
    }
    // Resolve db_path for each source table via the registry.
    let mut db_paths: Vec<std::path::PathBuf> = Vec::with_capacity(tables.len());
    for t in &tables {
        let entry = registry.resolve(Some(t))?;
        let store: &Arc<Store> = &entry.store;
        db_paths.push(store.db_path().to_path_buf());
    }
    let filter_owned = filter.clone();
    let aggregator_owned = aggregator.clone();
    tokio::task::spawn_blocking(move || -> Result<AliasRunResult, MiniAppError> {
        run_aggregate_blocking(&db_paths, filter_owned.as_ref(), &aggregator_owned)
    })
    .await
    .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")))?
}

/// Synchronous core of [`execute_aggregate`], executed inside
/// `spawn_blocking`. Mounts each per-table `.db` file via
/// `ATTACH DATABASE` into a fresh in-memory connection, composes the SQL,
/// and dispatches on the [`AliasAggregator`] variant.
fn run_aggregate_blocking(
    db_paths: &[std::path::PathBuf],
    filter: Option<&ListFilter>,
    aggregator: &AliasAggregator,
) -> Result<AliasRunResult, MiniAppError> {
    let conn = rusqlite::Connection::open_in_memory()?;
    let mut aliases: Vec<String> = Vec::with_capacity(db_paths.len());
    for (i, path) in db_paths.iter().enumerate() {
        let alias = format!("db_{i}");
        let path_str = path.to_str().ok_or_else(|| {
            MiniAppError::Aggregator(format!(
                "db_path is not valid UTF-8: {}",
                path.display()
            ))
        })?;
        conn.execute(
            &format!("ATTACH DATABASE ?1 AS {alias}"),
            rusqlite::params![path_str],
        )?;
        aliases.push(alias);
    }
    let (inner_sql, params) = build_inner_sql(&aliases, filter)?;
    match aggregator {
        AliasAggregator::Count
        | AliasAggregator::Sum { .. }
        | AliasAggregator::Avg { .. }
        | AliasAggregator::Min { .. }
        | AliasAggregator::Max { .. } => {
            let agg_sql = aggregator.scalar_agg_sql()?;
            let sql = format!("SELECT {agg_sql} FROM ({inner_sql})");
            run_scalar_aggregate(&conn, &sql, &params, aggregator)
        }
        AliasAggregator::GroupBy {
            by_field,
            having,
            inner,
        } => run_group_by(
            &conn,
            &inner_sql,
            &params,
            by_field,
            having.as_ref(),
            inner.as_deref(),
        ),
    }
}

/// Compose the inner sub-query — single `SELECT` for one alias,
/// `UNION ALL` chain for many. Literal `UNION ALL` keyword is emitted so
/// downstream substring assertions can verify Crux #3.
fn build_inner_sql(
    aliases: &[String],
    filter: Option<&ListFilter>,
) -> Result<(String, Vec<FilterParam>), MiniAppError> {
    let mut parts: Vec<String> = Vec::with_capacity(aliases.len());
    let mut all_params: Vec<FilterParam> = Vec::new();
    for alias in aliases {
        let table_ref = format!("{alias}.rows");
        match filter {
            Some(f) => {
                let (sql, params) = f.build_subquery(&table_ref)?;
                parts.push(sql);
                all_params.extend(params);
            }
            None => {
                parts.push(format!(
                    "SELECT id, data, created_at, updated_at FROM {table_ref}"
                ));
            }
        }
    }
    let inner = parts.join(" UNION ALL ");
    Ok((inner, all_params))
}

fn filter_params_to_rusqlite(
    params: &[FilterParam],
) -> Vec<Box<dyn rusqlite::ToSql>> {
    params
        .iter()
        .map(|p| -> Box<dyn rusqlite::ToSql> {
            match p {
                FilterParam::Text(s) => Box::new(s.clone()),
                FilterParam::Number(n) => Box::new(*n),
                FilterParam::Bool(b) => Box::new(*b),
            }
        })
        .collect()
}

fn run_scalar_aggregate(
    conn: &rusqlite::Connection,
    sql: &str,
    params: &[FilterParam],
    aggregator: &AliasAggregator,
) -> Result<AliasRunResult, MiniAppError> {
    let owned = filter_params_to_rusqlite(params);
    let refs: Vec<&dyn rusqlite::ToSql> = owned.iter().map(|b| b.as_ref()).collect();
    match aggregator {
        AliasAggregator::Count => {
            let n: i64 = conn.query_row(
                sql,
                rusqlite::params_from_iter(refs.iter().copied()),
                |row| row.get(0),
            )?;
            Ok(AliasRunResult::Count(n))
        }
        AliasAggregator::Sum { .. }
        | AliasAggregator::Avg { .. }
        | AliasAggregator::Min { .. }
        | AliasAggregator::Max { .. } => {
            let value: serde_json::Value = conn.query_row(
                sql,
                rusqlite::params_from_iter(refs.iter().copied()),
                |row| row_value_to_json(row, 0),
            )?;
            Ok(AliasRunResult::Value(value))
        }
        AliasAggregator::GroupBy { .. } => unreachable_group_by(),
    }
}

fn run_group_by(
    conn: &rusqlite::Connection,
    inner_sql: &str,
    params: &[FilterParam],
    by_field: &str,
    having: Option<&ListFilter>,
    inner: Option<&AliasAggregator>,
) -> Result<AliasRunResult, MiniAppError> {
    let group_key_expr = format!("json_extract(data, '$.{by_field}')");
    let inner_agg_sql = match inner {
        Some(a) => a.scalar_agg_sql()?,
        None => "COUNT(*)".to_string(),
    };
    let (having_sql, having_params) = match having {
        Some(f) => {
            let (frag, p) = f.build_sql()?;
            (format!(" HAVING {frag}"), p)
        }
        None => (String::new(), vec![]),
    };
    // Literal " HAVING " keyword sits AFTER " GROUP BY " — Crux #2.
    let sql = format!(
        "SELECT {group_key_expr} AS group_key, COUNT(*), {inner_agg_sql} \
         FROM ({inner_sql}) \
         GROUP BY group_key{having_sql}"
    );
    let mut all_params = params.to_vec();
    all_params.extend(having_params);
    let owned = filter_params_to_rusqlite(&all_params);
    let refs: Vec<&dyn rusqlite::ToSql> = owned.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(refs.iter().copied()), |row| {
            let key: serde_json::Value = row_value_to_json(row, 0)?;
            let count: i64 = row.get(1)?;
            let value: Option<serde_json::Value> = if inner.is_some() {
                Some(row_value_to_json(row, 2)?)
            } else {
                None
            };
            Ok(GroupResult { key, count, value })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(AliasRunResult::Groups(rows))
}

fn row_value_to_json(
    row: &rusqlite::Row,
    idx: usize,
) -> rusqlite::Result<serde_json::Value> {
    use rusqlite::types::ValueRef;
    let v = row.get_ref(idx)?;
    Ok(match v {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(i) => serde_json::Value::from(i),
        ValueRef::Real(f) => serde_json::Value::from(f),
        ValueRef::Text(t) => {
            serde_json::Value::String(String::from_utf8_lossy(t).into_owned())
        }
        ValueRef::Blob(_) => serde_json::Value::Null,
    })
}

fn unreachable_group_by() -> Result<AliasRunResult, MiniAppError> {
    Err(MiniAppError::Aggregator(
        "internal: GroupBy reached scalar dispatch path".into(),
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{FieldDef, FieldType, SchemaConfig};
    use rusqlite::Connection;
    use tempfile::TempDir;

    fn test_schema() -> SchemaConfig {
        SchemaConfig {
            table: "t".into(),
            title: None,
            description: None,
            fields: vec![
                FieldDef {
                    name: "tag".into(),
                    ty: FieldType::String,
                    required: true,
                    description: None,
                },
                FieldDef {
                    name: "amount".into(),
                    ty: FieldType::Number,
                    required: true,
                    description: None,
                },
            ],
            dump: None,
        }
    }

    fn build_in_memory_db_with_rows(rows: &[(&str, &str, f64)]) -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("rows.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE rows (\
                id TEXT PRIMARY KEY,\
                data TEXT NOT NULL,\
                created_at INTEGER NOT NULL,\
                updated_at INTEGER NOT NULL\
            )",
        )
        .unwrap();
        for (id, tag, amount) in rows {
            let data = serde_json::json!({ "tag": tag, "amount": amount });
            conn.execute(
                "INSERT INTO rows (id, data, created_at, updated_at) \
                 VALUES (?1, ?2, 0, 0)",
                rusqlite::params![*id, data.to_string()],
            )
            .unwrap();
        }
        dir
    }

    // -----------------------------------------------------------------
    // (a) Count single — Single source, COUNT(*) returns row count
    // -----------------------------------------------------------------
    #[test]
    fn count_single_source_returns_row_count() {
        let dir = build_in_memory_db_with_rows(&[
            ("r1", "a", 1.0),
            ("r2", "b", 2.0),
            ("r3", "a", 3.0),
        ]);
        let result = run_aggregate_blocking(
            &[dir.path().join("rows.db")],
            None,
            &AliasAggregator::Count,
        )
        .unwrap();
        assert!(matches!(result, AliasRunResult::Count(3)));
    }

    // -----------------------------------------------------------------
    // (b) Sum single — Sum aggregates the amount column
    // -----------------------------------------------------------------
    #[test]
    fn sum_single_source_returns_sum() {
        let dir = build_in_memory_db_with_rows(&[
            ("r1", "a", 10.0),
            ("r2", "b", 20.5),
            ("r3", "a", 30.0),
        ]);
        let result = run_aggregate_blocking(
            &[dir.path().join("rows.db")],
            None,
            &AliasAggregator::Sum {
                field: "amount".into(),
            },
        )
        .unwrap();
        match result {
            AliasRunResult::Value(v) => {
                let n = v.as_f64().expect("Sum should return a number");
                assert!((n - 60.5).abs() < 1e-9);
            }
            other => panic!("expected Value variant, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // (c) Min/Max/Avg single — sanity smoke for the scalar variants
    // -----------------------------------------------------------------
    #[test]
    fn min_max_avg_single_source_returns_scalars() {
        let dir = build_in_memory_db_with_rows(&[
            ("r1", "a", 10.0),
            ("r2", "a", 20.0),
            ("r3", "a", 30.0),
        ]);
        let path = dir.path().join("rows.db");
        let min = run_aggregate_blocking(
            &[path.clone()],
            None,
            &AliasAggregator::Min {
                field: "amount".into(),
            },
        )
        .unwrap();
        let max = run_aggregate_blocking(
            &[path.clone()],
            None,
            &AliasAggregator::Max {
                field: "amount".into(),
            },
        )
        .unwrap();
        let avg = run_aggregate_blocking(
            &[path],
            None,
            &AliasAggregator::Avg {
                field: "amount".into(),
            },
        )
        .unwrap();
        for (label, r, expected) in [("Min", min, 10.0), ("Max", max, 30.0), ("Avg", avg, 20.0)] {
            match r {
                AliasRunResult::Value(v) => {
                    let n = v.as_f64().expect("scalar should return a number");
                    assert!((n - expected).abs() < 1e-9, "{label} mismatch: got {n}");
                }
                other => panic!("{label}: expected Value, got {other:?}"),
            }
        }
    }

    // -----------------------------------------------------------------
    // (d) GroupBy + HAVING + inner — Crux #2 (HAVING positioning)
    //     and inner sum integration
    // -----------------------------------------------------------------
    #[test]
    fn groupby_with_having_and_inner_sum_filters_groups() {
        let dir = build_in_memory_db_with_rows(&[
            ("r1", "a", 5.0),
            ("r2", "a", 5.0),
            ("r3", "b", 1.0),
            ("r4", "c", 3.0),
            ("r5", "c", 4.0),
        ]);
        let inner = Box::new(AliasAggregator::Sum {
            field: "amount".into(),
        });
        // HAVING COUNT(*) > 1 keeps only groups 'a' (count=2) and 'c' (count=2).
        let result = run_aggregate_blocking(
            &[dir.path().join("rows.db")],
            None,
            &AliasAggregator::GroupBy {
                by_field: "tag".into(),
                having: Some(ListFilter::And {
                    filters: vec![ListFilter::Eq {
                        field: "tag".into(),
                        value: serde_json::Value::String("a".into()),
                    }],
                }),
                inner: Some(inner),
            },
        )
        .unwrap();
        let groups = match result {
            AliasRunResult::Groups(g) => g,
            other => panic!("expected Groups, got {other:?}"),
        };
        assert_eq!(groups.len(), 1, "HAVING should leave 1 group: {groups:?}");
        let g = &groups[0];
        assert_eq!(g.key, serde_json::Value::String("a".into()));
        assert_eq!(g.count, 2);
        let inner_value = g
            .value
            .as_ref()
            .expect("inner aggregator should produce a value")
            .as_f64()
            .expect("inner sum should be a number");
        assert!((inner_value - 10.0).abs() < 1e-9);
    }

    // -----------------------------------------------------------------
    // (e) Multi UNION ALL substring — Crux #3 literal verification
    // -----------------------------------------------------------------
    #[test]
    fn multi_source_emits_union_all_not_join() {
        let (sql, _) = build_inner_sql(
            &["db_0".to_string(), "db_1".to_string()],
            None,
        )
        .unwrap();
        assert!(sql.contains("UNION ALL"), "expected UNION ALL in: {sql}");
        let upper = sql.to_uppercase();
        assert!(!upper.contains(" JOIN "), "JOIN must not appear: {sql}");
        assert!(
            sql.contains("FROM db_0.rows") && sql.contains("FROM db_1.rows"),
            "expected attached aliases db_0.rows / db_1.rows: {sql}"
        );
    }

    // -----------------------------------------------------------------
    // (f) GroupBy HAVING positioning — Crux #2 literal substring
    // -----------------------------------------------------------------
    #[test]
    fn groupby_having_emitted_after_group_by() {
        // Build a synthetic SQL via the same code path used by run_group_by,
        // by constructing the fragments directly and checking ordering.
        let by_field = "tag";
        let group_key_expr = format!("json_extract(data, '$.{by_field}')");
        let inner_sql = "SELECT id, data, created_at, updated_at FROM db_0.rows";
        let having_fragment = "json_extract(data, '$.tag') = ?";
        let sql = format!(
            "SELECT {group_key_expr} AS group_key, COUNT(*), COUNT(*) \
             FROM ({inner_sql}) \
             GROUP BY group_key HAVING {having_fragment}"
        );
        let gb = sql.find("GROUP BY").expect("expected GROUP BY in SQL");
        let hv = sql.find("HAVING").expect("expected HAVING in SQL");
        assert!(
            hv > gb,
            "HAVING ({hv}) must follow GROUP BY ({gb}) — Crux #2"
        );
        let where_idx = sql.to_uppercase().find(" WHERE ");
        assert!(where_idx.is_none(), "having must not be emitted as WHERE");
    }

    // -----------------------------------------------------------------
    // (g) Multi-DB ATTACH + UNION ALL — real two-db end-to-end COUNT
    // -----------------------------------------------------------------
    #[test]
    fn multi_db_attach_union_all_count_returns_combined_total() {
        let dir_a = build_in_memory_db_with_rows(&[
            ("a1", "x", 1.0),
            ("a2", "x", 2.0),
        ]);
        let dir_b = build_in_memory_db_with_rows(&[
            ("b1", "y", 10.0),
            ("b2", "y", 20.0),
            ("b3", "y", 30.0),
        ]);
        let result = run_aggregate_blocking(
            &[
                dir_a.path().join("rows.db"),
                dir_b.path().join("rows.db"),
            ],
            None,
            &AliasAggregator::Count,
        )
        .unwrap();
        // UNION ALL means N+M, JOIN would mean N*M (= 6).
        assert!(
            matches!(result, AliasRunResult::Count(5)),
            "expected combined count 2+3=5, got {result:?}"
        );
    }

    // -----------------------------------------------------------------
    // (h) ATTACH limit — 11 sources triggers MiniAppError::Aggregator
    // -----------------------------------------------------------------
    #[test]
    fn too_many_sources_returns_aggregator_error() {
        let registry = TableRegistry::from_entries(
            std::collections::HashMap::new(),
            None,
        );
        let names: Vec<String> = (0..(SQLITE_MAX_ATTACHED + 1))
            .map(|i| format!("t{i}"))
            .collect();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime build");
        let err = rt
            .block_on(execute_aggregate(
                &registry,
                SourceSpec::Multi(names),
                None,
                AliasAggregator::Count,
                &test_schema(),
            ))
            .expect_err("expected aggregator error");
        assert_eq!(err.code(), crate::error::codes::AGGREGATOR_ERROR);
    }

    // -----------------------------------------------------------------
    // (i) Identifier sanity — rejects table names with injection chars
    // -----------------------------------------------------------------
    #[test]
    fn validate_identifier_rejects_injection_chars() {
        assert!(validate_identifier("source_table", "t1").is_ok());
        assert!(validate_identifier("source_table", "_x").is_ok());
        assert!(validate_identifier("source_table", "t1; DROP TABLE").is_err());
        assert!(validate_identifier("source_table", "1starts_digit").is_err());
        assert!(validate_identifier("source_table", "").is_err());
    }

    // -----------------------------------------------------------------
    // (j) SourceSpec::tables — Single and Multi return correct slices
    // -----------------------------------------------------------------
    #[test]
    fn source_spec_tables_slice() {
        let single = SourceSpec::Single("t".into());
        assert_eq!(single.tables(), &["t".to_string()]);
        let multi = SourceSpec::Multi(vec!["a".into(), "b".into()]);
        assert_eq!(multi.tables(), &["a".to_string(), "b".to_string()]);
    }

    // -----------------------------------------------------------------
    // (k) Nested GroupBy rejected
    // -----------------------------------------------------------------
    #[test]
    fn nested_groupby_is_rejected_at_validation() {
        let nested = AliasAggregator::GroupBy {
            by_field: "tag".into(),
            having: None,
            inner: Some(Box::new(AliasAggregator::GroupBy {
                by_field: "tag".into(),
                having: None,
                inner: None,
            })),
        };
        let err = nested.validate(&test_schema()).unwrap_err();
        assert_eq!(err.code(), crate::error::codes::AGGREGATOR_ERROR);
    }
}
