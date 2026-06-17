//! Top-level orchestration for the `alias_run` MCP tool.
//!
//! This module exposes [`execute_alias_run`] as the single entry point for
//! running a named alias. Both the MCP tool handler and direct SDK consumers
//! call this function; the MCP handler is a thin wrapper that:
//! 1. Resolves the [`AliasRecord`] from global or per-table storage.
//! 2. Calls [`execute_alias_run`].
//! 3. Serialises the [`AliasRunValue`] result to JSON (backward-compat shape).
//!
//! # Crux compliance
//!
//! - **Crux #1 / #2**: `crates/core/Cargo.toml` must not declare `rmcp` as a
//!   dependency. All types here are pure Core-native types. MCP boundary
//!   conversions (e.g. `MiniAppError → rmcp::ErrorData`) are the sole
//!   responsibility of private adapter functions in `crates/mcp`.
//!
//! # MiniJinja render pipeline (Crux §1/#2)
//!
//! When `record.params_schema` is `Some`, the `filter` field is a MiniJinja
//! template. The template is rendered with `params` as the context, then the
//! rendered string is parsed as a [`ListFilter`] JSON document. When
//! `params_schema` is `None`, the render step is skipped entirely for backward
//! compatibility with plain-JSON aliases.

use std::sync::Arc;

use serde::Serialize;

use crate::aggregator::{AliasAggregator, AliasRunResult, SourceSpec, execute_aggregate};
use crate::alias_storage::AliasRecord;
use crate::error::MiniAppError;
use crate::filter::ListFilter;
use crate::materialize::{FieldSelector, apply_projection};
use crate::registry::TableRegistry;

// =============================================================================
// Result type
// =============================================================================

/// The result of [`execute_alias_run`].
///
/// Wraps both the plain Rows path (per-table `store.list` + field projection)
/// and the Aggregator path (`execute_aggregate`). MCP callers serialise each
/// variant with its natural JSON shape to preserve backward compatibility:
///
/// - `Rows(records)` → `serde_json::to_string(&records)`
/// - `Aggregate(result)` → `serde_json::to_string(&result)`
#[derive(Debug, Serialize)]
pub enum AliasRunValue {
    /// Plain rows path — result of `store.list` + field projection.
    Rows(Vec<crate::store::RowRecord>),
    /// Aggregator path — wraps the existing [`AliasRunResult`].
    Aggregate(AliasRunResult),
}

// =============================================================================
// Public entry point
// =============================================================================

/// Execute an alias and return the typed result.
///
/// This is the canonical alias_run implementation. Both the MCP `alias_run`
/// tool handler and direct SDK consumers call this function.
///
/// # Arguments
///
/// - `registry` — Live [`TableRegistry`]; used to resolve store handles and
///   schema configs from table names, and to collect table names for
///   [`SourceSpec::Pattern`] resolution.
/// - `record` — The [`AliasRecord`] that describes the alias (sources,
///   aggregator, filter template, parameter schema, default limit).
/// - `params` — Optional JSON value used as the MiniJinja render context.
///   Required when `record.params_schema` is `Some`; ignored (and silently
///   accepted) when `None`.
/// - `table_fallback` — Legacy single-table mode: the `table` argument
///   supplied to `alias_run`. Used when `record.sources` is
///   `SourceSpec::Single` with an empty placeholder produced by the legacy
///   per-table path. Ignored when `record.sources` is already a fully
///   populated `Single`/`Multi`/`Pattern`.
/// - `limit_override` — Caller-supplied row limit. Falls back to
///   `record.default_limit` when `None`.
/// - `offset` — Number of rows to skip (plain Rows path only).
/// - `fields` — Field projection selector (plain Rows path only).
///
/// # Errors
///
/// - [`MiniAppError::AliasParamsRequired`] — `params_schema` is `Some` but
///   `params` is `None`.
/// - [`MiniAppError::AliasTemplateError`] — MiniJinja render failure.
/// - [`MiniAppError::Filter`] — filter parse or validate failure.
/// - [`MiniAppError::Aggregator`] — aggregator execute failure.
/// - [`MiniAppError::TableNotFound`] — a referenced table is not in the
///   registry.
/// - [`MiniAppError::Storage`] — underlying SQLite error.
pub async fn execute_alias_run(
    registry: &TableRegistry,
    record: AliasRecord,
    params: Option<serde_json::Value>,
    table_fallback: Option<&str>,
    limit_override: Option<u32>,
    offset: Option<u32>,
    fields: Option<FieldSelector>,
) -> Result<AliasRunValue, MiniAppError> {
    // -----------------------------------------------------------------
    // Step 1: Render the filter template (Crux #1/#2).
    // -----------------------------------------------------------------
    let filter_text = record.filter;
    let filter: ListFilter = if record.params_schema.is_some() {
        let mut params_value = params.ok_or_else(|| MiniAppError::AliasParamsRequired {
            name: record.name.clone(),
        })?;
        // Defensive parse: some MCP transports (notably the Claude Code
        // stdio client) stringify `serde_json::Value` argument fields, so
        // a `params: {"k": "v"}` payload arrives here as
        // `Value::String("{\"k\":\"v\"}")` rather than `Value::Object(..)`.
        // When minijinja receives a String it cannot resolve `{{ k }}`
        // (the String has no keys), the placeholder evaluates to undefined
        // and lenient render emits an empty value. Re-parse JSON-encoded
        // strings into their Object form so render works regardless of
        // the transport.
        if let serde_json::Value::String(ref s) = params_value {
            params_value = serde_json::from_str(s).map_err(|e| {
                MiniAppError::AliasTemplateError(format!(
                    "params arrived as a string but failed JSON parse: {e}"
                ))
            })?;
        }
        let env = minijinja::Environment::new();
        let rendered = env
            .render_str(&filter_text, &params_value)
            .map_err(|e| MiniAppError::AliasTemplateError(e.to_string()))?;
        serde_json::from_str(&rendered)
            .map_err(|e| MiniAppError::Schema(format!("rendered filter parse error: {e}")))?
    } else {
        serde_json::from_str(&filter_text)
            .map_err(|e| MiniAppError::Schema(format!("filter parse error: {e}")))?
    };

    let limit = limit_override.or(record.default_limit);

    // -----------------------------------------------------------------
    // Crux #2: Field-projection fallback.
    //
    // When the caller supplies `fields` at run-time, use it as-is.
    // When the caller omits `fields` (None), fall back to the stored
    // default from `record.fields`.
    // When `record.fields` is also None, no projection is applied — all
    // fields are returned (Crux #3: NULL must never become an empty list).
    // -----------------------------------------------------------------
    let fields = match fields {
        Some(f) => Some(f),
        None => match record.fields.as_deref() {
            Some(json) => Some(serde_json::from_str(json).map_err(|e| {
                MiniAppError::Schema(format!(
                    "alias '{}' stored fields parse error: {e}",
                    record.name
                ))
            })?),
            None => None,
        },
    };

    // -----------------------------------------------------------------
    // Step 2: Aggregator path dispatch.
    // -----------------------------------------------------------------
    if let Some(agg) = record.aggregator {
        return execute_aggregator_path(registry, record.sources, filter, agg, limit).await;
    }

    // -----------------------------------------------------------------
    // Step 3: Plain (Rows) path.
    // -----------------------------------------------------------------
    execute_rows_path(
        registry,
        record.sources,
        table_fallback,
        filter,
        limit,
        offset,
        fields,
    )
    .await
}

// =============================================================================
// Internal helpers
// =============================================================================

/// Aggregator execution path: resolves Pattern sources, validates, and calls
/// [`execute_aggregate`].
async fn execute_aggregator_path(
    registry: &TableRegistry,
    sources: SourceSpec,
    filter: ListFilter,
    agg: AliasAggregator,
    _limit: Option<u32>,
) -> Result<AliasRunValue, MiniAppError> {
    let resolved = if sources.requires_resolve() {
        let table_names: Vec<String> = registry.table_names().map(str::to_owned).collect();
        sources.resolve_pattern(&table_names)?
    } else {
        sources
    };

    let schema_table =
        resolved.tables().first().cloned().ok_or_else(|| {
            MiniAppError::Aggregator("alias sources resolved to zero tables".into())
        })?;

    let schema = Arc::clone(&registry.resolve(Some(schema_table.as_str()))?.schema);

    filter.validate(&schema)?;

    let result = execute_aggregate(registry, resolved, Some(filter), agg, &schema).await?;
    Ok(AliasRunValue::Aggregate(result))
}

/// Plain rows execution path: resolves a single-table store and returns rows
/// after applying optional field projection.
async fn execute_rows_path(
    registry: &TableRegistry,
    sources: SourceSpec,
    table_fallback: Option<&str>,
    filter: ListFilter,
    limit: Option<u32>,
    offset: Option<u32>,
    fields: Option<FieldSelector>,
) -> Result<AliasRunValue, MiniAppError> {
    let table_name: Option<&str> = match &sources {
        // Non-empty Single → use it directly.
        // Empty-string Single is the legacy sentinel produced by the MCP wrapper
        // when reading from per-table `_aliases`; fall through to `table_fallback`.
        SourceSpec::Single(t) if !t.is_empty() => Some(t.as_str()),
        SourceSpec::Single(_) => None,
        SourceSpec::Multi(_) | SourceSpec::Pattern(_) => {
            return Err(MiniAppError::Aggregator(
                "Multi/Pattern source aliases require an aggregator (Phase 2 limitation)".into(),
            ));
        }
    };

    // Use `table_name` from sources when available; otherwise fall back to
    // the legacy `table_fallback` arg (per-table alias_run path).
    let effective_table = table_name.or(table_fallback);

    let entry = registry.resolve(effective_table)?;
    let store = Arc::clone(&entry.store);
    let schema = Arc::clone(&entry.schema);

    filter.validate(&schema)?;
    let records = store.list(limit, offset, Some(filter)).await?;
    let records = apply_projection(records, &fields, &schema)?;
    Ok(AliasRunValue::Rows(records))
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tempfile::tempdir;

    use super::*;
    use crate::aggregator::{AliasAggregator, SourceSpec};
    use crate::alias_storage::AliasRecord;
    use crate::registry::{TableEntry, TableRegistry};
    use crate::schema::{FieldDef, FieldType, SchemaConfig};
    use crate::store::Store;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Minimal schema with a `status` string field (direct struct construction).
    fn status_schema() -> SchemaConfig {
        SchemaConfig {
            table: "items".into(),
            title: None,
            description: None,
            fields: vec![FieldDef {
                name: "status".into(),
                ty: FieldType::String,
                required: false,
                description: None,
            }],
            dump: None,
        }
    }

    /// Build a tempfile-backed Store with seed rows.
    async fn make_store_with_rows(schema: &SchemaConfig, rows: Vec<serde_json::Value>) -> Store {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let store = Store::open(&db_path, schema.clone())
            .await
            .expect("store open");
        // Leak the tempdir so the db file lives for the test duration.
        std::mem::forget(dir);
        for row in rows {
            store.create(row).await.expect("insert row");
        }
        store
    }

    /// Build a single-table [`TableRegistry`] from a store + schema.
    fn registry_from_store(table: &str, store: Store, schema: SchemaConfig) -> TableRegistry {
        let mut entries = HashMap::new();
        entries.insert(
            table.to_string(),
            TableEntry {
                store: Arc::new(store),
                schema: Arc::new(schema),
                schema_path: Arc::new(std::path::PathBuf::new()),
            },
        );
        TableRegistry::from_entries(entries, Some(table.to_string()))
    }

    /// Build a minimal [`AliasRecord`] (no aggregator, no params, no stored fields).
    fn plain_alias(sources: SourceSpec, filter_json: &str) -> AliasRecord {
        AliasRecord {
            name: "test_alias".into(),
            sources,
            aggregator: None,
            filter: filter_json.into(),
            default_limit: None,
            description: None,
            params_schema: None,
            fields: None,
            scope: None,
        }
    }

    // -----------------------------------------------------------------------
    // Test: Rows path — Single source + plain filter
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn rows_path_single_source() {
        let schema = status_schema();
        let store = make_store_with_rows(
            &schema,
            vec![
                serde_json::json!({"status": "open"}),
                serde_json::json!({"status": "closed"}),
            ],
        )
        .await;
        let registry = registry_from_store("items", store, schema);

        // Filter: only "open" rows. ListFilter uses {"type":"eq",...} shape.
        let filter_json = r#"{"type":"eq","field":"status","value":"open"}"#;
        let record = plain_alias(SourceSpec::Single("items".into()), filter_json);

        let result = execute_alias_run(&registry, record, None, None, None, None, None)
            .await
            .expect("execute_alias_run");

        match result {
            AliasRunValue::Rows(rows) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].data["status"], "open");
            }
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Test: Aggregator path — Count
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn aggregator_path_count() {
        let schema = status_schema();
        let store = make_store_with_rows(
            &schema,
            vec![
                serde_json::json!({"status": "open"}),
                serde_json::json!({"status": "open"}),
                serde_json::json!({"status": "closed"}),
            ],
        )
        .await;
        let registry = registry_from_store("items", store, schema);

        // Use an "open" status filter to match 2 of 3 rows.
        let record = AliasRecord {
            name: "count_alias".into(),
            sources: SourceSpec::Single("items".into()),
            aggregator: Some(AliasAggregator::Count),
            filter: r#"{"type":"eq","field":"status","value":"open"}"#.into(),
            default_limit: None,
            description: None,
            params_schema: None,
            fields: None,
            scope: None,
        };

        let result = execute_alias_run(&registry, record, None, None, None, None, None)
            .await
            .expect("execute_alias_run");

        match result {
            AliasRunValue::Aggregate(AliasRunResult::Count(n)) => assert_eq!(n, 2),
            other => panic!("expected Aggregate(Count(2)), got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Test: MiniJinja render — params_schema=Some, template substitution
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn jinja_render_substitution() {
        let schema = status_schema();
        let store = make_store_with_rows(
            &schema,
            vec![
                serde_json::json!({"status": "open"}),
                serde_json::json!({"status": "closed"}),
            ],
        )
        .await;
        let registry = registry_from_store("items", store, schema);

        // Template: substitute {{ status }} from params. Uses ListFilter JSON shape.
        let record = AliasRecord {
            name: "templated_alias".into(),
            sources: SourceSpec::Single("items".into()),
            aggregator: None,
            filter: r#"{"type":"eq","field":"status","value":"{{ status }}"}"#.into(),
            default_limit: None,
            description: None,
            params_schema: Some(r#"["status"]"#.into()),
            fields: None,
            scope: None,
        };

        let params = serde_json::json!({"status": "closed"});

        let result = execute_alias_run(&registry, record, Some(params), None, None, None, None)
            .await
            .expect("execute_alias_run");

        match result {
            AliasRunValue::Rows(rows) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].data["status"], "closed");
            }
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Test: MCP transport stringified params — Value::String("{...}") path
    //
    // Some MCP transports (notably the Claude Code stdio client) deliver
    // `Option<serde_json::Value>` argument fields as JSON-encoded strings
    // rather than parsed objects. Verify the defensive re-parse path so
    // `{{ key }}` still resolves when params arrives as Value::String.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn jinja_render_with_stringified_params() {
        let schema = status_schema();
        let store = make_store_with_rows(
            &schema,
            vec![
                serde_json::json!({"status": "open"}),
                serde_json::json!({"status": "closed"}),
            ],
        )
        .await;
        let registry = registry_from_store("items", store, schema);

        let record = AliasRecord {
            name: "templated_alias".into(),
            sources: SourceSpec::Single("items".into()),
            aggregator: None,
            filter: r#"{"type":"eq","field":"status","value":"{{ status }}"}"#.into(),
            default_limit: None,
            description: None,
            params_schema: Some(r#"["status"]"#.into()),
            fields: None,
            scope: None,
        };

        // params arrives as Value::String containing a JSON-encoded object
        // (the failure mode observed via the Claude Code MCP transport).
        let stringified_params = serde_json::Value::String(r#"{"status": "closed"}"#.to_string());

        let result = execute_alias_run(
            &registry,
            record,
            Some(stringified_params),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("execute_alias_run must re-parse stringified params");

        match result {
            AliasRunValue::Rows(rows) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].data["status"], "closed");
            }
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Test: Legacy mode fallback — sources Single("") + table_fallback
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn legacy_mode_table_fallback() {
        let schema = status_schema();
        let store =
            make_store_with_rows(&schema, vec![serde_json::json!({"status": "open"})]).await;
        let registry = registry_from_store("items", store, schema);

        // Simulate a legacy record where `sources` has an empty Single table
        // name and `table_fallback` provides the real name.
        let record = AliasRecord {
            name: "legacy_alias".into(),
            sources: SourceSpec::Single(String::new()),
            aggregator: None,
            filter: r#"{"type":"eq","field":"status","value":"open"}"#.into(),
            default_limit: None,
            description: None,
            params_schema: None,
            fields: None,
            scope: None,
        };

        let result = execute_alias_run(&registry, record, None, Some("items"), None, None, None)
            .await
            .expect("execute_alias_run");

        match result {
            AliasRunValue::Rows(rows) => assert!(!rows.is_empty()),
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Test: Multi/Pattern without aggregator → error
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn multi_without_aggregator_is_error() {
        let schema = status_schema();
        let store = make_store_with_rows(&schema, vec![]).await;
        let registry = registry_from_store("items", store, schema);

        let record = AliasRecord {
            name: "multi_alias".into(),
            sources: SourceSpec::Multi(vec!["items".into(), "other".into()]),
            aggregator: None,
            filter: r#"{"type":"eq","field":"status","value":"open"}"#.into(),
            default_limit: None,
            description: None,
            params_schema: None,
            fields: None,
            scope: None,
        };

        let err = execute_alias_run(&registry, record, None, None, None, None, None)
            .await
            .expect_err("should fail");

        let msg = err.to_string();
        assert!(
            msg.contains("Multi/Pattern source aliases require an aggregator"),
            "unexpected error: {msg}"
        );
    }
}
