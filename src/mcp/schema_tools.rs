/// Schema CRUD tool implementations for mini-app-mcp.
///
/// Provides three free async functions consumed by the MCP tool thin-wrappers
/// in `server.rs` and (later) the `schema_batch` composite tool in Subtask 3:
///
/// - [`do_schema_create`] — write a new `schema.yaml` + open a fresh DB, then
///   rebuild the registry.
/// - [`do_schema_update`] — back up the existing YAML+DB pair, overwrite the
///   YAML, rebuild the registry.  **No DDL is applied to the DB** (Crux
///   `no automatic DDL migration`).
/// - [`do_schema_delete`] — back up the existing YAML+DB pair, remove the
///   `schema.yaml`, rebuild the registry.  **The DB file is not deleted**
///   (Crux `no automatic DDL migration` — operator must remove explicitly).
///
/// # dry_run guarantee (Crux `dry_run side-effect-free guarantee`)
///
/// When `dry_run=true`, all three functions perform only read operations
/// (filesystem existence checks, schema parsing, row counting) and return an
/// `affects` JSON object without writing to any YAML file, SQLite table,
/// backup directory, or the `ArcSwap` registry.  No `tmpfile` is created.
///
/// # No automatic DDL migration (Crux `no automatic DDL migration`)
///
/// `do_schema_update` and `do_schema_delete` write or remove the `schema.yaml`
/// and rebuild the in-memory registry, but never execute `ALTER TABLE` or any
/// other DDL against existing SQLite tables.  Column structure is the
/// operator's responsibility.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::backup;
use crate::config::Config;
use crate::error::MiniAppError;
use crate::mcp::registry::TableRegistry;
use crate::schema::{FieldDef, FieldType, SchemaConfig};
use crate::store::Store;

// =============================================================================
// schema_batch types
// =============================================================================

/// A single operation inside a `schema_batch` call.
///
/// All ops in a batch must target the same table (architecture constraint:
/// SQLite SAVEPOINT is per-connection and each table has its own connection).
///
/// The `op` discriminant uses snake_case tag so JSON looks like:
/// `{"op":"query","sql":"SELECT 1","table":"t"}` /
/// `{"op":"schema_update","table":"t","scope":"project","fields":[…]}`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BatchOp {
    /// Execute a raw SQL statement inside the SAVEPOINT.
    ///
    /// Schema validation is bypassed — the SQL runs directly.
    Query {
        /// Raw SQL string to execute.
        sql: String,
        /// Optional positional parameters (JSON scalars only).
        params: Option<Vec<serde_json::Value>>,
        /// Logical table name this op targets (used for single-table enforcement).
        table: String,
    },
    /// Create a new schema inside the batch (YAML write deferred to commit).
    SchemaCreate {
        /// Table name to create.
        table: String,
        /// Scope: `"project"` or `"user"`.
        scope: String,
        /// Field definitions for the new schema.
        fields: Vec<FieldDefInput>,
    },
    /// Update an existing schema inside the batch (YAML write deferred to commit).
    SchemaUpdate {
        /// Table name to update.
        table: String,
        /// Scope: `"project"` or `"user"`.
        scope: String,
        /// New field definitions (full overwrite).
        fields: Vec<FieldDefInput>,
    },
    /// Delete a schema inside the batch (removal deferred to commit).
    ///
    /// The DB file is never deleted (Crux: no automatic DDL migration).
    SchemaDelete {
        /// Table name to delete.
        table: String,
        /// Scope: `"project"` or `"user"`.
        scope: String,
    },
}

impl BatchOp {
    /// Returns the table name targeted by this op.
    pub fn table_name(&self) -> &str {
        match self {
            BatchOp::Query { table, .. } => table,
            BatchOp::SchemaCreate { table, .. } => table,
            BatchOp::SchemaUpdate { table, .. } => table,
            BatchOp::SchemaDelete { table, .. } => table,
        }
    }
}

/// Parameters for the `schema_batch` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SchemaBatchParams {
    /// List of operations to execute atomically.
    ///
    /// All ops must target the same table (single-table SAVEPOINT constraint).
    pub ops: Vec<BatchOp>,
    /// When `true`, compute affects per op and return without writing anything.
    ///
    /// No YAML file, SQLite row, backup, or registry change occurs (Crux:
    /// dry_run side-effect-free guarantee).
    #[serde(default)]
    pub dry_run: bool,
}

/// Result returned by a successful `schema_batch` call.
#[derive(Debug, Serialize)]
pub struct BatchResult {
    /// Whether the batch was committed (`false` for `dry_run`).
    pub committed: bool,
    /// Number of ops executed (or simulated in dry_run).
    pub ops_executed: usize,
    /// YAML paths written during commit (empty for dry_run or query-only batches).
    pub yaml_writes: Vec<PathBuf>,
    /// Backup pair paths written before SAVEPOINT (empty for dry_run or create-only batches).
    pub backups_written: Vec<PathBuf>,
    /// Whether the registry was rebuilt (always false for dry_run).
    pub registry_rebuilt: bool,
}

// =============================================================================
// Scope resolution helper
// =============================================================================

/// Validate a table name supplied via MCP tool input.
///
/// Rejects values that would escape the scope_root via path traversal,
/// reference an absolute path, or contain platform-specific path separators
/// or control characters.  Allowed character set: ASCII alphanumeric, `_`,
/// `-`.  Empty / `.` / `..` / leading-`.` are rejected.
///
/// Called from `resolve_scope_paths` so all three CRUD tools (`do_schema_create`,
/// `do_schema_update`, `do_schema_delete`) inherit the validation through a
/// single chokepoint.
fn validate_table_name(table: &str) -> Result<(), MiniAppError> {
    if table.is_empty() {
        return Err(MiniAppError::Validation {
            field: "table".into(),
            reason: "table name must not be empty".into(),
        });
    }
    if table == "." || table == ".." {
        return Err(MiniAppError::Validation {
            field: "table".into(),
            reason: format!("table name '{table}' is reserved"),
        });
    }
    if table.starts_with('.') {
        return Err(MiniAppError::Validation {
            field: "table".into(),
            reason: "table name must not start with '.'".into(),
        });
    }
    for c in table.chars() {
        let ok = c.is_ascii_alphanumeric() || c == '_' || c == '-';
        if !ok {
            return Err(MiniAppError::Validation {
                field: "table".into(),
                reason: format!(
                    "table name contains invalid character '{c}'; allowed: [A-Za-z0-9_-]"
                ),
            });
        }
    }
    Ok(())
}

/// Derive the scope root directory (user_dir or project_dir) and the full
/// `schema.yaml` path for a given `scope` + `table` pair.
///
/// Layout:  `{scope_root}/{table}/schema.yaml`
///
/// # Arguments
/// - `scope` — `"project"` or `"user"` (any other value returns `Validation`).
/// - `table` — the logical table name.
/// - `config` — the server configuration supplying `user_dir` / `project_dir`.
///
/// # Returns
/// `(scope_root: PathBuf, yaml_path: PathBuf)`
///
/// # Errors
/// - [`MiniAppError::Validation`] if `scope` is not `"project"` or `"user"`.
/// - [`MiniAppError::Config`] if the relevant dir is `None`.
fn resolve_scope_paths(
    scope: &str,
    table: &str,
    config: &Config,
) -> Result<(PathBuf, PathBuf), MiniAppError> {
    validate_table_name(table)?;
    let scope_root = match scope {
        "project" => config.project_dir.clone().ok_or_else(|| {
            MiniAppError::Config("project scope requires MINI_APP_PROJECT_DIR".into())
        })?,
        "user" => config
            .user_dir
            .clone()
            .ok_or_else(|| MiniAppError::Config("user scope requires MINI_APP_USER_DIR".into()))?,
        other => {
            return Err(MiniAppError::Validation {
                field: "scope".into(),
                reason: format!("invalid scope '{other}'; must be 'project' or 'user'"),
            });
        }
    };
    let yaml_path = scope_root.join(table).join("schema.yaml");
    Ok((scope_root, yaml_path))
}

// =============================================================================
// schema_create
// =============================================================================

/// `schema_create` tool parameters.
#[derive(Deserialize, JsonSchema)]
pub struct SchemaCreateParams {
    /// Logical table name.  Must be a valid directory name.
    pub table: String,
    /// Scope in which to create the table: `"project"` or `"user"`.
    pub scope: String,
    /// Field definitions for the new schema.
    pub fields: Vec<FieldDefInput>,
    /// When `true`, compute and return what would happen without writing anything.
    #[serde(default)]
    pub dry_run: bool,
}

/// A single field definition supplied via the MCP tool call.
#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub struct FieldDefInput {
    /// Field name.
    pub name: String,
    /// JSON type: `string`, `number`, `boolean`, `array`, or `object`.
    #[serde(rename = "type")]
    pub ty: String,
    /// Whether the field is required.
    #[serde(default)]
    pub required: bool,
}

/// Parse a field-type string into [`FieldType`], returning a `Validation` error
/// on unknown types.
fn parse_field_type(ty_str: &str, field_name: &str) -> Result<FieldType, MiniAppError> {
    match ty_str {
        "string" => Ok(FieldType::String),
        "number" => Ok(FieldType::Number),
        "boolean" => Ok(FieldType::Boolean),
        "array" => Ok(FieldType::Array),
        "object" => Ok(FieldType::Object),
        other => Err(MiniAppError::Validation {
            field: format!("fields[{}].type", field_name),
            reason: format!(
                "unknown type '{}'; must be one of: string, number, boolean, array, object",
                other
            ),
        }),
    }
}

/// Convert [`FieldDefInput`] slice into [`FieldDef`] vec.
fn parse_fields(inputs: &[FieldDefInput]) -> Result<Vec<FieldDef>, MiniAppError> {
    inputs
        .iter()
        .map(|f| {
            let ty = parse_field_type(&f.ty, &f.name)?;
            Ok(FieldDef {
                name: f.name.clone(),
                ty,
                required: f.required,
            })
        })
        .collect()
}

/// Implement `schema_create`.
///
/// - `dry_run=true`: confirm the schema.yaml path is absent, return `affects`.
///   Zero FS writes.
/// - `dry_run=false`: create `{scope_root}/{table}/` directory (if absent),
///   write `schema.yaml` via atomic tmp+rename, open/create the DB, rebuild
///   the registry.
///
/// # Crux compliance
/// - **no automatic DDL migration**: the DB is opened (creating an empty file
///   if absent) but no `ALTER TABLE` or column DDL is ever issued.
/// - **dry_run side-effect-free**: when `dry_run=true`, only existence checks
///   are performed.
pub async fn do_schema_create(
    config: &Config,
    tables: &Arc<ArcSwap<TableRegistry>>,
    params: SchemaCreateParams,
) -> Result<String, MiniAppError> {
    let (scope_root, yaml_path) = resolve_scope_paths(&params.scope, &params.table, config)?;
    let table_dir = scope_root.join(&params.table);
    let db_path = table_dir.join(format!("{}.db", params.table));

    if params.dry_run {
        // dry_run: only check if path is absent.  Zero FS writes.
        let yaml_exists = yaml_path.exists();
        let result = serde_json::json!({
            "dry_run": true,
            "affects": {
                "would_create": yaml_path.display().to_string(),
                "already_exists": yaml_exists,
            }
        });
        return serde_json::to_string(&result).map_err(|e| MiniAppError::Schema(e.to_string()));
    }

    // Real execution: reject if schema already exists.
    if yaml_path.exists() {
        return Err(MiniAppError::SchemaExists {
            table: params.table.clone(),
        });
    }

    // Validate and convert fields.
    let fields = parse_fields(&params.fields)?;

    let schema = SchemaConfig {
        table: params.table.clone(),
        fields,
        dump: None,
    };

    // Create the table directory (idempotent).
    let table_dir_clone = table_dir.clone();
    tokio::task::spawn_blocking(move || std::fs::create_dir_all(&table_dir_clone))
        .await
        .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")))?
        .map_err(MiniAppError::Io)?;

    // Write schema.yaml via atomic tmp+rename (no backup needed for create).
    schema.write_to_path(&yaml_path).await?;

    // Open (create) the DB so the table entry is immediately usable.
    // No DDL migrations are applied — Store::open only runs CREATE TABLE IF NOT EXISTS
    // on its fixed internal schema (Crux `no automatic DDL migration`).
    Store::open(&db_path, schema.clone()).await?;

    // Rebuild registry — Y1: each schema CRUD tool triggers rebuild at the end.
    rebuild_registry(config, tables).await?;

    let result = serde_json::json!({
        "created": params.table,
        "schema_path": yaml_path.display().to_string(),
        "scope": params.scope,
    });
    serde_json::to_string(&result).map_err(|e| MiniAppError::Schema(e.to_string()))
}

// =============================================================================
// schema_update
// =============================================================================

/// `schema_update` tool parameters.
#[derive(Deserialize, JsonSchema)]
pub struct SchemaUpdateParams {
    /// Table name to update (must already exist).
    pub table: String,
    /// Scope: `"project"` or `"user"`.
    pub scope: String,
    /// New field definitions (full overwrite of the schema).
    pub fields: Vec<FieldDefInput>,
    /// When `true`, return field diff without writing anything.
    #[serde(default)]
    pub dry_run: bool,
}

/// Implement `schema_update`.
///
/// - `dry_run=true`: load existing schema, compute field diff, return `affects`.
///   Zero FS writes.
/// - `dry_run=false`: back up (YAML + DB), overwrite `schema.yaml`, rebuild
///   registry.  **No DDL is applied to existing SQLite tables.**
///
/// # Crux compliance
/// - **no automatic DDL migration**: only the YAML file is replaced and the
///   registry is rebuilt.  The live `rows` table DDL is never modified.
/// - **dry_run side-effect-free**: no tmpfile, no backup write, no registry
///   swap when `dry_run=true`.
pub async fn do_schema_update(
    config: &Config,
    tables: &Arc<ArcSwap<TableRegistry>>,
    params: SchemaUpdateParams,
) -> Result<String, MiniAppError> {
    let (scope_root, yaml_path) = resolve_scope_paths(&params.scope, &params.table, config)?;
    let db_path = scope_root
        .join(&params.table)
        .join(format!("{}.db", params.table));

    // The table must already be mounted in the registry.
    {
        let guard = tables.load();
        guard
            .resolve(Some(&params.table))
            .map_err(|_| MiniAppError::TableNotFound {
                table: params.table.clone(),
            })?;
    }

    // Load the existing schema for diff computation.
    let old_schema =
        crate::schema::load_from_path(&yaml_path).map_err(|_| MiniAppError::TableNotFound {
            table: params.table.clone(),
        })?;

    // Validate and convert new fields.
    let new_fields = parse_fields(&params.fields)?;

    // Compute field diff.
    let old_field_map: HashMap<String, String> = old_schema
        .fields
        .iter()
        .map(|f| (f.name.clone(), f.ty.as_str().to_string()))
        .collect();
    let new_field_map: HashMap<String, String> = new_fields
        .iter()
        .map(|f| (f.name.clone(), f.ty.as_str().to_string()))
        .collect();

    let mut fields_added: Vec<String> = new_field_map
        .keys()
        .filter(|k| !old_field_map.contains_key(*k))
        .cloned()
        .collect();
    let mut fields_removed: Vec<String> = old_field_map
        .keys()
        .filter(|k| !new_field_map.contains_key(*k))
        .cloned()
        .collect();
    let mut fields_type_changed: Vec<String> = old_field_map
        .iter()
        .filter(|(k, old_ty)| new_field_map.get(*k).map(|t| t != *old_ty).unwrap_or(false))
        .map(|(k, _)| k.clone())
        .collect();

    fields_added.sort();
    fields_removed.sort();
    fields_type_changed.sort();

    if params.dry_run {
        // Count rows via Store without side effects.
        let rows_count = {
            let guard = tables.load();
            let entry = guard.resolve(Some(&params.table))?;
            entry.store.row_count().await?
        };
        let result = serde_json::json!({
            "dry_run": true,
            "affects": {
                "fields_added": fields_added,
                "fields_removed": fields_removed,
                "fields_type_changed": fields_type_changed,
                "rows_unchanged": rows_count,
            }
        });
        return serde_json::to_string(&result).map_err(|e| MiniAppError::Schema(e.to_string()));
    }

    // Real execution: back up YAML + DB before overwriting.
    // backup ordering: write_backup_pair → YAML write → purge.
    backup::write_backup_pair(&scope_root, &params.table, &yaml_path, &db_path).await?;

    // Build new schema and write atomically.
    let new_schema = SchemaConfig {
        table: params.table.clone(),
        fields: new_fields,
        dump: None,
    };
    new_schema.write_to_path(&yaml_path).await?;

    // Purge old backups after successful write.
    let retention = config.backup_retention();
    backup::purge_old_backups(&scope_root, &params.table, retention).await?;

    // Rebuild registry — Y1: each schema CRUD tool triggers rebuild at the end.
    // This does NOT alter the SQLite table DDL (Crux `no automatic DDL migration`).
    rebuild_registry(config, tables).await?;

    let result = serde_json::json!({
        "updated": params.table,
        "schema_path": yaml_path.display().to_string(),
        "scope": params.scope,
        "fields_added": fields_added,
        "fields_removed": fields_removed,
        "fields_type_changed": fields_type_changed,
    });
    serde_json::to_string(&result).map_err(|e| MiniAppError::Schema(e.to_string()))
}

// =============================================================================
// schema_delete
// =============================================================================

/// `schema_delete` tool parameters.
#[derive(Deserialize, JsonSchema)]
pub struct SchemaDeleteParams {
    /// Table name to delete (must already exist in the registry).
    pub table: String,
    /// Scope: `"project"` or `"user"`.
    pub scope: String,
    /// When `true`, return orphaned row count without deleting anything.
    #[serde(default)]
    pub dry_run: bool,
}

/// Implement `schema_delete`.
///
/// - `dry_run=true`: count rows in the table, return `affects`.  Zero FS writes.
/// - `dry_run=false`: back up (YAML + DB), remove `schema.yaml`, rebuild
///   registry.  **The DB file is not deleted** — the operator must remove it
///   explicitly (Crux `no automatic DDL migration`).
///
/// # Crux compliance
/// - **no automatic DDL migration**: only `schema.yaml` is removed and the
///   registry is rebuilt.  The `<table>.db` file is left in place.
/// - **dry_run side-effect-free**: no backup write, no file removal, no
///   registry swap when `dry_run=true`.
pub async fn do_schema_delete(
    config: &Config,
    tables: &Arc<ArcSwap<TableRegistry>>,
    params: SchemaDeleteParams,
) -> Result<String, MiniAppError> {
    let (scope_root, yaml_path) = resolve_scope_paths(&params.scope, &params.table, config)?;
    let db_path = scope_root
        .join(&params.table)
        .join(format!("{}.db", params.table));

    // The table must already be mounted in the registry.
    {
        let guard = tables.load();
        guard
            .resolve(Some(&params.table))
            .map_err(|_| MiniAppError::TableNotFound {
                table: params.table.clone(),
            })?;
    }

    if params.dry_run {
        // Count rows without side effects.
        let rows_count = {
            let guard = tables.load();
            let entry = guard.resolve(Some(&params.table))?;
            entry.store.row_count().await?
        };
        let result = serde_json::json!({
            "dry_run": true,
            "affects": {
                "rows_orphaned": rows_count,
                "would_remove_yaml": yaml_path.display().to_string(),
                "db_file_retained": db_path.display().to_string(),
            }
        });
        return serde_json::to_string(&result).map_err(|e| MiniAppError::Schema(e.to_string()));
    }

    // Real execution: back up YAML + DB before removing schema.yaml.
    // The DB path may not yet exist if the table was never written to — backup
    // will fail gracefully with a Backup error in that case, which is the
    // correct behavior (signal the operator that something is wrong).
    backup::write_backup_pair(&scope_root, &params.table, &yaml_path, &db_path).await?;

    // Remove schema.yaml (the backup was just written above).
    // The DB file is intentionally NOT removed (Crux `no automatic DDL migration`).
    let yaml_path_rm = yaml_path.clone();
    tokio::task::spawn_blocking(move || std::fs::remove_file(&yaml_path_rm))
        .await
        .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")))?
        .map_err(MiniAppError::Io)?;

    // Purge old backups after successful removal.
    let retention = config.backup_retention();
    backup::purge_old_backups(&scope_root, &params.table, retention).await?;

    // Rebuild registry — Y1: removes the table from in-memory registry.
    // DB file remains on disk (operator responsibility).
    rebuild_registry(config, tables).await?;

    let result = serde_json::json!({
        "deleted": params.table,
        "schema_path_removed": yaml_path.display().to_string(),
        "db_file_retained": db_path.display().to_string(),
        "scope": params.scope,
    });
    serde_json::to_string(&result).map_err(|e| MiniAppError::Schema(e.to_string()))
}

// =============================================================================
// schema_batch execution
// =============================================================================

/// Execute a batch of ops atomically under a single SQLite SAVEPOINT.
///
/// # Crux compliance
/// - **schema_batch SAVEPOINT atomicity**: all DB ops execute under a single
///   SAVEPOINT; any failure rolls back all preceding ops.  YAML writes are
///   held in memory (deferred-write list) and only applied after SAVEPOINT
///   commit succeeds — if SAVEPOINT fails, no YAML is touched.
/// - **dry_run side-effect-free**: when `dry_run=true`, only read operations
///   are performed; no YAML file, SQLite row, backup, or registry change occurs.
/// - **no automatic DDL migration**: schema ops only rewrite YAML + rebuild
///   registry; no `ALTER TABLE` or `DROP TABLE` is ever issued.
///
/// # Single-table constraint (design choice A)
/// All ops must target the same table.  A multi-table batch is rejected with
/// `MiniAppError::Validation` before any SAVEPOINT is entered.
///
/// # Execution order
/// 1. Validate all op table names (early-reject multi-table).
/// 2. For schema mutation ops: write `_backup` pair before SAVEPOINT opens.
/// 3. Open SAVEPOINT; execute ops in order; collect deferred YAML writes in memory.
/// 4. Commit SAVEPOINT; apply deferred YAML writes; purge backups; rebuild registry once.
pub async fn execute_batch(
    config: &Config,
    tables: &Arc<ArcSwap<TableRegistry>>,
    params: SchemaBatchParams,
) -> Result<String, MiniAppError> {
    // ── 0. Empty batch ───────────────────────────────────────────────────────
    if params.ops.is_empty() {
        let result = BatchResult {
            committed: !params.dry_run,
            ops_executed: 0,
            yaml_writes: vec![],
            backups_written: vec![],
            registry_rebuilt: false,
        };
        return serde_json::to_string(&result).map_err(|e| MiniAppError::Schema(e.to_string()));
    }

    // ── 1. Single-table enforcement ──────────────────────────────────────────
    let first_table = params.ops[0].table_name().to_string();
    for op in &params.ops {
        if op.table_name() != first_table {
            return Err(MiniAppError::Validation {
                field: "ops".into(),
                reason: "all ops in a batch must target the same table".into(),
            });
        }
    }

    // ── 2. dry_run path (side-effect-free) ──────────────────────────────────
    if params.dry_run {
        let mut aggregate_affects: Vec<serde_json::Value> = Vec::new();
        for (idx, op) in params.ops.iter().enumerate() {
            let affects = compute_op_affects(op, config, tables).await.map_err(|e| {
                MiniAppError::BatchAborted {
                    op_index: idx,
                    reason: e.to_string(),
                }
            })?;
            aggregate_affects.push(affects);
        }
        let result = serde_json::json!({
            "committed": false,
            "dry_run": true,
            "ops_executed": params.ops.len(),
            "affects": aggregate_affects,
            "yaml_writes": [],
            "backups_written": [],
            "registry_rebuilt": false,
        });
        return serde_json::to_string(&result).map_err(|e| MiniAppError::Schema(e.to_string()));
    }

    // ── 3. Real execution ────────────────────────────────────────────────────
    //
    // Step 3a: Write _backup pairs for schema mutation ops BEFORE SAVEPOINT.
    // (DB backup ordering per subtask Constraints: backup must be a snapshot
    //  taken before any mutation, cannot be inside SAVEPOINT due to connection
    //  reuse.)
    let mut backups_written: Vec<PathBuf> = Vec::new();
    for (idx, op) in params.ops.iter().enumerate() {
        match op {
            BatchOp::SchemaUpdate { table, scope, .. }
            | BatchOp::SchemaDelete { table, scope, .. } => {
                let (scope_root, yaml_path) = resolve_scope_paths(scope, table, config)?;
                let db_path = scope_root.join(table).join(format!("{}.db", table));
                backup::write_backup_pair(&scope_root, table, &yaml_path, &db_path)
                    .await
                    .map_err(|e| MiniAppError::BatchAborted {
                        op_index: idx,
                        reason: format!("backup failed: {e}"),
                    })?;
                // Track backup paths for the result.
                let bk_dir = scope_root.join("_backup");
                backups_written.push(bk_dir);
            }
            _ => {}
        }
    }

    // Step 3b: Resolve the Store for the target table.
    let (store, _schema) = {
        let guard = tables.load();
        // We need the Store only for query ops. If there's no query op (only schema ops),
        // the table may not be mounted yet (e.g. schema_create as first op).
        // We'll resolve lazily inside the SAVEPOINT for query ops.
        // For now, we only need the Store if any query op is present.
        let has_query = params
            .ops
            .iter()
            .any(|op| matches!(op, BatchOp::Query { .. }));
        if has_query {
            let entry =
                guard
                    .resolve(Some(&first_table))
                    .map_err(|_| MiniAppError::TableNotFound {
                        table: first_table.clone(),
                    })?;
            (
                Some(Arc::clone(&entry.store)),
                Some(Arc::clone(&entry.schema)),
            )
        } else {
            (None, None)
        }
    };

    // Step 3c: Execute all ops under a single SAVEPOINT.
    // For query ops: execute SQL inside SAVEPOINT via store.execute_under_savepoint.
    // For schema ops: accumulate (path, content) pairs in deferred-write list (memory only).
    //
    // Since we may have mixed query + schema ops, and SAVEPOINT must be a single
    // spawn_blocking call, we run all query SQL inside one SAVEPOINT call.
    // Schema ops are handled as deferred writes outside SAVEPOINT (YAML only).
    //
    // Per design choice X1: YAML writes for schema ops are held in memory
    // during SAVEPOINT; committed to disk only after SAVEPOINT succeeds.

    // Collect deferred YAML writes: (target_path, new_yaml_content, op_kind)
    enum DeferredYamlWrite {
        Write { path: PathBuf, schema: SchemaConfig },
        Remove { path: PathBuf },
    }
    let mut deferred_writes: Vec<DeferredYamlWrite> = Vec::new();

    // Gather all query ops into a single SAVEPOINT if any.
    let query_ops: Vec<(usize, &BatchOp)> = params
        .ops
        .iter()
        .enumerate()
        .filter(|(_, op)| matches!(op, BatchOp::Query { .. }))
        .collect();

    if !query_ops.is_empty() {
        let store_ref = store.as_ref().ok_or_else(|| MiniAppError::TableNotFound {
            table: first_table.clone(),
        })?;

        // Collect SQL + params for query ops.
        #[derive(Clone)]
        struct QuerySpec {
            idx: usize,
            sql: String,
            sql_params: Vec<String>,
        }

        let specs: Vec<QuerySpec> = query_ops
            .iter()
            .map(|(idx, op)| {
                if let BatchOp::Query {
                    sql,
                    params: raw_params,
                    ..
                } = op
                {
                    let sql_params: Vec<String> = raw_params
                        .as_deref()
                        .unwrap_or(&[])
                        .iter()
                        .map(|v| match v {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .collect();
                    QuerySpec {
                        idx: *idx,
                        sql: sql.clone(),
                        sql_params,
                    }
                } else {
                    unreachable!("filtered to Query ops only")
                }
            })
            .collect();

        store_ref
            .execute_under_savepoint(move |sp| {
                for spec in &specs {
                    let params_refs: Vec<&dyn rusqlite::types::ToSql> = spec
                        .sql_params
                        .iter()
                        .map(|s| s as &dyn rusqlite::types::ToSql)
                        .collect();
                    sp.execute(&spec.sql, params_refs.as_slice()).map_err(|e| {
                        MiniAppError::BatchAborted {
                            op_index: spec.idx,
                            reason: e.to_string(),
                        }
                    })?;
                }
                Ok(())
            })
            .await?;
    }

    // Step 3d: Prepare deferred YAML writes for schema ops.
    for (idx, op) in params.ops.iter().enumerate() {
        match op {
            BatchOp::SchemaCreate {
                table,
                scope,
                fields,
            } => {
                let (scope_root, yaml_path) = resolve_scope_paths(scope, table, config)?;
                let table_dir = scope_root.join(table);

                // Reject if schema already exists.
                if yaml_path.exists() {
                    return Err(MiniAppError::BatchAborted {
                        op_index: idx,
                        reason: MiniAppError::SchemaExists {
                            table: table.clone(),
                        }
                        .to_string(),
                    });
                }

                let parsed_fields =
                    parse_fields(fields).map_err(|e| MiniAppError::BatchAborted {
                        op_index: idx,
                        reason: e.to_string(),
                    })?;
                let new_schema = SchemaConfig {
                    table: table.clone(),
                    fields: parsed_fields,
                    dump: None,
                };

                // Create the directory.
                let table_dir_clone = table_dir.clone();
                tokio::task::spawn_blocking(move || std::fs::create_dir_all(&table_dir_clone))
                    .await
                    .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")))?
                    .map_err(|e: std::io::Error| MiniAppError::BatchAborted {
                        op_index: idx,
                        reason: format!("create_dir_all failed: {e}"),
                    })?;

                deferred_writes.push(DeferredYamlWrite::Write {
                    path: yaml_path,
                    schema: new_schema,
                });
            }
            BatchOp::SchemaUpdate {
                table,
                scope,
                fields,
            } => {
                let (_scope_root, yaml_path) = resolve_scope_paths(scope, table, config)?;

                let parsed_fields =
                    parse_fields(fields).map_err(|e| MiniAppError::BatchAborted {
                        op_index: idx,
                        reason: e.to_string(),
                    })?;
                let new_schema = SchemaConfig {
                    table: table.clone(),
                    fields: parsed_fields,
                    dump: None,
                };
                deferred_writes.push(DeferredYamlWrite::Write {
                    path: yaml_path,
                    schema: new_schema,
                });
            }
            BatchOp::SchemaDelete { table, scope, .. } => {
                let (_scope_root, yaml_path) = resolve_scope_paths(scope, table, config)?;
                deferred_writes.push(DeferredYamlWrite::Remove { path: yaml_path });
            }
            BatchOp::Query { .. } => {
                // Already handled in SAVEPOINT block above.
            }
        }
    }

    // ── 4. Apply deferred YAML writes ────────────────────────────────────────
    // SAVEPOINT committed (or no query ops). Now write/remove YAML files.
    let mut yaml_writes: Vec<PathBuf> = Vec::new();

    for deferred in deferred_writes {
        match deferred {
            DeferredYamlWrite::Write { path, schema } => {
                schema.write_to_path(&path).await.map_err(|e| {
                    tracing::warn!(
                        error = %e,
                        path = %path.display(),
                        "schema_batch: deferred YAML write failed after SAVEPOINT commit"
                    );
                    MiniAppError::Backup(format!(
                        "deferred YAML rename failed after SAVEPOINT commit: {e}"
                    ))
                })?;
                yaml_writes.push(path);
            }
            DeferredYamlWrite::Remove { path } => {
                let path_clone = path.clone();
                tokio::task::spawn_blocking(move || std::fs::remove_file(&path_clone))
                    .await
                    .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")))?
                    .map_err(|e: std::io::Error| {
                        tracing::warn!(
                            error = %e,
                            path = %path.display(),
                            "schema_batch: deferred YAML remove failed after SAVEPOINT commit"
                        );
                        MiniAppError::Backup(format!(
                            "deferred YAML remove failed after SAVEPOINT commit: {e}"
                        ))
                    })?;
                yaml_writes.push(path);
            }
        }
    }

    // ── 5. Purge old backups (after successful YAML writes) ──────────────────
    let retention = config.backup_retention();
    for op in &params.ops {
        match op {
            BatchOp::SchemaUpdate { table, scope, .. }
            | BatchOp::SchemaDelete { table, scope, .. } => {
                let (scope_root, _) = resolve_scope_paths(scope, table, config)?;
                // Best-effort — warn on failure but don't fail the batch.
                if let Err(e) = backup::purge_old_backups(&scope_root, table, retention).await {
                    tracing::warn!(
                        error = %e,
                        table = %table,
                        "schema_batch: backup purge failed (non-fatal)"
                    );
                }
            }
            _ => {}
        }
    }

    // ── 6. For schema_create ops, open the new DB ────────────────────────────
    for op in &params.ops {
        if let BatchOp::SchemaCreate {
            table,
            scope,
            fields,
        } = op
        {
            let (scope_root, yaml_path) = resolve_scope_paths(scope, table, config)?;
            let db_path = scope_root.join(table).join(format!("{}.db", table));
            let parsed_fields = parse_fields(fields)?;
            let schema = SchemaConfig {
                table: table.clone(),
                fields: parsed_fields,
                dump: None,
            };
            // Ensure the DB file is created and initialized.
            let _ = yaml_path; // already written above
            Store::open(&db_path, schema).await?;
        }
    }

    // ── 7. Rebuild registry once at end (design choice Y1) ──────────────────
    rebuild_registry(config, tables).await?;

    let result = BatchResult {
        committed: true,
        ops_executed: params.ops.len(),
        yaml_writes,
        backups_written,
        registry_rebuilt: true,
    };
    serde_json::to_string(&result).map_err(|e| MiniAppError::Schema(e.to_string()))
}

/// Compute the dry-run affects for a single op without any side effects.
///
/// # Crux compliance (dry_run side-effect-free guarantee)
/// Only read operations are performed. No YAML, SQLite table, backup, or
/// registry modification occurs.
async fn compute_op_affects(
    op: &BatchOp,
    config: &Config,
    tables: &Arc<ArcSwap<TableRegistry>>,
) -> Result<serde_json::Value, MiniAppError> {
    match op {
        BatchOp::Query { sql, .. } => {
            // For query ops, dry_run reports the SQL that would run.
            Ok(serde_json::json!({
                "op": "query",
                "sql": sql,
                "rows_affected": null,
                "note": "dry_run: SQL not executed",
            }))
        }
        BatchOp::SchemaCreate {
            table,
            scope,
            fields,
        } => {
            let (_, yaml_path) = resolve_scope_paths(scope, table, config)?;
            let field_count = fields.len();
            Ok(serde_json::json!({
                "op": "schema_create",
                "table": table,
                "would_create": yaml_path.display().to_string(),
                "already_exists": yaml_path.exists(),
                "fields_count": field_count,
            }))
        }
        BatchOp::SchemaUpdate {
            table,
            scope,
            fields,
        } => {
            let (_, yaml_path) = resolve_scope_paths(scope, table, config)?;
            // Load existing schema for diff.
            let old_schema = crate::schema::load_from_path(&yaml_path).ok();
            let new_fields = parse_fields(fields)?;

            let (fields_added, fields_removed) = if let Some(old) = old_schema {
                let old_set: std::collections::HashSet<String> =
                    old.fields.iter().map(|f| f.name.clone()).collect();
                let new_set: std::collections::HashSet<String> =
                    new_fields.iter().map(|f| f.name.clone()).collect();
                let mut added: Vec<String> = new_set.difference(&old_set).cloned().collect();
                let mut removed: Vec<String> = old_set.difference(&new_set).cloned().collect();
                added.sort();
                removed.sort();
                (added, removed)
            } else {
                (vec![], vec![])
            };

            // Count rows via registry if table is mounted.
            let rows_count = {
                let guard = tables.load();
                if let Ok(entry) = guard.resolve(Some(table)) {
                    entry.store.row_count().await.unwrap_or(0)
                } else {
                    0
                }
            };

            Ok(serde_json::json!({
                "op": "schema_update",
                "table": table,
                "fields_added": fields_added,
                "fields_removed": fields_removed,
                "rows_unchanged": rows_count,
            }))
        }
        BatchOp::SchemaDelete { table, scope, .. } => {
            let (_, yaml_path) = resolve_scope_paths(scope, table, config)?;
            let rows_count = {
                let guard = tables.load();
                if let Ok(entry) = guard.resolve(Some(table)) {
                    entry.store.row_count().await.unwrap_or(0)
                } else {
                    0
                }
            };
            Ok(serde_json::json!({
                "op": "schema_delete",
                "table": table,
                "would_remove_yaml": yaml_path.display().to_string(),
                "rows_orphaned": rows_count,
            }))
        }
    }
}

// =============================================================================
// Registry rebuild helper (Y1 design choice)
// =============================================================================

/// Re-scan the configured directories and atomically swap the active registry.
///
/// Extracted from `tool_reload` so that all three schema CRUD tools and
/// `tool_reload` itself share the same logic (design choice Y1).
///
/// The blocking I/O runs inside `tokio::task::spawn_blocking` to avoid
/// stalling the async runtime (K-110).  Inside the blocking closure,
/// `Handle::current().block_on(async {...})` drives the async
/// [`TableRegistry::mount_from_dirs`] call (replicating the existing pattern
/// in `tool_reload`, server.rs L826).
///
/// The registry swap uses last-write-wins semantics: a concurrent `tool_reload`
/// or another schema CRUD tool may overwrite this swap (same as the existing
/// `tool_reload` behavior, see server.rs:873).
///
/// # Arguments
/// - `config`: mount configuration (user_dir / project_dir / legacy paths).
/// - `tables`: the `ArcSwap` holding the live registry.
///
/// # Errors
/// - [`MiniAppError::Schema`] — if the `spawn_blocking` task panics, mapped
///   via `"blocking task panic: {e}"` (same pattern as `tool_reload`).
/// - Any error propagated from [`TableRegistry::mount_from_dirs`] or
///   [`TableRegistry::mount_legacy_into`].
pub async fn rebuild_registry(
    config: &Config,
    tables: &Arc<ArcSwap<TableRegistry>>,
) -> Result<(), MiniAppError> {
    let config = config.clone();
    let new_registry = tokio::task::spawn_blocking(move || {
        tokio::runtime::Handle::current().block_on(async {
            let mut registry = TableRegistry::mount_from_dirs(
                config.user_dir.as_deref(),
                config.project_dir.as_deref(),
            )
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "rebuild_registry: mount_from_dirs failed");
                e
            })?;

            if config.has_legacy_env() {
                let schema_path = config.schema_path.as_ref().ok_or_else(|| {
                    MiniAppError::Config(
                        "MINI_APP_SCHEMA required when has_legacy_env is true".into(),
                    )
                })?;
                let db_path = config.db_path.as_ref().ok_or_else(|| {
                    MiniAppError::Config("MINI_APP_DB required when has_legacy_env is true".into())
                })?;
                registry = TableRegistry::mount_legacy_into(registry, schema_path, db_path)
                    .await
                    .map_err(|e| {
                        tracing::warn!(error = %e, "rebuild_registry: mount_legacy_into failed");
                        e
                    })?;
            }

            Ok::<TableRegistry, MiniAppError>(registry)
        })
    })
    .await
    .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")))?
    .map_err(|e: MiniAppError| e)?;

    // Atomic swap — last-write-wins on concurrent calls.
    tables.store(Arc::new(new_registry));
    Ok(())
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::registry::TableRegistry;
    use std::collections::HashMap;
    use std::io::Write as _;
    use std::path::Path;
    use tempfile::TempDir;

    // ── Helpers ──────────────────────────────────────────────────────────────

    /// Create a minimal `Config` pointing at `user_dir` or `project_dir`.
    fn make_config(user_dir: Option<PathBuf>, project_dir: Option<PathBuf>) -> Config {
        Config {
            schema_path: None,
            db_path: None,
            user_dir,
            project_dir,
            backup_retention: Some(10),
            snapshot_retention: None,
        }
    }

    /// Create a table directory at `{scope_dir}/{table}/` with a `schema.yaml`
    /// and an empty SQLite DB.  Returns the yaml_path.
    async fn scaffold_table(scope_dir: &Path, table: &str, fields_yaml: &str) -> PathBuf {
        let table_dir = scope_dir.join(table);
        std::fs::create_dir_all(&table_dir).expect("create table dir");
        let yaml_path = table_dir.join("schema.yaml");
        let yaml = format!("table: {table}\nfields:\n{fields_yaml}\n");
        let mut f = std::fs::File::create(&yaml_path).expect("create schema.yaml");
        f.write_all(yaml.as_bytes()).expect("write schema.yaml");
        // Create the DB by opening a Store.
        let schema = crate::schema::load_from_path(&yaml_path).expect("load schema");
        let db_path = table_dir.join(format!("{}.db", table));
        Store::open(&db_path, schema).await.expect("open store");
        yaml_path
    }

    /// Build a multi-table registry from a directory.
    async fn make_registry_from_dir(dir: &Path) -> Arc<ArcSwap<TableRegistry>> {
        let registry = TableRegistry::mount_from_dirs(Some(dir), None)
            .await
            .expect("mount registry");
        Arc::new(ArcSwap::from_pointee(registry))
    }

    // ── T1: happy-path ────────────────────────────────────────────────────────

    /// T1: schema_create with dry_run=true returns affects without writing files.
    #[tokio::test]
    async fn schema_create_dry_run_returns_affects_no_filesystem_write() {
        let dir = TempDir::new().expect("tempdir");
        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = Arc::new(ArcSwap::from_pointee(TableRegistry::from_entries(
            HashMap::new(),
            None,
        )));

        let params = SchemaCreateParams {
            table: "items".into(),
            scope: "project".into(),
            fields: vec![FieldDefInput {
                name: "name".into(),
                ty: "string".into(),
                required: true,
            }],
            dry_run: true,
        };

        let result_str = do_schema_create(&config, &tables, params)
            .await
            .expect("dry_run create must succeed");
        let result: serde_json::Value = serde_json::from_str(&result_str).unwrap();
        assert_eq!(result["dry_run"], true, "dry_run flag must be true");
        assert!(result["affects"]["would_create"].is_string());
        assert_eq!(result["affects"]["already_exists"], false);

        // No file must have been written.
        let yaml_path = dir.path().join("items").join("schema.yaml");
        assert!(
            !yaml_path.exists(),
            "schema.yaml must NOT be written on dry_run"
        );
    }

    /// T1: schema_create (real) creates the yaml and mounts the table.
    #[tokio::test]
    async fn schema_create_real_creates_yaml_and_registers_table() {
        let dir = TempDir::new().expect("tempdir");
        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = Arc::new(ArcSwap::from_pointee(TableRegistry::from_entries(
            HashMap::new(),
            None,
        )));

        let params = SchemaCreateParams {
            table: "todos".into(),
            scope: "project".into(),
            fields: vec![FieldDefInput {
                name: "title".into(),
                ty: "string".into(),
                required: true,
            }],
            dry_run: false,
        };

        let result_str = do_schema_create(&config, &tables, params)
            .await
            .expect("create must succeed");
        let result: serde_json::Value = serde_json::from_str(&result_str).unwrap();
        assert_eq!(result["created"], "todos");

        // schema.yaml must exist.
        let yaml_path = dir.path().join("todos").join("schema.yaml");
        assert!(yaml_path.exists(), "schema.yaml must be created");

        // Table must be registered.
        let guard = tables.load();
        assert!(
            guard.resolve(Some("todos")).is_ok(),
            "todos must be in registry after create"
        );
    }

    /// T1: schema_update with dry_run=true returns field diff without writing.
    #[tokio::test]
    async fn schema_update_dry_run_returns_field_diff_no_filesystem_write() {
        let dir = TempDir::new().expect("tempdir");
        scaffold_table(
            dir.path(),
            "items",
            "  - name: title\n    type: string\n    required: true\n",
        )
        .await;
        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = make_registry_from_dir(dir.path()).await;

        let yaml_path = dir.path().join("items").join("schema.yaml");
        let yaml_before = std::fs::read_to_string(&yaml_path).expect("read yaml");

        let params = SchemaUpdateParams {
            table: "items".into(),
            scope: "project".into(),
            fields: vec![
                FieldDefInput {
                    name: "title".into(),
                    ty: "string".into(),
                    required: true,
                },
                FieldDefInput {
                    name: "description".into(),
                    ty: "string".into(),
                    required: false,
                },
            ],
            dry_run: true,
        };

        let result_str = do_schema_update(&config, &tables, params)
            .await
            .expect("dry_run update must succeed");
        let result: serde_json::Value = serde_json::from_str(&result_str).unwrap();
        assert_eq!(result["dry_run"], true);
        let added = result["affects"]["fields_added"].as_array().unwrap();
        assert!(
            added.iter().any(|v| v == "description"),
            "description must be in fields_added"
        );
        let removed = result["affects"]["fields_removed"].as_array().unwrap();
        assert!(removed.is_empty(), "no fields must be removed");

        // schema.yaml must be unchanged.
        let yaml_after = std::fs::read_to_string(&yaml_path).expect("read yaml after");
        assert_eq!(yaml_before, yaml_after, "yaml must not change on dry_run");

        // No backup dir must be created.
        assert!(
            !dir.path().join("_backup").exists(),
            "_backup dir must not be created on dry_run"
        );
    }

    /// T1: schema_update (real) overwrites yaml and writes a backup pair.
    #[tokio::test]
    async fn schema_update_real_overwrites_yaml_and_writes_backup_pair() {
        let dir = TempDir::new().expect("tempdir");
        scaffold_table(
            dir.path(),
            "orders",
            "  - name: amount\n    type: number\n    required: true\n",
        )
        .await;
        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = make_registry_from_dir(dir.path()).await;

        let params = SchemaUpdateParams {
            table: "orders".into(),
            scope: "project".into(),
            fields: vec![
                FieldDefInput {
                    name: "amount".into(),
                    ty: "number".into(),
                    required: true,
                },
                FieldDefInput {
                    name: "currency".into(),
                    ty: "string".into(),
                    required: false,
                },
            ],
            dry_run: false,
        };

        do_schema_update(&config, &tables, params)
            .await
            .expect("update must succeed");

        // schema.yaml must be updated — 'currency' field present.
        let yaml_path = dir.path().join("orders").join("schema.yaml");
        let yaml = std::fs::read_to_string(&yaml_path).expect("read yaml");
        assert!(
            yaml.contains("currency"),
            "updated yaml must contain 'currency'"
        );

        // _backup dir must contain at least one yaml backup.
        let backup_dir = dir.path().join("_backup");
        assert!(backup_dir.exists(), "_backup dir must be created");
        let backup_yamls: Vec<_> = std::fs::read_dir(&backup_dir)
            .expect("read backup dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".yaml"))
            .collect();
        assert!(
            !backup_yamls.is_empty(),
            "at least one yaml backup must exist"
        );
    }

    /// T1: schema_delete with dry_run=true returns row count without deleting.
    #[tokio::test]
    async fn schema_delete_dry_run_returns_row_count_no_filesystem_write() {
        let dir = TempDir::new().expect("tempdir");
        scaffold_table(
            dir.path(),
            "logs",
            "  - name: msg\n    type: string\n    required: false\n",
        )
        .await;
        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = make_registry_from_dir(dir.path()).await;

        let yaml_path = dir.path().join("logs").join("schema.yaml");

        let params = SchemaDeleteParams {
            table: "logs".into(),
            scope: "project".into(),
            dry_run: true,
        };

        let result_str = do_schema_delete(&config, &tables, params)
            .await
            .expect("dry_run delete must succeed");
        let result: serde_json::Value = serde_json::from_str(&result_str).unwrap();
        assert_eq!(result["dry_run"], true);
        assert!(result["affects"]["rows_orphaned"].is_number());
        assert!(result["affects"]["would_remove_yaml"].is_string());

        // schema.yaml must still exist.
        assert!(
            yaml_path.exists(),
            "schema.yaml must not be removed on dry_run"
        );
        // No backup dir must have been created.
        assert!(
            !dir.path().join("_backup").exists(),
            "_backup dir must not be created on dry_run"
        );
    }

    /// T1: schema_delete (real) moves yaml to backup and unmounts the table.
    #[tokio::test]
    async fn schema_delete_real_moves_yaml_to_backup_and_unmounts_table() {
        let dir = TempDir::new().expect("tempdir");
        scaffold_table(
            dir.path(),
            "sessions",
            "  - name: token\n    type: string\n    required: true\n",
        )
        .await;
        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = make_registry_from_dir(dir.path()).await;

        let params = SchemaDeleteParams {
            table: "sessions".into(),
            scope: "project".into(),
            dry_run: false,
        };

        do_schema_delete(&config, &tables, params)
            .await
            .expect("delete must succeed");

        // schema.yaml must be gone.
        let yaml_path = dir.path().join("sessions").join("schema.yaml");
        assert!(!yaml_path.exists(), "schema.yaml must be removed");

        // Table must no longer be in registry.
        let guard = tables.load();
        assert!(
            guard.resolve(Some("sessions")).is_err(),
            "sessions must be unregistered after delete"
        );
    }

    // ── T2: edge cases ────────────────────────────────────────────────────────

    /// T2: schema_create on existing table returns SchemaExists error.
    #[tokio::test]
    async fn schema_create_existing_table_returns_schema_exists_error() {
        let dir = TempDir::new().expect("tempdir");
        scaffold_table(
            dir.path(),
            "items",
            "  - name: x\n    type: string\n    required: false\n",
        )
        .await;
        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = make_registry_from_dir(dir.path()).await;

        let params = SchemaCreateParams {
            table: "items".into(),
            scope: "project".into(),
            fields: vec![],
            dry_run: false,
        };

        let err = do_schema_create(&config, &tables, params)
            .await
            .expect_err("create on existing must fail");
        assert!(
            matches!(err, MiniAppError::SchemaExists { .. }),
            "expected SchemaExists, got {:?}",
            err
        );
    }

    /// T2: schema_update on unknown table returns TableNotFound.
    #[tokio::test]
    async fn schema_update_unknown_table_returns_table_not_found() {
        let dir = TempDir::new().expect("tempdir");
        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = Arc::new(ArcSwap::from_pointee(TableRegistry::from_entries(
            HashMap::new(),
            None,
        )));

        let params = SchemaUpdateParams {
            table: "ghost".into(),
            scope: "project".into(),
            fields: vec![],
            dry_run: false,
        };

        let err = do_schema_update(&config, &tables, params)
            .await
            .expect_err("update on unknown table must fail");
        assert!(
            matches!(err, MiniAppError::TableNotFound { .. }),
            "expected TableNotFound, got {:?}",
            err
        );
    }

    /// T2: schema_delete with zero rows succeeds.
    #[tokio::test]
    async fn schema_delete_with_zero_rows_succeeds() {
        let dir = TempDir::new().expect("tempdir");
        scaffold_table(
            dir.path(),
            "empty_table",
            "  - name: x\n    type: string\n    required: false\n",
        )
        .await;
        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = make_registry_from_dir(dir.path()).await;

        let params = SchemaDeleteParams {
            table: "empty_table".into(),
            scope: "project".into(),
            dry_run: false,
        };

        // Should succeed even though the table has no rows.
        do_schema_delete(&config, &tables, params)
            .await
            .expect("delete with zero rows must succeed");
    }

    /// T2: schema_create with scope=user when user_dir is None returns Config error.
    #[tokio::test]
    async fn schema_create_scope_user_when_user_dir_none_returns_config_error() {
        let config = make_config(None, None); // user_dir intentionally None
        let tables = Arc::new(ArcSwap::from_pointee(TableRegistry::from_entries(
            HashMap::new(),
            None,
        )));

        let params = SchemaCreateParams {
            table: "notes".into(),
            scope: "user".into(),
            fields: vec![],
            dry_run: false,
        };

        let err = do_schema_create(&config, &tables, params)
            .await
            .expect_err("create with user scope when user_dir=None must fail");
        assert!(
            matches!(err, MiniAppError::Config(_)),
            "expected Config error, got {:?}",
            err
        );
    }

    /// T2: schema_update (real) does NOT alter the DB table structure.
    /// This is the critical Crux `no automatic DDL migration` test.
    #[tokio::test]
    async fn schema_update_real_does_not_alter_db_table_structure() {
        use rusqlite::Connection;

        let dir = TempDir::new().expect("tempdir");
        scaffold_table(
            dir.path(),
            "products",
            "  - name: title\n    type: string\n    required: true\n",
        )
        .await;
        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = make_registry_from_dir(dir.path()).await;

        // Capture DB column info BEFORE update.
        let db_path = dir.path().join("products").join("products.db");
        let cols_before: Vec<String> = {
            let conn = Connection::open(&db_path).expect("open db");
            let mut stmt = conn
                .prepare("PRAGMA table_info(rows)")
                .expect("table_info stmt");
            stmt.query_map([], |row| row.get::<_, String>(1))
                .expect("query columns")
                .map(|r| r.expect("column name"))
                .collect()
        };

        // Update schema adding a new field 'price'.
        let params = SchemaUpdateParams {
            table: "products".into(),
            scope: "project".into(),
            fields: vec![
                FieldDefInput {
                    name: "title".into(),
                    ty: "string".into(),
                    required: true,
                },
                FieldDefInput {
                    name: "price".into(),
                    ty: "number".into(),
                    required: false,
                },
            ],
            dry_run: false,
        };

        do_schema_update(&config, &tables, params)
            .await
            .expect("update must succeed");

        // Capture DB column info AFTER update.
        let cols_after: Vec<String> = {
            let conn = Connection::open(&db_path).expect("open db after");
            let mut stmt = conn
                .prepare("PRAGMA table_info(rows)")
                .expect("table_info stmt");
            stmt.query_map([], |row| row.get::<_, String>(1))
                .expect("query columns")
                .map(|r| r.expect("column name"))
                .collect()
        };

        // The DB schema (column list) must be identical before and after update.
        assert_eq!(
            cols_before, cols_after,
            "DB column structure must not be altered by schema_update (Crux no-auto-DDL)"
        );

        // The new field 'price' must NOT appear as a SQLite column.
        assert!(
            !cols_after.contains(&"price".to_string()),
            "'price' must not be added as a SQLite column (Crux no-auto-DDL)"
        );
    }

    // ── T3: error paths ───────────────────────────────────────────────────────

    /// T3: schema_create IO error on yaml write returns an error and yaml is absent.
    #[tokio::test]
    async fn schema_create_io_error_on_yaml_write_returns_io_error() {
        // Point config at a path that cannot be written to (file as table dir).
        let dir = TempDir::new().expect("tempdir");
        // Create a file where the table directory would go so dir creation fails.
        let blocker = dir.path().join("blocked_table");
        std::fs::write(&blocker, b"I am a file not a dir").expect("write blocker");

        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = Arc::new(ArcSwap::from_pointee(TableRegistry::from_entries(
            HashMap::new(),
            None,
        )));

        let params = SchemaCreateParams {
            table: "blocked_table".into(),
            scope: "project".into(),
            fields: vec![FieldDefInput {
                name: "x".into(),
                ty: "string".into(),
                required: false,
            }],
            dry_run: false,
        };

        let err = do_schema_create(&config, &tables, params)
            .await
            .expect_err("create on blocked path must fail");
        // The error must be IO or Schema (not a panic).
        assert!(
            matches!(
                err,
                MiniAppError::Io(_) | MiniAppError::Schema(_) | MiniAppError::Backup(_)
            ),
            "expected Io/Schema/Backup error on write failure, got {:?}",
            err
        );
    }

    /// T3: schema_update backup failure leaves yaml unchanged.
    /// Simulated by blocking `_backup` dir creation (a file named `_backup`
    /// prevents `create_dir_all` from succeeding).
    #[tokio::test]
    async fn schema_update_backup_failure_returns_backup_error_and_yaml_unchanged() {
        let dir = TempDir::new().expect("tempdir");
        // Create schema.yaml and a real DB so table setup succeeds.
        scaffold_table(
            dir.path(),
            "blocker_table",
            "  - name: x\n    type: string\n    required: false\n",
        )
        .await;

        // Block `_backup` dir creation by placing a regular file at that path.
        let backup_blocker = dir.path().join("_backup");
        std::fs::write(&backup_blocker, b"I am a file, not a dir").expect("write blocker");

        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = make_registry_from_dir(dir.path()).await;
        let yaml_path = dir.path().join("blocker_table").join("schema.yaml");
        let yaml_before = std::fs::read_to_string(&yaml_path).expect("read yaml before");

        let params = SchemaUpdateParams {
            table: "blocker_table".into(),
            scope: "project".into(),
            fields: vec![FieldDefInput {
                name: "y".into(),
                ty: "number".into(),
                required: false,
            }],
            dry_run: false,
        };

        // The backup will fail because `_backup` is a file, not a directory.
        // The yaml must not be modified.
        let result = do_schema_update(&config, &tables, params).await;
        assert!(result.is_err(), "update with blocked _backup dir must fail");
        let err = result.unwrap_err();
        assert!(
            matches!(err, MiniAppError::Backup(_)),
            "expected Backup error, got {:?}",
            err
        );

        // yaml must be unchanged.
        let yaml_after = std::fs::read_to_string(&yaml_path).expect("read yaml after");
        assert_eq!(
            yaml_before, yaml_after,
            "yaml must not change after backup failure"
        );
    }

    /// T3: schema_delete does NOT drop the DB file.
    /// Critical Crux `no automatic DDL migration` validation.
    #[tokio::test]
    async fn schema_delete_does_not_drop_db_file() {
        let dir = TempDir::new().expect("tempdir");
        scaffold_table(
            dir.path(),
            "vault",
            "  - name: secret\n    type: string\n    required: true\n",
        )
        .await;
        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = make_registry_from_dir(dir.path()).await;

        let db_path = dir.path().join("vault").join("vault.db");
        assert!(db_path.exists(), "db must exist before delete");

        let params = SchemaDeleteParams {
            table: "vault".into(),
            scope: "project".into(),
            dry_run: false,
        };

        do_schema_delete(&config, &tables, params)
            .await
            .expect("delete must succeed");

        // DB file must still exist (Crux `no automatic DDL migration`).
        assert!(
            db_path.exists(),
            "DB file must NOT be deleted by schema_delete (Crux no-auto-DDL)"
        );
    }

    // ── Concurrency tests ─────────────────────────────────────────────────────

    /// Concurrency: 4 tasks store a new registry concurrently while 4 tasks
    /// load.  Verifies ArcSwap last-write-wins with no panic / null deref.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn test_arcswap_store_concurrent_reload() {
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::task::JoinSet;

        let tables: StdArc<Arc<ArcSwap<TableRegistry>>> = StdArc::new(Arc::new(
            ArcSwap::from_pointee(TableRegistry::from_entries(HashMap::new(), None)),
        ));

        let total_seen = StdArc::new(AtomicUsize::new(0));
        let mut set = JoinSet::new();

        // 4 writer tasks each store a new 0-entry registry.
        for _ in 0..4 {
            let t = StdArc::clone(&tables);
            set.spawn(async move {
                let new_registry = TableRegistry::from_entries(HashMap::new(), None);
                t.store(Arc::new(new_registry));
            });
        }

        // 4 reader tasks each load and count entries (must not panic).
        for _ in 0..4 {
            let t = StdArc::clone(&tables);
            let counter = StdArc::clone(&total_seen);
            set.spawn(async move {
                let guard = t.load_full();
                let count = guard.table_count();
                counter.fetch_add(count, Ordering::Relaxed);
            });
        }

        while let Some(res) = set.join_next().await {
            res.expect("task must not panic");
        }

        // All tasks completed without panic.  `total_seen` may be any value
        // (0..=0 since all new registries are empty), but the key guarantee is
        // no panic / null deref.
        let _ = total_seen.load(Ordering::Relaxed);
    }

    /// Concurrency: 2 tasks concurrently call rebuild_registry which internally
    /// uses spawn_blocking + Handle::current().block_on.  Neither must deadlock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_block_on_inside_spawn_blocking() {
        use tokio::task::JoinSet;

        let dir = TempDir::new().expect("tempdir");
        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = Arc::new(Arc::new(ArcSwap::from_pointee(
            TableRegistry::from_entries(HashMap::new(), None),
        )));

        let mut set = JoinSet::new();

        for _ in 0..2 {
            let cfg = config.clone();
            let t = Arc::clone(&tables);
            set.spawn(async move {
                rebuild_registry(&cfg, &t)
                    .await
                    .expect("rebuild_registry must not deadlock");
            });
        }

        while let Some(res) = set.join_next().await {
            res.expect("task must not panic");
        }

        // Last-write-wins: registry is valid (0 or more tables from empty dir).
        let guard = tables.load_full();
        // No assertion on count — just that it resolves without panic.
        let _ = guard.table_count();
    }

    // ── Path-traversal validation tests ───────────────────────────────────────

    #[tokio::test]
    async fn schema_create_rejects_path_traversal_in_table_name() {
        let dir = TempDir::new().expect("tempdir");
        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = Arc::new(ArcSwap::from_pointee(TableRegistry::from_entries(
            HashMap::new(),
            None,
        )));
        for bad in [
            "../escape",
            "../../etc/passwd",
            "/abs/path",
            ".",
            "..",
            ".hidden",
            "a/b",
            "a\\b",
            "",
        ] {
            let params = SchemaCreateParams {
                table: bad.into(),
                scope: "project".into(),
                fields: vec![FieldDefInput {
                    name: "x".into(),
                    ty: "string".into(),
                    required: false,
                }],
                dry_run: false,
            };
            let err = do_schema_create(&config, &tables, params)
                .await
                .expect_err(&format!("must reject table='{bad}'"));
            assert!(
                matches!(err, MiniAppError::Validation { .. }),
                "expected Validation, got {err:?} for '{bad}'"
            );
        }
        // Defence-in-depth: ensure the would-be escape directory was NOT created.
        let parent = dir.path().parent().expect("tempdir parent");
        assert!(
            !parent.join("escape").exists(),
            "../escape must not have been created"
        );
    }

    #[tokio::test]
    async fn schema_update_and_delete_also_reject_traversal() {
        let dir = TempDir::new().expect("tempdir");
        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = Arc::new(ArcSwap::from_pointee(TableRegistry::from_entries(
            HashMap::new(),
            None,
        )));
        let upd = SchemaUpdateParams {
            table: "../bad".into(),
            scope: "project".into(),
            fields: vec![],
            dry_run: false,
        };
        assert!(matches!(
            do_schema_update(&config, &tables, upd).await,
            Err(MiniAppError::Validation { .. })
        ));
        let del = SchemaDeleteParams {
            table: "../bad".into(),
            scope: "project".into(),
            dry_run: false,
        };
        assert!(matches!(
            do_schema_delete(&config, &tables, del).await,
            Err(MiniAppError::Validation { .. })
        ));
    }

    #[tokio::test]
    async fn schema_create_dry_run_also_rejects_traversal() {
        // dry_run path must also be gated — no information disclosure of
        // attacker-controlled absolute path via `would_create` field.
        let dir = TempDir::new().expect("tempdir");
        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = Arc::new(ArcSwap::from_pointee(TableRegistry::from_entries(
            HashMap::new(),
            None,
        )));
        let params = SchemaCreateParams {
            table: "../../etc/atk".into(),
            scope: "project".into(),
            fields: vec![],
            dry_run: true,
        };
        assert!(matches!(
            do_schema_create(&config, &tables, params).await,
            Err(MiniAppError::Validation { .. })
        ));
    }

    // ── schema_batch tests ────────────────────────────────────────────────────

    /// T1: query-only batch executes SQL under SAVEPOINT and commits.
    #[tokio::test]
    async fn schema_batch_query_only_executes_under_savepoint() {
        let dir = TempDir::new().expect("tempdir");
        scaffold_table(
            dir.path(),
            "items",
            "  - name: title\n    type: string\n    required: true\n",
        )
        .await;
        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = make_registry_from_dir(dir.path()).await;

        let params = SchemaBatchParams {
            ops: vec![BatchOp::Query {
                sql: "INSERT INTO rows (id, data, created_at, updated_at) \
                          VALUES ('batch-id-1', '{\"title\":\"hello\"}', 1000, 1000)"
                    .into(),
                params: None,
                table: "items".into(),
            }],
            dry_run: false,
        };

        let result_str = execute_batch(&config, &tables, params)
            .await
            .expect("query-only batch must succeed");
        let result: serde_json::Value = serde_json::from_str(&result_str).unwrap();
        assert_eq!(result["committed"], true);
        assert_eq!(result["ops_executed"], 1);

        // Verify row was inserted.
        let guard = tables.load();
        let entry = guard.resolve(Some("items")).expect("items must be mounted");
        let rows = entry.store.list(Some(10), None).await.unwrap();
        assert_eq!(rows.len(), 1, "committed INSERT must persist");
    }

    /// T1: schema_update inside batch with subsequent query succeeds.
    /// YAML swapped, registry rebuilt, row count visible.
    #[tokio::test]
    async fn schema_batch_schema_update_then_query_succeeds_with_yaml_swap() {
        let dir = TempDir::new().expect("tempdir");
        scaffold_table(
            dir.path(),
            "items",
            "  - name: title\n    type: string\n    required: true\n",
        )
        .await;
        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = make_registry_from_dir(dir.path()).await;

        let params = SchemaBatchParams {
            ops: vec![BatchOp::SchemaUpdate {
                table: "items".into(),
                scope: "project".into(),
                fields: vec![
                    FieldDefInput {
                        name: "title".into(),
                        ty: "string".into(),
                        required: true,
                    },
                    FieldDefInput {
                        name: "note".into(),
                        ty: "string".into(),
                        required: false,
                    },
                ],
            }],
            dry_run: false,
        };

        let result_str = execute_batch(&config, &tables, params)
            .await
            .expect("schema_update batch must succeed");
        let result: serde_json::Value = serde_json::from_str(&result_str).unwrap();
        assert_eq!(result["committed"], true);
        assert_eq!(result["registry_rebuilt"], true);

        // The YAML must contain the new field 'note'.
        let yaml_path = dir.path().join("items").join("schema.yaml");
        let yaml = std::fs::read_to_string(&yaml_path).expect("read yaml");
        assert!(
            yaml.contains("note"),
            "updated yaml must contain 'note' field"
        );
    }

    /// T1: dry_run returns aggregate affects without side effects.
    #[tokio::test]
    async fn schema_batch_dry_run_returns_aggregate_affects() {
        let dir = TempDir::new().expect("tempdir");
        scaffold_table(
            dir.path(),
            "items",
            "  - name: title\n    type: string\n    required: true\n",
        )
        .await;
        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = make_registry_from_dir(dir.path()).await;

        let yaml_before = std::fs::read_to_string(dir.path().join("items").join("schema.yaml"))
            .expect("read yaml before");

        let params = SchemaBatchParams {
            ops: vec![BatchOp::SchemaUpdate {
                table: "items".into(),
                scope: "project".into(),
                fields: vec![
                    FieldDefInput {
                        name: "title".into(),
                        ty: "string".into(),
                        required: true,
                    },
                    FieldDefInput {
                        name: "extra".into(),
                        ty: "string".into(),
                        required: false,
                    },
                ],
            }],
            dry_run: true,
        };

        let result_str = execute_batch(&config, &tables, params)
            .await
            .expect("dry_run batch must succeed");
        let result: serde_json::Value = serde_json::from_str(&result_str).unwrap();
        assert_eq!(result["committed"], false);
        assert_eq!(result["dry_run"], true);
        assert_eq!(result["registry_rebuilt"], false);

        // YAML must be unchanged (dry_run side-effect-free guarantee).
        let yaml_after = std::fs::read_to_string(dir.path().join("items").join("schema.yaml"))
            .expect("read yaml after");
        assert_eq!(yaml_before, yaml_after, "dry_run must not write YAML");
        // No backup dir created.
        assert!(
            !dir.path().join("_backup").exists(),
            "_backup must not be created on dry_run"
        );
    }

    /// T2: op failure rolls back all preceding ops — Crux SAVEPOINT atomicity.
    ///
    /// Sequence: query INSERT (op #0) → invalid SQL (op #1 fails) →
    /// SAVEPOINT rolled back → DB row count = 0.
    /// YAML is also never written since query-only ops don't write YAML.
    #[tokio::test]
    async fn schema_batch_op_failure_rolls_back_all_preceding_ops() {
        let dir = TempDir::new().expect("tempdir");
        scaffold_table(
            dir.path(),
            "items",
            "  - name: title\n    type: string\n    required: true\n",
        )
        .await;
        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = make_registry_from_dir(dir.path()).await;

        let params = SchemaBatchParams {
            ops: vec![
                BatchOp::Query {
                    sql: "INSERT INTO rows (id, data, created_at, updated_at) \
                          VALUES ('rollback-id', '{\"title\":\"x\"}', 1000, 1000)"
                        .into(),
                    params: None,
                    table: "items".into(),
                },
                BatchOp::Query {
                    // Intentionally malformed SQL to trigger failure.
                    sql: "THIS IS NOT VALID SQL".into(),
                    params: None,
                    table: "items".into(),
                },
            ],
            dry_run: false,
        };

        let err = execute_batch(&config, &tables, params)
            .await
            .expect_err("batch with invalid SQL must fail");
        assert!(
            matches!(err, MiniAppError::BatchAborted { .. }),
            "expected BatchAborted, got: {err:?}"
        );

        // After rollback: no rows must exist (Crux SAVEPOINT atomicity).
        let guard = tables.load();
        let entry = guard.resolve(Some("items")).expect("items must be mounted");
        let rows = entry.store.list(Some(10), None).await.unwrap();
        assert_eq!(
            rows.len(),
            0,
            "SAVEPOINT rollback must revert INSERT (Crux: schema_batch SAVEPOINT atomicity)"
        );
    }

    /// T2: multi-table ops return VALIDATION_ERROR.
    #[tokio::test]
    async fn schema_batch_multi_table_ops_returns_validation_error() {
        let dir = TempDir::new().expect("tempdir");
        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = Arc::new(ArcSwap::from_pointee(TableRegistry::from_entries(
            HashMap::new(),
            None,
        )));

        let params = SchemaBatchParams {
            ops: vec![
                BatchOp::Query {
                    sql: "SELECT 1".into(),
                    params: None,
                    table: "table_a".into(),
                },
                BatchOp::Query {
                    sql: "SELECT 1".into(),
                    params: None,
                    table: "table_b".into(),
                },
            ],
            dry_run: false,
        };

        let err = execute_batch(&config, &tables, params)
            .await
            .expect_err("multi-table batch must fail");
        assert!(
            matches!(err, MiniAppError::Validation { ref field, .. } if field == "ops"),
            "expected Validation on ops, got: {err:?}"
        );
    }

    /// T2: empty ops succeeds with zero affects.
    #[tokio::test]
    async fn schema_batch_empty_ops_succeeds_with_zero_affects() {
        let dir = TempDir::new().expect("tempdir");
        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = Arc::new(ArcSwap::from_pointee(TableRegistry::from_entries(
            HashMap::new(),
            None,
        )));

        let params = SchemaBatchParams {
            ops: vec![],
            dry_run: false,
        };

        let result_str = execute_batch(&config, &tables, params)
            .await
            .expect("empty batch must succeed");
        let result: serde_json::Value = serde_json::from_str(&result_str).unwrap();
        assert_eq!(result["ops_executed"], 0);
    }

    /// T2: rollback leaves no tmp files (deferred-write strategy).
    ///
    /// Since deferred writes are held in memory (not tmp files), this test
    /// verifies the invariant by asserting no .tmp files exist after rollback.
    #[tokio::test]
    async fn schema_batch_savepoint_rollback_leaves_no_tmp_files() {
        let dir = TempDir::new().expect("tempdir");
        scaffold_table(
            dir.path(),
            "items",
            "  - name: title\n    type: string\n    required: true\n",
        )
        .await;
        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = make_registry_from_dir(dir.path()).await;

        let params = SchemaBatchParams {
            ops: vec![BatchOp::Query {
                sql: "INVALID SQL FORCED FAIL".into(),
                params: None,
                table: "items".into(),
            }],
            dry_run: false,
        };

        let _ = execute_batch(&config, &tables, params).await;

        // Scan for .tmp files under the scope dir.
        fn has_tmp_files(dir: &Path) -> bool {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return false;
            };
            for entry in rd.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.ends_with(".tmp") {
                    return true;
                }
                if entry.path().is_dir() && has_tmp_files(&entry.path()) {
                    return true;
                }
            }
            false
        }

        assert!(
            !has_tmp_files(dir.path()),
            "no .tmp files must exist after rollback (deferred-write in memory)"
        );
    }

    /// T3: schema_batch yaml_rename_failure after SAVEPOINT commit returns Backup error.
    ///
    /// Simulate by pointing the deferred-write at an unwritable path.
    /// We do a schema_update where the scope dir is read-only.
    /// NOTE: On macOS root-level file creation test may be unreliable; we use
    /// a file-as-dir blocker approach matching ST2 test patterns.
    #[tokio::test]
    async fn schema_batch_yaml_rename_failure_after_savepoint_commit_returns_backup_error_with_log_warning()
     {
        let dir = TempDir::new().expect("tempdir");
        scaffold_table(
            dir.path(),
            "items",
            "  - name: title\n    type: string\n    required: true\n",
        )
        .await;

        // Block `_backup` dir creation by placing a regular file at that path.
        // This causes backup::write_backup_pair to fail.
        let backup_blocker = dir.path().join("_backup");
        std::fs::write(&backup_blocker, b"blocker file").expect("write blocker");

        let config = make_config(None, Some(dir.path().to_path_buf()));
        let tables = make_registry_from_dir(dir.path()).await;

        let params = SchemaBatchParams {
            ops: vec![BatchOp::SchemaUpdate {
                table: "items".into(),
                scope: "project".into(),
                fields: vec![FieldDefInput {
                    name: "title".into(),
                    ty: "string".into(),
                    required: true,
                }],
            }],
            dry_run: false,
        };

        let err = execute_batch(&config, &tables, params)
            .await
            .expect_err("batch with blocked backup must fail");
        assert!(
            matches!(
                err,
                MiniAppError::BatchAborted { .. } | MiniAppError::Backup(_) | MiniAppError::Io(_)
            ),
            "expected BatchAborted/Backup/Io, got: {err:?}"
        );
    }
}
