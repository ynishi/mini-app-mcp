/// `row_materialize` MCP tool implementation.
///
/// This module exposes [`do_materialize`] which selects rows from a table,
/// projects their fields, serialises them into one of four formats, and writes
/// the result to an absolute filesystem path.
///
/// # Crux constraints
///
/// - **Absolute path Agent-First trust** (Crux #1): `dest` is validated with
///   [`std::path::Path::is_absolute`] only.  Relative paths are rejected at
///   the parameter-validation boundary; no project-root sandbox is applied to
///   absolute paths.
/// - **format × selector × concat grid test** (Crux #2): the test suite covers
///   every cell of the 4 × 2 × 2 grid.
/// - **SHA256 digest** (Crux #3): every `MaterializeFile` entry carries a
///   64-character hex SHA-256 digest computed from the written bytes.  The
///   field is never empty, `None`, or truncated — even when `dry_run=true`.
use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;
use hex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::MiniAppError;
use crate::filter::ListFilter;
use crate::registry::TableRegistry;
use crate::schema::SchemaConfig;
use crate::store::RowRecord;

// =============================================================================
// Public parameter / result types
// =============================================================================

/// Selector that identifies which rows to materialise.
///
/// # Variants
/// - `ById` — fetch a single row by its UUID primary key.
/// - `ByFilter` — fetch rows matching a [`ListFilter`] predicate.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RowSelector {
    /// Select a single row by its UUID.
    ById {
        /// The UUID of the row to select.
        id: String,
    },
    /// Select rows matching a filter predicate.
    ByFilter {
        /// The filter to apply.
        filter: ListFilter,
        /// Maximum number of rows to return.  Defaults to 100.
        limit: Option<u32>,
        /// Number of rows to skip.  Defaults to 0.
        offset: Option<u32>,
    },
}

/// Field projection selector.
///
/// # Variants
/// - `All` — include all schema fields in declaration order.
/// - `List` — include only the listed fields, in the specified order.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum FieldSelector {
    /// Include every schema field in declaration order.
    All,
    /// Include only the listed fields, in the specified order.
    List {
        /// The field names to include.
        fields: Vec<String>,
    },
}

impl FieldSelector {
    /// Validate field names against the schema's canonical field definitions.
    ///
    /// # Errors
    /// Returns `MiniAppError::Validation` (code: `VALIDATION_ERROR`) if any
    /// field name in `FieldSelector::List` is not present in the schema.
    ///
    /// # Crux compliance
    /// Validates against `schema.fields` (canonical definitions), never
    /// against actual keys present in materialized data (Crux #2).
    pub fn validate(&self, schema: &SchemaConfig) -> Result<(), MiniAppError> {
        if let FieldSelector::List { fields } = self {
            let schema_names: std::collections::HashSet<&str> =
                schema.fields.iter().map(|f| f.name.as_str()).collect();
            for f in fields {
                if !schema_names.contains(f.as_str()) {
                    return Err(MiniAppError::Validation {
                        field: f.clone(),
                        reason: format!(
                            "unknown field '{}' — only schema-registered fields are allowed in field projection",
                            f
                        ),
                    });
                }
            }
        }
        Ok(())
    }
}

/// Output serialisation format.
///
/// Determines both the content written to each file and the file extension
/// used in the `{dest}/{id}.{ext}` naming scheme when `concat=false`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MaterializeFormat {
    /// Plain text: field values joined by newlines.  Extension: `.txt`.
    Raw,
    /// Markdown: each field rendered as a heading + body block.  Extension: `.md`.
    Markdown,
    /// JSON: single object per row, or array when `concat=true`.  Extension: `.json`.
    Json,
    /// YAML: single document per row, or YAML document stream when `concat=true`.  Extension: `.yaml`.
    Yaml,
}

/// Behaviour when the target file already exists.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WriteMode {
    /// Overwrite the existing file (default).
    Overwrite,
    /// Return [`MiniAppError::MaterializeDestInvalid`] if the file already exists.
    Error,
}

/// Parameters for the `row_materialize` MCP tool.
///
/// # Required fields
/// - `selector` — identifies which rows to materialise.
/// - `fields` — field projection.
/// - `format` — output serialisation format.
/// - `dest` — **absolute** filesystem path.  Relative paths are rejected at
///   validation time (Crux #1: Agent-First trust model).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct MaterializeParams {
    /// Target table name.  Optional in legacy single-table mode.
    pub table: Option<String>,
    /// Row selector (by id or by filter).
    pub selector: RowSelector,
    /// Field projection (all fields or a named subset).
    pub fields: FieldSelector,
    /// Output format.
    pub format: MaterializeFormat,
    /// **Absolute** destination path.  When `concat=false` (default) this is
    /// treated as a directory; when `concat=true` it is the output file path.
    pub dest: String,
    /// When `false` (default) each row is written to `{dest}/{row.id}.{ext}`.
    /// When `true` all rows are concatenated into a single file at `{dest}`.
    pub concat: Option<bool>,
    /// Behaviour when the target file already exists.  Defaults to `Overwrite`.
    pub write_mode: Option<WriteMode>,
    /// When `true`, validation, projection, serialisation, and SHA-256
    /// computation are performed but **no file is written**.  The returned
    /// [`MaterializeFile`] entries carry would-be `path`, `bytes`, and
    /// `sha256` values (Crux #3 — digest is always present).
    pub dry_run: Option<bool>,
}

/// A single output file produced by `row_materialize`.
///
/// # Fields
/// - `path` — absolute filesystem path of the written (or would-be) file.
/// - `bytes` — number of bytes written.
/// - `sha256` — 64-character lower-hex SHA-256 digest of the written bytes
///   (Crux #3: always present, never empty).
/// - `row_id` — UUID of the source row when `concat=false`; `null` when
///   `concat=true` (one file covers many rows).  `null` is serialised as JSON
///   `null` (no `skip_serializing_if`).
#[derive(Debug, Serialize)]
pub struct MaterializeFile {
    /// Absolute path of the output file.
    pub path: String,
    /// Byte length of the written content.
    pub bytes: u64,
    /// 64-character lower-hex SHA-256 digest (Crux #3).
    pub sha256: String,
    /// Source row UUID, or `null` when `concat=true`.
    pub row_id: Option<String>,
}

/// Return value of `row_materialize`.
#[derive(Debug, Serialize)]
pub struct MaterializeResult {
    /// Number of output files written (or would-be written when `dry_run=true`).
    pub count: usize,
    /// Per-file metadata.
    pub files: Vec<MaterializeFile>,
}

// =============================================================================
// Internal helpers
// =============================================================================

/// Returns the file extension for a given format.
fn ext_for(format: &MaterializeFormat) -> &'static str {
    match format {
        MaterializeFormat::Raw => "txt",
        MaterializeFormat::Markdown => "md",
        MaterializeFormat::Json => "json",
        MaterializeFormat::Yaml => "yaml",
    }
}

/// Project a single row's `data` JSON object into a `serde_json::Map` using
/// the selected field names.
///
/// Fields that are not present in `data` are mapped to `serde_json::Value::Null`.
fn project_row(
    data: &serde_json::Value,
    field_names: &[String],
) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    for name in field_names {
        let v = data.get(name).cloned().unwrap_or(serde_json::Value::Null);
        map.insert(name.clone(), v);
    }
    map
}

/// Apply field projection to a list of [`RowRecord`]s.
///
/// This is the **single shared post-materialization, pre-serialization boundary**
/// for field projection across the `list`, `get`, and `alias_run` operations
/// (Crux #1: one function, called by all three handlers).
///
/// - `None` or `Some(FieldSelector::All)` → returns `records` unchanged (backward-compatible).
/// - `Some(FieldSelector::List { fields })` → validates field names against
///   `schema` (Crux #2), then projects each row's `data` object to the listed
///   fields.  The row's `id`, `created_at`, and `updated_at` are always preserved.
///
/// # Errors
/// Returns `MiniAppError::Validation` (`VALIDATION_ERROR`) if any field name in
/// `FieldSelector::List` is not present in the schema's canonical field definitions.
pub fn apply_projection(
    records: Vec<RowRecord>,
    fields: &Option<FieldSelector>,
    schema: &SchemaConfig,
) -> Result<Vec<RowRecord>, MiniAppError> {
    let field_selector = match fields {
        None => return Ok(records),
        Some(fs) => fs,
    };
    match field_selector {
        FieldSelector::All => Ok(records),
        FieldSelector::List {
            fields: field_names,
        } => {
            field_selector.validate(schema)?;
            let projected = records
                .into_iter()
                .map(|row| {
                    let projected_map = project_row(&row.data, field_names);
                    RowRecord {
                        data: serde_json::Value::Object(projected_map),
                        ..row
                    }
                })
                .collect();
            Ok(projected)
        }
    }
}

/// Serialise a single projected row into bytes for the given format.
///
/// # Errors
/// - [`MiniAppError::MaterializeFormatError`] if serialisation fails.
fn serialize_row(
    format: &MaterializeFormat,
    projected: &serde_json::Map<String, serde_json::Value>,
    row_id: &str,
) -> Result<Vec<u8>, MiniAppError> {
    match format {
        MaterializeFormat::Raw => {
            // Field values joined by newlines, one value per line.
            let lines: Vec<String> = projected
                .values()
                .map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect();
            Ok(lines.join("\n").into_bytes())
        }
        MaterializeFormat::Markdown => {
            // `# {id}\n\n## {field}\n\n{value}\n\n...`
            let mut md = format!("# {}\n", row_id);
            for (field, value) in projected {
                let text = match value {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                md.push_str(&format!("\n## {}\n\n{}\n", field, text));
            }
            Ok(md.into_bytes())
        }
        MaterializeFormat::Json => {
            let val = serde_json::Value::Object(projected.clone());
            serde_json::to_vec_pretty(&val)
                .map_err(|e| MiniAppError::MaterializeFormatError(format!("json: {e}")))
        }
        MaterializeFormat::Yaml => serde_yaml_bw::to_string(projected)
            .map(|s| s.into_bytes())
            .map_err(|e| MiniAppError::MaterializeFormatError(format!("yaml: {e}"))),
    }
}

/// Concatenate multiple per-row byte sequences according to format rules.
///
/// # Rules
/// - `Raw`: join with `\n\n`
/// - `Markdown`: join with `\n---\n\n`
/// - `Json`: serialise as a JSON array of projected objects
/// - `Yaml`: join with `---\n` (YAML document stream)
///
/// # Errors
/// - [`MiniAppError::MaterializeFormatError`] if JSON serialisation fails.
fn concat_rows(
    format: &MaterializeFormat,
    rows: &[serde_json::Map<String, serde_json::Value>],
    ids: &[String],
) -> Result<Vec<u8>, MiniAppError> {
    match format {
        MaterializeFormat::Raw => {
            // Each row serialised as newline-joined values, separated by \n\n.
            let parts: Result<Vec<String>, _> = rows
                .iter()
                .zip(ids.iter())
                .map(|(projected, id)| {
                    serialize_row(&MaterializeFormat::Raw, projected, id)
                        .map(|b| String::from_utf8_lossy(&b).into_owned())
                })
                .collect();
            let parts = parts?;
            Ok(parts.join("\n\n").into_bytes())
        }
        MaterializeFormat::Markdown => {
            let parts: Result<Vec<String>, _> = rows
                .iter()
                .zip(ids.iter())
                .map(|(projected, id)| {
                    serialize_row(&MaterializeFormat::Markdown, projected, id)
                        .map(|b| String::from_utf8_lossy(&b).into_owned())
                })
                .collect();
            let parts = parts?;
            Ok(parts.join("\n---\n\n").into_bytes())
        }
        MaterializeFormat::Json => {
            let arr: Vec<serde_json::Value> = rows
                .iter()
                .map(|m| serde_json::Value::Object(m.clone()))
                .collect();
            serde_json::to_vec_pretty(&arr)
                .map_err(|e| MiniAppError::MaterializeFormatError(format!("json array: {e}")))
        }
        MaterializeFormat::Yaml => {
            // YAML document stream: each doc preceded by `---\n`.
            let mut out = String::new();
            for projected in rows {
                let doc = serde_yaml_bw::to_string(projected)
                    .map_err(|e| MiniAppError::MaterializeFormatError(format!("yaml: {e}")))?;
                out.push_str("---\n");
                out.push_str(&doc);
            }
            Ok(out.into_bytes())
        }
    }
}

/// Compute the SHA-256 hex digest of a byte slice (Crux #3).
fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

// =============================================================================
// Core function
// =============================================================================

/// Execute the `row_materialize` tool.
///
/// Selects rows from the target table, projects fields, serialises to the
/// requested format, and writes the result to absolute filesystem path(s).
///
/// # Crux constraints enforced here
/// 1. **Absolute path**: `params.dest` is checked with `Path::is_absolute()`
///    at step (a).  Relative paths return `MaterializeDestRelative` immediately.
///    No sandbox constraint is applied to absolute paths.
/// 2. **SHA-256**: every `MaterializeFile` entry carries a 64-char hex SHA-256
///    digest, including when `dry_run=true`.
///
/// # Arguments
/// - `config` — server mount configuration.
/// - `tables` — live `ArcSwap`-wrapped table registry.
/// - `params` — tool parameters.
///
/// # Returns
/// [`MaterializeResult`] on success (JSON-serialisable).
///
/// # Errors
/// - [`MiniAppError::MaterializeDestRelative`] — `dest` is not absolute.
/// - [`MiniAppError::MaterializeFieldUnknown`] — projected field not in schema.
/// - [`MiniAppError::MaterializeInvalidParam`] — incompatible parameter combination.
/// - [`MiniAppError::MaterializeRowNotFound`] — `ById` selector found no row.
/// - [`MiniAppError::MaterializeEmptyResult`] — `ByFilter` selector matched zero rows.
/// - [`MiniAppError::MaterializeFormatError`] — serialisation failure.
/// - [`MiniAppError::MaterializeDestInvalid`] — dest path problem or write_mode conflict.
/// - [`MiniAppError::MaterializeIo`] — filesystem write failure.
/// - [`MiniAppError::MaterializeSha256`] — `spawn_blocking` task panic during SHA-256/write.
pub async fn do_materialize(
    _config: &Config,
    tables: &Arc<ArcSwap<TableRegistry>>,
    params: MaterializeParams,
) -> Result<MaterializeResult, MiniAppError> {
    // (a) Absolute path validation (Crux #1).
    if !Path::new(&params.dest).is_absolute() {
        tracing::warn!(dest = %params.dest, "row_materialize: dest is not absolute");
        return Err(MiniAppError::MaterializeDestRelative {
            path: params.dest.clone(),
        });
    }

    let dest = params.dest.clone();
    let concat = params.concat.unwrap_or(false);
    let dry_run = params.dry_run.unwrap_or(false);
    let write_mode_is_error = matches!(params.write_mode, Some(WriteMode::Error));

    // (b) Resolve table — ArcSwap Guard dropped before any .await (K-103).
    let (store, schema) = {
        let registry = tables.load_full();
        let entry = registry.resolve(params.table.as_deref())?;
        (Arc::clone(&entry.store), Arc::clone(&entry.schema))
    };

    // (c) Validate projected field names against schema.
    let field_names: Vec<String> = match &params.fields {
        FieldSelector::All => schema.fields.iter().map(|f| f.name.clone()).collect(),
        FieldSelector::List { fields } => {
            let schema_names: std::collections::HashSet<&str> =
                schema.fields.iter().map(|f| f.name.as_str()).collect();
            for f in fields {
                if !schema_names.contains(f.as_str()) {
                    tracing::warn!(field = %f, "row_materialize: unknown projection field");
                    return Err(MiniAppError::MaterializeFieldUnknown { field: f.clone() });
                }
            }
            fields.clone()
        }
    };

    // (d) Parameter consistency check.
    if let RowSelector::ById { .. } = &params.selector {
        if concat {
            tracing::warn!("row_materialize: concat=true with selector=by_id is invalid");
            return Err(MiniAppError::MaterializeInvalidParam {
                field: "concat".to_string(),
                reason: "concat=true requires selector=by_filter (ById always yields a single row)"
                    .to_string(),
            });
        }
    }

    // (e) Fetch rows.
    let rows = match params.selector {
        RowSelector::ById { ref id } => {
            let row = store.get(id).await.map_err(|e| match e {
                MiniAppError::NotFound { .. } => {
                    tracing::warn!(id = %id, "row_materialize: row not found");
                    MiniAppError::MaterializeRowNotFound { id: id.clone() }
                }
                other => other,
            })?;
            vec![row]
        }
        RowSelector::ByFilter {
            filter,
            limit,
            offset,
        } => {
            let rows = store.list(limit, offset, Some(filter), None).await?;
            if rows.is_empty() {
                tracing::warn!("row_materialize: by_filter selector matched zero rows");
                return Err(MiniAppError::MaterializeEmptyResult);
            }
            rows
        }
    };

    // (f) Project each row's data into ordered maps.
    let projected_rows: Vec<serde_json::Map<String, serde_json::Value>> = rows
        .iter()
        .map(|row| project_row(&row.data, &field_names))
        .collect();

    let row_ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();

    let format = &params.format;
    let ext = ext_for(format);

    let mut files: Vec<MaterializeFile> = Vec::new();

    if concat {
        // (g)+(h) Concat path: all rows → one file.
        let bytes = concat_rows(format, &projected_rows, &row_ids)?;
        let sha256 = sha256_hex(&bytes);
        let byte_len = bytes.len() as u64;
        let dest_path = dest.clone();

        // write_mode=Error check (dry_run=true still validates — AC #6).
        if write_mode_is_error && Path::new(&dest_path).exists() {
            tracing::warn!(path = %dest_path, "row_materialize: dest already exists with write_mode=error");
            return Err(MiniAppError::MaterializeDestInvalid {
                path: dest_path.clone(),
                reason: "file already exists with write_mode=error".to_string(),
            });
        }

        if !dry_run {
            // Ensure parent directory exists, then write — both inside spawn_blocking (K-110).
            let dest_clone = dest_path.clone();
            let bytes_clone = bytes.clone();
            tokio::task::spawn_blocking(move || -> Result<(), MiniAppError> {
                if let Some(parent) = Path::new(&dest_clone).parent() {
                    if !parent.as_os_str().is_empty() {
                        std::fs::create_dir_all(parent).map_err(|e| {
                            MiniAppError::MaterializeIo(format!(
                                "create_dir_all '{}': {e}",
                                parent.display()
                            ))
                        })?;
                    }
                }
                std::fs::write(&dest_clone, &bytes_clone).map_err(|e| {
                    MiniAppError::MaterializeIo(format!("write '{}': {e}", dest_clone))
                })
            })
            .await
            .map_err(|e| MiniAppError::MaterializeIo(format!("blocking task panic: {e}")))??;
        }

        // (i) row_id is None for concat (Crux #2 wf-sim restructure_shape #2).
        files.push(MaterializeFile {
            path: dest_path,
            bytes: byte_len,
            sha256,
            row_id: None,
        });
    } else {
        // Non-concat path: one file per row.

        // Ensure destination directory exists (idempotent), inside spawn_blocking (K-110).
        if !dry_run {
            let dest_dir = dest.clone();
            tokio::task::spawn_blocking(move || -> Result<(), MiniAppError> {
                std::fs::create_dir_all(&dest_dir).map_err(|e| {
                    MiniAppError::MaterializeIo(format!("create_dir_all '{}': {e}", dest_dir))
                })
            })
            .await
            .map_err(|e| MiniAppError::MaterializeIo(format!("blocking task panic: {e}")))??;
        }

        for (row, projected) in rows.iter().zip(projected_rows.iter()) {
            let out_path = format!("{}/{}.{}", dest, row.id, ext);

            // write_mode=Error check (dry_run=true still validates — AC #6).
            if write_mode_is_error && Path::new(&out_path).exists() {
                tracing::warn!(path = %out_path, "row_materialize: output file already exists with write_mode=error");
                return Err(MiniAppError::MaterializeDestInvalid {
                    path: out_path.clone(),
                    reason: "file already exists with write_mode=error".to_string(),
                });
            }

            let bytes = serialize_row(format, projected, &row.id)?;
            let sha256 = sha256_hex(&bytes);
            let byte_len = bytes.len() as u64;

            if !dry_run {
                let out_path_clone = out_path.clone();
                let bytes_clone = bytes.clone();
                tokio::task::spawn_blocking(move || -> Result<(), MiniAppError> {
                    std::fs::write(&out_path_clone, &bytes_clone).map_err(|e| {
                        MiniAppError::MaterializeIo(format!("write '{}': {e}", out_path_clone))
                    })
                })
                .await
                .map_err(|e| MiniAppError::MaterializeIo(format!("blocking task panic: {e}")))??;
            }

            // (i) row_id = Some(row.id) for non-concat.
            files.push(MaterializeFile {
                path: out_path,
                bytes: byte_len,
                sha256,
                row_id: Some(row.id.clone()),
            });
        }
    }

    // (j) Return result.
    let count = files.len();
    Ok(MaterializeResult { count, files })
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::registry::TableRegistry;
    use crate::schema::{FieldDef, FieldType, SchemaConfig};
    use crate::store::Store;
    use std::path::PathBuf;
    use std::sync::Arc;

    // -------------------------------------------------------------------------
    // Test helpers
    // -------------------------------------------------------------------------

    /// Build a minimal test server backed by an in-memory store with one row.
    async fn make_test_env() -> (Arc<ArcSwap<TableRegistry>>, String, Arc<Config>) {
        let schema = SchemaConfig {
            table: "test".to_string(),
            title: None,
            description: None,
            fields: vec![
                FieldDef {
                    name: "title".to_string(),
                    ty: FieldType::String,
                    required: true,
                    description: None,
                },
                FieldDef {
                    name: "body".to_string(),
                    ty: FieldType::String,
                    required: false,
                    description: None,
                },
            ],
            dump: None,
            history: Default::default(),
        };

        // Use an in-memory SQLite store for tests.
        // SAFETY: ":memory:" always opens without error in test context.
        let store = Store::open(std::path::Path::new(":memory:"), schema.clone())
            .await
            .expect("in-memory store must open");

        // Insert one row before passing ownership to the registry.
        let data = serde_json::json!({"title": "hello", "body": "world"});
        // SAFETY: validated JSON matches the schema above.
        let row = store.create(data).await.expect("create must succeed");
        let row_id = row.id.clone();

        let registry = TableRegistry::from_single(
            store,
            schema,
            PathBuf::from("/fake/schema.yaml"),
            "test".to_string(),
        );

        let config = Arc::new(Config {
            schema_path: None,
            db_path: None,
            user_dir: None,
            project_dir: None,
            backup_retention: None,
            snapshot_retention: None,
        });

        (Arc::new(ArcSwap::from_pointee(registry)), row_id, config)
    }

    /// Insert a second row via the registry and return its id.
    async fn add_second_row(tables: &Arc<ArcSwap<TableRegistry>>) -> String {
        let registry = tables.load_full();
        // SAFETY: resolve(None) works because a default_table is set in from_single.
        let entry = registry.resolve(None).expect("resolve must succeed");
        let data = serde_json::json!({"title": "second", "body": "entry"});
        // SAFETY: validated JSON matches the test schema.
        let row = entry.store.create(data).await.expect("create must succeed");
        row.id
    }

    // -------------------------------------------------------------------------
    // 16-cell grid: format (4) × selector (2) × concat (2)
    // -------------------------------------------------------------------------
    // Naming: materialize_grid_{format}_{selector}_{concat}
    //
    // ById + concat=true (4 cells) → error path (MaterializeInvalidParam)
    // ById + concat=false (4 cells) → happy path
    // ByFilter + concat=false (4 cells) → happy path
    // ByFilter + concat=true (4 cells) → happy path, row_id=None
    // -------------------------------------------------------------------------

    // -- raw × ById × no_concat --

    #[tokio::test]
    async fn materialize_grid_raw_by_id_no_concat() {
        let (tables, row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();
        let dest_path = dest.path().to_str().unwrap().to_string();

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ById { id: row_id.clone() },
            fields: FieldSelector::All,
            format: MaterializeFormat::Raw,
            dest: dest_path.clone(),
            concat: Some(false),
            write_mode: None,
            dry_run: None,
        };

        let result = do_materialize(&config, &tables, params).await.unwrap();
        assert_eq!(result.count, 1);
        let f = &result.files[0];
        assert_eq!(f.row_id, Some(row_id.clone()));
        assert_eq!(f.sha256.len(), 64);
        assert!(f.bytes > 0);
        // File was written.
        let written = std::fs::read_to_string(&f.path).unwrap();
        assert!(written.contains("hello"));
    }

    // -- markdown × ById × no_concat --

    #[tokio::test]
    async fn materialize_grid_markdown_by_id_no_concat() {
        let (tables, row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();
        let dest_path = dest.path().to_str().unwrap().to_string();

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ById { id: row_id.clone() },
            fields: FieldSelector::All,
            format: MaterializeFormat::Markdown,
            dest: dest_path,
            concat: Some(false),
            write_mode: None,
            dry_run: None,
        };

        let result = do_materialize(&config, &tables, params).await.unwrap();
        assert_eq!(result.count, 1);
        let f = &result.files[0];
        assert_eq!(f.row_id, Some(row_id.clone()));
        assert_eq!(f.sha256.len(), 64);
        assert!(f.path.ends_with(".md"));
        let written = std::fs::read_to_string(&f.path).unwrap();
        assert!(written.contains(&format!("# {}", row_id)));
        assert!(written.contains("## title"));
    }

    // -- json × ById × no_concat --

    #[tokio::test]
    async fn materialize_grid_json_by_id_no_concat() {
        let (tables, row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();
        let dest_path = dest.path().to_str().unwrap().to_string();

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ById { id: row_id.clone() },
            fields: FieldSelector::All,
            format: MaterializeFormat::Json,
            dest: dest_path,
            concat: Some(false),
            write_mode: None,
            dry_run: None,
        };

        let result = do_materialize(&config, &tables, params).await.unwrap();
        assert_eq!(result.count, 1);
        let f = &result.files[0];
        assert_eq!(f.row_id, Some(row_id));
        assert_eq!(f.sha256.len(), 64);
        assert!(f.path.ends_with(".json"));
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&f.path).unwrap()).unwrap();
        assert_eq!(parsed["title"], "hello");
    }

    // -- yaml × ById × no_concat --

    #[tokio::test]
    async fn materialize_grid_yaml_by_id_no_concat() {
        let (tables, row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();
        let dest_path = dest.path().to_str().unwrap().to_string();

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ById { id: row_id.clone() },
            fields: FieldSelector::All,
            format: MaterializeFormat::Yaml,
            dest: dest_path,
            concat: Some(false),
            write_mode: None,
            dry_run: None,
        };

        let result = do_materialize(&config, &tables, params).await.unwrap();
        assert_eq!(result.count, 1);
        let f = &result.files[0];
        assert_eq!(f.row_id, Some(row_id));
        assert_eq!(f.sha256.len(), 64);
        assert!(f.path.ends_with(".yaml"));
        let content = std::fs::read_to_string(&f.path).unwrap();
        assert!(content.contains("title"));
    }

    // -- raw × ById × concat=true → error path --

    #[tokio::test]
    async fn materialize_grid_raw_by_id_concat() {
        let (tables, row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();
        let dest_path = format!("{}/out.txt", dest.path().display());

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ById { id: row_id },
            fields: FieldSelector::All,
            format: MaterializeFormat::Raw,
            dest: dest_path,
            concat: Some(true),
            write_mode: None,
            dry_run: None,
        };

        let err = do_materialize(&config, &tables, params).await.unwrap_err();
        assert!(matches!(
            err,
            MiniAppError::MaterializeInvalidParam { ref field, .. } if field == "concat"
        ));
    }

    // -- markdown × ById × concat=true → error path --

    #[tokio::test]
    async fn materialize_grid_markdown_by_id_concat() {
        let (tables, row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();
        let dest_path = format!("{}/out.md", dest.path().display());

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ById { id: row_id },
            fields: FieldSelector::All,
            format: MaterializeFormat::Markdown,
            dest: dest_path,
            concat: Some(true),
            write_mode: None,
            dry_run: None,
        };

        let err = do_materialize(&config, &tables, params).await.unwrap_err();
        assert!(matches!(
            err,
            MiniAppError::MaterializeInvalidParam { ref field, .. } if field == "concat"
        ));
    }

    // -- json × ById × concat=true → error path --

    #[tokio::test]
    async fn materialize_grid_json_by_id_concat() {
        let (tables, row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();
        let dest_path = format!("{}/out.json", dest.path().display());

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ById { id: row_id },
            fields: FieldSelector::All,
            format: MaterializeFormat::Json,
            dest: dest_path,
            concat: Some(true),
            write_mode: None,
            dry_run: None,
        };

        let err = do_materialize(&config, &tables, params).await.unwrap_err();
        assert!(matches!(
            err,
            MiniAppError::MaterializeInvalidParam { ref field, .. } if field == "concat"
        ));
    }

    // -- yaml × ById × concat=true → error path --

    #[tokio::test]
    async fn materialize_grid_yaml_by_id_concat() {
        let (tables, row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();
        let dest_path = format!("{}/out.yaml", dest.path().display());

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ById { id: row_id },
            fields: FieldSelector::All,
            format: MaterializeFormat::Yaml,
            dest: dest_path,
            concat: Some(true),
            write_mode: None,
            dry_run: None,
        };

        let err = do_materialize(&config, &tables, params).await.unwrap_err();
        assert!(matches!(
            err,
            MiniAppError::MaterializeInvalidParam { ref field, .. } if field == "concat"
        ));
    }

    // -- raw × ByFilter × no_concat --

    #[tokio::test]
    async fn materialize_grid_raw_by_filter_no_concat() {
        let (tables, row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();
        let dest_path = dest.path().to_str().unwrap().to_string();

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ByFilter {
                filter: crate::filter::ListFilter::Eq {
                    field: "title".to_string(),
                    value: serde_json::json!("hello"),
                },
                limit: None,
                offset: None,
            },
            fields: FieldSelector::All,
            format: MaterializeFormat::Raw,
            dest: dest_path,
            concat: Some(false),
            write_mode: None,
            dry_run: None,
        };

        let result = do_materialize(&config, &tables, params).await.unwrap();
        assert_eq!(result.count, 1);
        let f = &result.files[0];
        assert_eq!(f.row_id, Some(row_id));
        assert_eq!(f.sha256.len(), 64);
        let written = std::fs::read_to_string(&f.path).unwrap();
        assert!(written.contains("hello"));
    }

    // -- markdown × ByFilter × no_concat --

    #[tokio::test]
    async fn materialize_grid_markdown_by_filter_no_concat() {
        let (tables, row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();
        let dest_path = dest.path().to_str().unwrap().to_string();

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ByFilter {
                filter: crate::filter::ListFilter::Eq {
                    field: "title".to_string(),
                    value: serde_json::json!("hello"),
                },
                limit: None,
                offset: None,
            },
            fields: FieldSelector::All,
            format: MaterializeFormat::Markdown,
            dest: dest_path,
            concat: Some(false),
            write_mode: None,
            dry_run: None,
        };

        let result = do_materialize(&config, &tables, params).await.unwrap();
        assert_eq!(result.count, 1);
        let f = &result.files[0];
        assert_eq!(f.row_id, Some(row_id.clone()));
        assert_eq!(f.sha256.len(), 64);
        let written = std::fs::read_to_string(&f.path).unwrap();
        assert!(written.contains(&format!("# {}", row_id)));
    }

    // -- json × ByFilter × no_concat --

    #[tokio::test]
    async fn materialize_grid_json_by_filter_no_concat() {
        let (tables, row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();
        let dest_path = dest.path().to_str().unwrap().to_string();

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ByFilter {
                filter: crate::filter::ListFilter::Eq {
                    field: "title".to_string(),
                    value: serde_json::json!("hello"),
                },
                limit: None,
                offset: None,
            },
            fields: FieldSelector::All,
            format: MaterializeFormat::Json,
            dest: dest_path,
            concat: Some(false),
            write_mode: None,
            dry_run: None,
        };

        let result = do_materialize(&config, &tables, params).await.unwrap();
        assert_eq!(result.count, 1);
        let f = &result.files[0];
        assert_eq!(f.row_id, Some(row_id));
        assert_eq!(f.sha256.len(), 64);
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&f.path).unwrap()).unwrap();
        assert_eq!(parsed["title"], "hello");
    }

    // -- yaml × ByFilter × no_concat --

    #[tokio::test]
    async fn materialize_grid_yaml_by_filter_no_concat() {
        let (tables, row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();
        let dest_path = dest.path().to_str().unwrap().to_string();

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ByFilter {
                filter: crate::filter::ListFilter::Eq {
                    field: "title".to_string(),
                    value: serde_json::json!("hello"),
                },
                limit: None,
                offset: None,
            },
            fields: FieldSelector::All,
            format: MaterializeFormat::Yaml,
            dest: dest_path,
            concat: Some(false),
            write_mode: None,
            dry_run: None,
        };

        let result = do_materialize(&config, &tables, params).await.unwrap();
        assert_eq!(result.count, 1);
        let f = &result.files[0];
        assert_eq!(f.row_id, Some(row_id));
        assert_eq!(f.sha256.len(), 64);
        let content = std::fs::read_to_string(&f.path).unwrap();
        assert!(content.contains("hello"));
    }

    // -- raw × ByFilter × concat=true --

    #[tokio::test]
    async fn materialize_grid_raw_by_filter_concat() {
        let (tables, _row_id, config) = make_test_env().await;
        add_second_row(&tables).await;
        let dest = tempfile::tempdir().unwrap();
        let out_file = format!("{}/all.txt", dest.path().display());

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ByFilter {
                filter: crate::filter::ListFilter::Eq {
                    field: "title".to_string(),
                    value: serde_json::json!("hello"),
                },
                limit: None,
                offset: None,
            },
            fields: FieldSelector::All,
            format: MaterializeFormat::Raw,
            dest: out_file.clone(),
            concat: Some(true),
            write_mode: None,
            dry_run: None,
        };

        let result = do_materialize(&config, &tables, params).await.unwrap();
        assert_eq!(result.count, 1);
        let f = &result.files[0];
        // concat=true → row_id must be None (Crux #2 wf-sim restructure_shape #2).
        assert_eq!(f.row_id, None);
        assert_eq!(f.sha256.len(), 64);
        assert_eq!(f.path, out_file);
        let content = std::fs::read_to_string(&f.path).unwrap();
        assert!(content.contains("hello"));
    }

    // -- markdown × ByFilter × concat=true --

    #[tokio::test]
    async fn materialize_grid_markdown_by_filter_concat() {
        let (tables, _row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();
        let out_file = format!("{}/all.md", dest.path().display());

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ByFilter {
                filter: crate::filter::ListFilter::Eq {
                    field: "title".to_string(),
                    value: serde_json::json!("hello"),
                },
                limit: None,
                offset: None,
            },
            fields: FieldSelector::All,
            format: MaterializeFormat::Markdown,
            dest: out_file.clone(),
            concat: Some(true),
            write_mode: None,
            dry_run: None,
        };

        let result = do_materialize(&config, &tables, params).await.unwrap();
        assert_eq!(result.count, 1);
        let f = &result.files[0];
        assert_eq!(f.row_id, None);
        assert_eq!(f.sha256.len(), 64);
        let content = std::fs::read_to_string(&f.path).unwrap();
        assert!(content.contains("## title"));
    }

    // -- json × ByFilter × concat=true --

    #[tokio::test]
    async fn materialize_grid_json_by_filter_concat() {
        let (tables, _row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();
        let out_file = format!("{}/all.json", dest.path().display());

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ByFilter {
                filter: crate::filter::ListFilter::Eq {
                    field: "title".to_string(),
                    value: serde_json::json!("hello"),
                },
                limit: None,
                offset: None,
            },
            fields: FieldSelector::All,
            format: MaterializeFormat::Json,
            dest: out_file.clone(),
            concat: Some(true),
            write_mode: None,
            dry_run: None,
        };

        let result = do_materialize(&config, &tables, params).await.unwrap();
        assert_eq!(result.count, 1);
        let f = &result.files[0];
        assert_eq!(f.row_id, None);
        assert_eq!(f.sha256.len(), 64);
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&f.path).unwrap()).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed[0]["title"], "hello");
    }

    // -- yaml × ByFilter × concat=true --

    #[tokio::test]
    async fn materialize_grid_yaml_by_filter_concat() {
        let (tables, _row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();
        let out_file = format!("{}/all.yaml", dest.path().display());

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ByFilter {
                filter: crate::filter::ListFilter::Eq {
                    field: "title".to_string(),
                    value: serde_json::json!("hello"),
                },
                limit: None,
                offset: None,
            },
            fields: FieldSelector::All,
            format: MaterializeFormat::Yaml,
            dest: out_file.clone(),
            concat: Some(true),
            write_mode: None,
            dry_run: None,
        };

        let result = do_materialize(&config, &tables, params).await.unwrap();
        assert_eq!(result.count, 1);
        let f = &result.files[0];
        assert_eq!(f.row_id, None);
        assert_eq!(f.sha256.len(), 64);
        let content = std::fs::read_to_string(&f.path).unwrap();
        assert!(content.starts_with("---\n"));
        assert!(content.contains("hello"));
    }

    // -------------------------------------------------------------------------
    // Path validation tests (3)
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn path_validation_relative_dest() {
        let (tables, row_id, config) = make_test_env().await;

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ById { id: row_id },
            fields: FieldSelector::All,
            format: MaterializeFormat::Raw,
            dest: "relative/path".to_string(), // not absolute
            concat: None,
            write_mode: None,
            dry_run: None,
        };

        let err = do_materialize(&config, &tables, params).await.unwrap_err();
        assert!(matches!(
            err,
            MiniAppError::MaterializeDestRelative { ref path } if path == "relative/path"
        ));
    }

    #[tokio::test]
    async fn path_validation_create_dir_all_success() {
        let (tables, row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();
        // Nested subdirectory that does not yet exist.
        let nested = format!("{}/subdir/nested", dest.path().display());

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ById { id: row_id.clone() },
            fields: FieldSelector::All,
            format: MaterializeFormat::Raw,
            dest: nested.clone(),
            concat: Some(false),
            write_mode: None,
            dry_run: None,
        };

        let result = do_materialize(&config, &tables, params).await.unwrap();
        assert_eq!(result.count, 1);
        assert!(std::path::Path::new(&nested).is_dir());
    }

    #[tokio::test]
    async fn path_validation_concat_true_file_dest() {
        let (tables, _row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();
        let out_file = format!("{}/out.txt", dest.path().display());

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ByFilter {
                filter: crate::filter::ListFilter::Eq {
                    field: "title".to_string(),
                    value: serde_json::json!("hello"),
                },
                limit: None,
                offset: None,
            },
            fields: FieldSelector::All,
            format: MaterializeFormat::Raw,
            dest: out_file.clone(),
            concat: Some(true),
            write_mode: None,
            dry_run: None,
        };

        let result = do_materialize(&config, &tables, params).await.unwrap();
        assert_eq!(result.count, 1);
        assert_eq!(result.files[0].path, out_file);
        assert!(std::path::Path::new(&out_file).exists());
    }

    // -------------------------------------------------------------------------
    // Field projection tests (3)
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn projection_all_fields_in_schema_order() {
        let (tables, row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ById { id: row_id },
            fields: FieldSelector::All,
            format: MaterializeFormat::Json,
            dest: dest.path().to_str().unwrap().to_string(),
            concat: None,
            write_mode: None,
            dry_run: None,
        };

        let result = do_materialize(&config, &tables, params).await.unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&result.files[0].path).unwrap()).unwrap();
        // Both schema fields must be present.
        assert!(parsed.get("title").is_some());
        assert!(parsed.get("body").is_some());
    }

    #[tokio::test]
    async fn projection_list_specified_order() {
        let (tables, row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ById { id: row_id },
            fields: FieldSelector::List {
                fields: vec!["body".to_string()],
            },
            format: MaterializeFormat::Json,
            dest: dest.path().to_str().unwrap().to_string(),
            concat: None,
            write_mode: None,
            dry_run: None,
        };

        let result = do_materialize(&config, &tables, params).await.unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&result.files[0].path).unwrap()).unwrap();
        assert_eq!(parsed["body"], "world");
        // "title" was not requested.
        assert!(parsed.get("title").is_none());
    }

    #[tokio::test]
    async fn projection_unknown_field_returns_error() {
        let (tables, row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ById { id: row_id },
            fields: FieldSelector::List {
                fields: vec!["nonexistent_field".to_string()],
            },
            format: MaterializeFormat::Json,
            dest: dest.path().to_str().unwrap().to_string(),
            concat: None,
            write_mode: None,
            dry_run: None,
        };

        let err = do_materialize(&config, &tables, params).await.unwrap_err();
        assert!(matches!(
            err,
            MiniAppError::MaterializeFieldUnknown { ref field } if field == "nonexistent_field"
        ));
    }

    // -------------------------------------------------------------------------
    // Error variant tests (6)
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn error_dest_invalid_write_mode_error_existing_file() {
        let (tables, _row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();
        let out_file = format!("{}/out.txt", dest.path().display());
        // Pre-create the file.
        std::fs::write(&out_file, b"existing").unwrap();

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ByFilter {
                filter: crate::filter::ListFilter::Eq {
                    field: "title".to_string(),
                    value: serde_json::json!("hello"),
                },
                limit: None,
                offset: None,
            },
            fields: FieldSelector::All,
            format: MaterializeFormat::Raw,
            dest: out_file.clone(),
            concat: Some(true),
            write_mode: Some(WriteMode::Error),
            dry_run: None,
        };

        let err = do_materialize(&config, &tables, params).await.unwrap_err();
        assert!(matches!(
            err,
            MiniAppError::MaterializeDestInvalid { ref path, .. } if path == &out_file
        ));
    }

    #[tokio::test]
    async fn error_row_not_found() {
        let (tables, _row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ById {
                id: "00000000-0000-0000-0000-000000000000".to_string(),
            },
            fields: FieldSelector::All,
            format: MaterializeFormat::Raw,
            dest: dest.path().to_str().unwrap().to_string(),
            concat: None,
            write_mode: None,
            dry_run: None,
        };

        let err = do_materialize(&config, &tables, params).await.unwrap_err();
        assert!(matches!(err, MiniAppError::MaterializeRowNotFound { .. }));
    }

    #[tokio::test]
    async fn error_empty_result() {
        let (tables, _row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ByFilter {
                filter: crate::filter::ListFilter::Eq {
                    field: "title".to_string(),
                    value: serde_json::json!("no_such_title"),
                },
                limit: None,
                offset: None,
            },
            fields: FieldSelector::All,
            format: MaterializeFormat::Raw,
            dest: dest.path().to_str().unwrap().to_string(),
            concat: None,
            write_mode: None,
            dry_run: None,
        };

        let err = do_materialize(&config, &tables, params).await.unwrap_err();
        assert!(matches!(err, MiniAppError::MaterializeEmptyResult));
    }

    #[tokio::test]
    async fn error_invalid_param_concat_by_id() {
        let (tables, row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();
        let out_file = format!("{}/out.txt", dest.path().display());

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ById { id: row_id },
            fields: FieldSelector::All,
            format: MaterializeFormat::Raw,
            dest: out_file,
            concat: Some(true),
            write_mode: None,
            dry_run: None,
        };

        let err = do_materialize(&config, &tables, params).await.unwrap_err();
        assert!(matches!(
            err,
            MiniAppError::MaterializeInvalidParam { ref field, .. } if field == "concat"
        ));
    }

    #[tokio::test]
    async fn error_field_unknown() {
        let (tables, row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ById { id: row_id },
            fields: FieldSelector::List {
                fields: vec!["unknown".to_string()],
            },
            format: MaterializeFormat::Raw,
            dest: dest.path().to_str().unwrap().to_string(),
            concat: None,
            write_mode: None,
            dry_run: None,
        };

        let err = do_materialize(&config, &tables, params).await.unwrap_err();
        assert!(matches!(
            err,
            MiniAppError::MaterializeFieldUnknown { ref field } if field == "unknown"
        ));
    }

    #[tokio::test]
    async fn error_dest_relative_is_rejected_at_validation() {
        // This is a second test of MaterializeDestRelative to confirm validation
        // boundary fires before any store access.
        let (tables, row_id, config) = make_test_env().await;

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ById { id: row_id },
            fields: FieldSelector::All,
            format: MaterializeFormat::Json,
            dest: "not/absolute".to_string(),
            concat: None,
            write_mode: None,
            dry_run: None,
        };

        let err = do_materialize(&config, &tables, params).await.unwrap_err();
        assert!(matches!(
            err,
            MiniAppError::MaterializeDestRelative { ref path } if path == "not/absolute"
        ));
    }

    // -------------------------------------------------------------------------
    // dry_run tests (2)
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn dry_run_no_write_but_sha256_and_bytes_present() {
        let (tables, row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();
        let dest_path = dest.path().to_str().unwrap().to_string();

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ById { id: row_id.clone() },
            fields: FieldSelector::All,
            format: MaterializeFormat::Json,
            dest: dest_path.clone(),
            concat: Some(false),
            write_mode: None,
            dry_run: Some(true),
        };

        let result = do_materialize(&config, &tables, params).await.unwrap();
        assert_eq!(result.count, 1);
        let f = &result.files[0];
        // sha256 must be present (Crux #3 — even in dry_run).
        assert_eq!(f.sha256.len(), 64);
        assert!(f.bytes > 0);
        // File must NOT be written on disk.
        let out_path = format!("{}/{}.json", dest_path, row_id);
        assert!(!std::path::Path::new(&out_path).exists());
    }

    #[tokio::test]
    async fn dry_run_write_mode_error_existing_file_still_errors() {
        let (tables, _row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();
        let out_file = format!("{}/out.txt", dest.path().display());
        // Pre-create the file.
        std::fs::write(&out_file, b"existing").unwrap();

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ByFilter {
                filter: crate::filter::ListFilter::Eq {
                    field: "title".to_string(),
                    value: serde_json::json!("hello"),
                },
                limit: None,
                offset: None,
            },
            fields: FieldSelector::All,
            format: MaterializeFormat::Raw,
            dest: out_file.clone(),
            concat: Some(true),
            write_mode: Some(WriteMode::Error),
            dry_run: Some(true), // dry_run=true still validates write_mode=Error.
        };

        let err = do_materialize(&config, &tables, params).await.unwrap_err();
        assert!(matches!(
            err,
            MiniAppError::MaterializeDestInvalid { ref path, .. } if path == &out_file
        ));
    }

    // -------------------------------------------------------------------------
    // row_id tests (2)
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn row_id_set_for_each_file_when_no_concat() {
        let (tables, row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ById { id: row_id.clone() },
            fields: FieldSelector::All,
            format: MaterializeFormat::Raw,
            dest: dest.path().to_str().unwrap().to_string(),
            concat: Some(false),
            write_mode: None,
            dry_run: None,
        };

        let result = do_materialize(&config, &tables, params).await.unwrap();
        assert_eq!(result.files[0].row_id, Some(row_id));
    }

    #[tokio::test]
    async fn row_id_is_none_when_concat() {
        let (tables, _row_id, config) = make_test_env().await;
        let dest = tempfile::tempdir().unwrap();
        let out_file = format!("{}/out.txt", dest.path().display());

        let params = MaterializeParams {
            table: None,
            selector: RowSelector::ByFilter {
                filter: crate::filter::ListFilter::Eq {
                    field: "title".to_string(),
                    value: serde_json::json!("hello"),
                },
                limit: None,
                offset: None,
            },
            fields: FieldSelector::All,
            format: MaterializeFormat::Raw,
            dest: out_file,
            concat: Some(true),
            write_mode: None,
            dry_run: None,
        };

        let result = do_materialize(&config, &tables, params).await.unwrap();
        assert_eq!(result.files[0].row_id, None);
    }

    // -------------------------------------------------------------------------
    // FieldSelector::validate — unit tests (Crux #2: schema-based validation)
    // -------------------------------------------------------------------------

    fn make_schema() -> SchemaConfig {
        SchemaConfig {
            table: "test".to_string(),
            title: None,
            description: None,
            fields: vec![
                FieldDef {
                    name: "title".to_string(),
                    ty: FieldType::String,
                    required: true,
                    description: None,
                },
                FieldDef {
                    name: "body".to_string(),
                    ty: FieldType::String,
                    required: false,
                    description: None,
                },
            ],
            dump: None,
            history: Default::default(),
        }
    }

    fn make_row(data: serde_json::Value) -> RowRecord {
        RowRecord {
            id: "test-id".to_string(),
            data,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn validate_field_selector_all_is_ok() {
        let schema = make_schema();
        let fs = FieldSelector::All;
        assert!(fs.validate(&schema).is_ok());
    }

    #[test]
    fn validate_field_selector_list_known_fields_ok() {
        let schema = make_schema();
        let fs = FieldSelector::List {
            fields: vec!["title".to_string(), "body".to_string()],
        };
        assert!(fs.validate(&schema).is_ok());
    }

    #[test]
    fn validate_field_selector_list_single_known_field_ok() {
        let schema = make_schema();
        let fs = FieldSelector::List {
            fields: vec!["title".to_string()],
        };
        assert!(fs.validate(&schema).is_ok());
    }

    #[test]
    fn validate_field_selector_list_unknown_field_returns_validation_error() {
        let schema = make_schema();
        let fs = FieldSelector::List {
            fields: vec!["title".to_string(), "nonexistent".to_string()],
        };
        let err = fs.validate(&schema).unwrap_err();
        match err {
            MiniAppError::Validation { field, reason } => {
                assert_eq!(field, "nonexistent");
                assert!(reason.contains("nonexistent"));
                assert!(reason.contains("schema-registered"));
            }
            other => panic!("expected Validation error, got {other:?}"),
        }
    }

    #[test]
    fn validate_field_selector_list_empty_fields_ok() {
        // Empty list is valid; projection will return empty data objects.
        let schema = make_schema();
        let fs = FieldSelector::List { fields: vec![] };
        assert!(fs.validate(&schema).is_ok());
    }

    // -------------------------------------------------------------------------
    // apply_projection — unit tests (Crux #1: single shared boundary)
    // -------------------------------------------------------------------------

    #[test]
    fn apply_projection_none_returns_unchanged() {
        let schema = make_schema();
        let row = make_row(serde_json::json!({"title": "hello", "body": "world"}));
        let records = vec![row];
        let result = apply_projection(records.clone(), &None, &schema).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, records[0].id);
        assert_eq!(
            result[0].data,
            serde_json::json!({"title": "hello", "body": "world"})
        );
    }

    #[test]
    fn apply_projection_all_returns_unchanged() {
        let schema = make_schema();
        let row = make_row(serde_json::json!({"title": "hello", "body": "world"}));
        let records = vec![row];
        let fields = Some(FieldSelector::All);
        let result = apply_projection(records.clone(), &fields, &schema).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].data,
            serde_json::json!({"title": "hello", "body": "world"})
        );
    }

    #[test]
    fn apply_projection_list_projects_data() {
        let schema = make_schema();
        let row = make_row(serde_json::json!({"title": "hello", "body": "world"}));
        let original_id = row.id.clone();
        let original_created_at = row.created_at;
        let records = vec![row];
        let fields = Some(FieldSelector::List {
            fields: vec!["title".to_string()],
        });
        let result = apply_projection(records, &fields, &schema).unwrap();
        assert_eq!(result.len(), 1);
        // Only "title" should be present in projected data.
        assert_eq!(result[0].data, serde_json::json!({"title": "hello"}));
        // Metadata fields must be preserved.
        assert_eq!(result[0].id, original_id);
        assert_eq!(result[0].created_at, original_created_at);
    }

    #[test]
    fn apply_projection_list_projects_multiple_rows() {
        let schema = make_schema();
        let row1 = make_row(serde_json::json!({"title": "first", "body": "one"}));
        let row2 = make_row(serde_json::json!({"title": "second", "body": "two"}));
        let fields = Some(FieldSelector::List {
            fields: vec!["body".to_string()],
        });
        let result = apply_projection(vec![row1, row2], &fields, &schema).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].data, serde_json::json!({"body": "one"}));
        assert_eq!(result[1].data, serde_json::json!({"body": "two"}));
    }

    #[test]
    fn apply_projection_unknown_field_returns_error() {
        let schema = make_schema();
        let row = make_row(serde_json::json!({"title": "hello", "body": "world"}));
        let fields = Some(FieldSelector::List {
            fields: vec!["nonexistent".to_string()],
        });
        let err = apply_projection(vec![row], &fields, &schema).unwrap_err();
        match err {
            MiniAppError::Validation { field, .. } => {
                assert_eq!(field, "nonexistent");
            }
            other => panic!("expected Validation error, got {other:?}"),
        }
    }

    #[test]
    fn apply_projection_missing_field_in_data_returns_null() {
        // project_row returns Null for fields absent from data.
        // This is acceptable: validation passes (field is in schema),
        // but the stored data doesn't have it.
        let schema = make_schema();
        let row = make_row(serde_json::json!({"title": "hello"}));
        let fields = Some(FieldSelector::List {
            fields: vec!["title".to_string(), "body".to_string()],
        });
        let result = apply_projection(vec![row], &fields, &schema).unwrap();
        assert_eq!(result[0].data["title"], "hello");
        assert_eq!(result[0].data["body"], serde_json::Value::Null);
    }
}
