/// MCP server implementation for mini-app-mcp.
///
/// Exposes 18 tools (`info`, `create`, `get`, `list`, `update`, `delete`,
/// `reload`, `schema_create`, `schema_update`, `schema_delete`, `schema_batch`,
/// `data_snapshot`, `row_materialize`, `alias_create`, `alias_list`, `alias_run`,
/// `alias_delete`, `query_aggregate`) and resources
/// (`schema://yaml`, `schema://json`, `schema://json-schema`, `docs://quickstart`,
/// `docs://tools`, `docs://errors`, `docs://filters`) as MCP capabilities over stdio transport.
/// No HTTP / REST / CLI-CRUD entry points are provided (Crux "MCP-only entry
/// point" constraint).
///
/// # Multi-table mode
///
/// When `MINI_APP_USER_DIR` and/or `MINI_APP_PROJECT_DIR` resolve to
/// directories containing `<table>/schema.yaml` subdirectories, all discovered
/// tables are mounted in a [`TableRegistry`].  Every tool accepts an optional
/// `table` argument to select the target table.
///
/// # Legacy single-table mode
///
/// When only `MINI_APP_SCHEMA` and `MINI_APP_DB` are set, the server operates
/// in single-table mode: one table is mounted and `table` may be omitted from
/// all tool calls.
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        AnnotateAble, ListResourcesResult, PaginatedRequestParams, ProtocolVersion, RawResource,
        ReadResourceRequestParams, ReadResourceResult, ResourceContents, ServerCapabilities,
        ServerInfo,
    },
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::aggregator::{AliasAggregator, SourceSpec};
use crate::alias_storage::{AliasRecord, AliasScope, GlobalAliasStorage};
use crate::config::Config;
use crate::error::MiniAppError;
use crate::filter::ListFilter;
use crate::materialize::{self, FieldSelector, MaterializeParams};
use crate::mcp::registry::TableRegistry;
use crate::mcp::resources as res;
use crate::mcp::schema_tools::{
    self, SchemaBatchParams, SchemaCreateParams, SchemaDeleteParams, SchemaUpdateParams,
};
use crate::schema::SchemaConfig;
use crate::snapshot::{self, DataSnapshotParams};
use crate::store::{Store, UpdateMode};

// =============================================================================
// Public entry point
// =============================================================================

/// Load config, mount tables into a [`TableRegistry`], and serve over stdio.
///
/// # Table mount order (crux #1: User→Project chain)
///
/// 1. Scan `user_dir` (`MINI_APP_USER_DIR` or `~/.mini-app/`) — base layer.
/// 2. Scan `project_dir` (`MINI_APP_PROJECT_DIR` or `./.mini-app/`) — override
///    layer; same-named tables replace User-scope entries (file-level swap).
/// 3. If `MINI_APP_SCHEMA` + `MINI_APP_DB` are both present, add that single
///    legacy table to the registry as well (`default_table` is set so that tool
///    calls with `table` omitted continue to work — crux #2 compatibility).
///    If both steps 1-2 and step 3 yield the same table name, legacy takes
///    precedence and a warning is logged.
///
/// # Errors
///
/// Returns an error if:
/// - The transport setup fails.
///
/// Zero-table start is **not** an error: the server logs a warning and starts
/// in empty mode, returning `TABLE_REQUIRED` on tool calls.
pub async fn run() -> anyhow::Result<()> {
    let config = Config::load()?;

    // Ensure the User-scope dir physically exists so first-time deployments
    // (user-global MCP registry without any pre-seeded tables) don't fail
    // before `mount_from_dirs` even runs. Project-scope dir is left alone
    // because it lives under the caller's CWD and creating it implicitly
    // would pollute arbitrary working directories.
    if let Some(dir) = config.user_dir.as_deref() {
        if let Err(e) = tokio::fs::create_dir_all(dir).await {
            tracing::warn!(dir = %dir.display(), error = %e, "failed to ensure MINI_APP_USER_DIR exists");
        }
    }

    // Phase 1 + 2: User → Project dir scan (crux #1: both paths are always
    // passed, even when one is None — the registry treats None as "0 tables
    // from that scope", not as "skip").
    let mut registry =
        TableRegistry::mount_from_dirs(config.user_dir.as_deref(), config.project_dir.as_deref())
            .await?;

    // Phase 3: legacy single-table env — add to the registry if both are set.
    if config.has_legacy_env() {
        let schema_path = config.schema_path.as_ref().ok_or_else(|| {
            MiniAppError::Config("MINI_APP_SCHEMA required when has_legacy_env is true".into())
        })?;
        let db_path = config.db_path.as_ref().ok_or_else(|| {
            MiniAppError::Config("MINI_APP_DB required when has_legacy_env is true".into())
        })?;
        registry = TableRegistry::mount_legacy_into(registry, schema_path, db_path).await?;
    }

    // 0 tables is no longer fatal: the server still serves `info` / resources
    // and surfaces TableRequired on tool calls. This lets users deploy
    // mini-app-mcp into a user-global MCP registry once and add table dirs
    // later without restarting the registry.
    if registry.table_count() == 0 {
        tracing::warn!(
            "no tables mounted yet — server will start empty. Add \
             <table>/schema.yaml under MINI_APP_USER_DIR ({:?}) or \
             MINI_APP_PROJECT_DIR ({:?}), or set MINI_APP_SCHEMA+MINI_APP_DB.",
            config.user_dir,
            config.project_dir,
        );
    }

    let arc_config = Arc::new(config);
    let server = MiniAppMcpServer::new_multi(registry, Arc::clone(&arc_config));
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

// =============================================================================
// MCP Server
// =============================================================================

/// The MCP server for mini-app-mcp.
///
/// Holds an `Arc<ArcSwap<TableRegistry>>` which allows atomic hot-reload of
/// the registry via the `reload` tool while in-flight requests continue against
/// their captured snapshot.  The server is `Clone` because `rmcp` clones it
/// per connection.
///
/// Use [`MiniAppMcpServer::new_multi`] for multi-table mode and
/// [`MiniAppMcpServer::new_single`] for the legacy single-table adapter (also
/// used in tests).
#[derive(Clone)]
pub struct MiniAppMcpServer {
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
    /// Registry of all mounted tables — atomically swappable via `reload`.
    tables: Arc<ArcSwap<TableRegistry>>,
    /// Mount configuration retained for `reload` re-scans.
    mount_config: Arc<Config>,
}

impl MiniAppMcpServer {
    /// Create a server backed by a pre-built [`TableRegistry`].
    ///
    /// This is the primary constructor for multi-table mode.  The registry
    /// must be built (and validated for ≥1 table) by the caller before
    /// calling this method.
    pub fn new_multi(registry: TableRegistry, config: Arc<Config>) -> Self {
        Self {
            tool_router: Self::tool_router(),
            tables: Arc::new(ArcSwap::from_pointee(registry)),
            mount_config: config,
        }
    }

    /// Legacy single-table adapter — wraps a `Store` + `SchemaConfig` +
    /// `schema_path` in a one-entry [`TableRegistry`] with a `default_table`
    /// set so `table` arguments may be omitted (crux #2 backward compat).
    ///
    /// This is the constructor used by tests and the original `run()` path.
    pub fn new_single(store: Store, schema: SchemaConfig, schema_path: PathBuf) -> Self {
        let table_name = schema.table.clone();
        let registry = TableRegistry::from_single(store, schema, schema_path, table_name);
        // Construct a minimal Config for legacy/test mode (no dir scanning).
        let config = Arc::new(Config {
            schema_path: None,
            db_path: None,
            user_dir: None,
            project_dir: None,
            backup_retention: None,
            snapshot_retention: None,
        });
        Self {
            tool_router: Self::tool_router(),
            tables: Arc::new(ArcSwap::from_pointee(registry)),
            mount_config: config,
        }
    }

    /// Resolve a table, falling back to `default_table` when `table` is `None`.
    ///
    /// Returns a pair `(Arc<Store>, Arc<SchemaConfig>)` for the resolved table.
    /// The `ArcSwap` Guard is dropped immediately after the clone, ensuring
    /// it is not held across any `.await` point.
    ///
    /// # Errors
    ///
    /// - [`MiniAppError::TableRequired`] — `table` is `None` in multi-table mode.
    /// - [`MiniAppError::TableNotFound`] — `table` names a table that is not
    ///   mounted in the registry.
    fn resolve_table(
        &self,
        table: Option<&str>,
    ) -> Result<(Arc<Store>, Arc<SchemaConfig>), MiniAppError> {
        let entry_arc = {
            let g = self.tables.load();
            let entry = g.resolve(table)?;
            (Arc::clone(&entry.store), Arc::clone(&entry.schema))
        };
        Ok(entry_arc)
    }

    /// Returns an owned clone of the current registry's global alias
    /// storage handle when available (multi-table mode). `None` in
    /// legacy single-table mode so the alias_* tools can fall back to
    /// per-table `_aliases`.
    fn global_aliases_handle(&self) -> Option<Arc<GlobalAliasStorage>> {
        self.tables.load().global_aliases().map(Arc::clone)
    }
}

/// Serialise a Phase 2 [`AliasRecord`] (alias_storage variant) into the
/// JSON shape returned by `alias_list`. Manual mapping because
/// `AliasRecord` does not derive `Serialize` (it owns the un-serialised
/// `SourceSpec` / `AliasAggregator` enums + a `scope: Option<AliasScope>`
/// field).
fn alias_record_to_json(r: &AliasRecord) -> serde_json::Value {
    serde_json::json!({
        "name": r.name,
        "sources": r.sources,
        "aggregator": r.aggregator,
        "filter": r.filter,
        "default_limit": r.default_limit,
        "description": r.description,
        "params_schema": r.params_schema,
        "scope": r.scope.map(|s| match s {
            AliasScope::Project => "project",
            AliasScope::User => "user",
        }),
    })
}

// =============================================================================
// alias_run ACL adapter helpers — private
// =============================================================================

/// Resolve an [`AliasRecord`] (alias_storage variant) for `alias_run`.
///
/// - **Phase 2 / multi-table mode**: reads from global alias storage; the
///   record already carries `sources` and `aggregator`.
/// - **Legacy / single-table mode**: reads from the per-table `_aliases`
///   store and constructs an `alias_storage::AliasRecord` with
///   `sources = Single("")` (empty sentinel) so the `execute_alias_run` core
///   function uses `table_fallback` for the actual store lookup.
///
/// Returns `(record, table_fallback)` where `table_fallback` is `Some` only
/// in legacy mode.
async fn alias_run_resolve_record(
    server: &MiniAppMcpServer,
    params: &AliasRunParams,
) -> Result<(AliasRecord, Option<String>), String> {
    if let Some(g) = server.global_aliases_handle() {
        let rec = g.alias_get(&params.name).await.map_err(|e| e.to_string())?;
        Ok((rec, None))
    } else {
        // Legacy single-table mode: per-table _aliases (5-field schema,
        // no sources / aggregator). Resolve the store first so TABLE_REQUIRED
        // / TABLE_NOT_FOUND surfaces here rather than inside the core fn.
        let (store, _) = server
            .resolve_table(params.table.as_deref())
            .map_err(|e| e.to_string())?;
        let leg = store
            .alias_get(&params.name)
            .await
            .map_err(|e| e.to_string())?;
        // Synthesise an alias_storage::AliasRecord from the legacy 5-field row.
        // sources = Single("") is an empty sentinel; execute_alias_run uses
        // table_fallback for the actual lookup when Single is empty.
        let rec = AliasRecord::new(
            leg.name,
            crate::aggregator::SourceSpec::Single(String::new()),
            None, // no aggregator in legacy mode
            leg.filter,
            leg.default_limit,
            leg.description,
            leg.params_schema,
            None, // legacy mode has no stored field projection
        );
        Ok((rec, params.table.clone()))
    }
}

/// Serialise an [`mini_app_core::alias_run::AliasRunValue`] to a JSON string
/// with the same backward-compatible shape that the pre-refactor
/// `tool_alias_run` produced.
///
/// - `Rows(records)` → JSON array of `RowRecord` objects (same as before).
/// - `Aggregate(result)` → `AliasRunResult` externally-tagged JSON (same as before).
fn alias_run_value_to_json(
    value: mini_app_core::alias_run::AliasRunValue,
) -> Result<String, String> {
    match value {
        mini_app_core::alias_run::AliasRunValue::Rows(records) => {
            serde_json::to_string(&records).map_err(|e| e.to_string())
        }
        mini_app_core::alias_run::AliasRunValue::Aggregate(result) => {
            serde_json::to_string(&result).map_err(|e| e.to_string())
        }
    }
}

// =============================================================================
// Reload helpers — private
// =============================================================================

/// Compute the diff between an old and new [`TableRegistry`].
///
/// Returns `(added, removed)` where `added` is the set of table names present
/// in `new` but not in `old`, and `removed` is the set present in `old` but
/// not in `new`.
fn registry_diff(old: &TableRegistry, new: &TableRegistry) -> (Vec<String>, Vec<String>) {
    let old_names: HashSet<&str> = old.table_names().collect();
    let new_names: HashSet<&str> = new.table_names().collect();

    let added: Vec<String> = new_names
        .difference(&old_names)
        .map(|s| s.to_string())
        .collect();
    let removed: Vec<String> = old_names
        .difference(&new_names)
        .map(|s| s.to_string())
        .collect();
    (added, removed)
}

// =============================================================================
// Resource support — private helpers
// =============================================================================

/// Base URIs for schema resources (without query string).
const URI_SCHEMA_YAML: &str = "schema://yaml";
const URI_SCHEMA_JSON: &str = "schema://json";
const URI_SCHEMA_JSON_SCHEMA: &str = "schema://json-schema";
/// Full URIs for documentation resources (no query params).
const URI_DOCS_QUICKSTART: &str = "docs://quickstart";
const URI_DOCS_TOOLS: &str = "docs://tools";
const URI_DOCS_ERRORS: &str = "docs://errors";
const URI_DOCS_FILTERS: &str = "docs://filters";

/// Parse the `table=<name>` query parameter from a URI of the form
/// `<base>[?table=<name>[&...]]`.
///
/// Returns `(base_uri, Option<table_name>)`.  Does not allocate if no `?` is
/// present.
fn parse_table_query(uri: &str) -> (&str, Option<&str>) {
    match uri.split_once('?') {
        Some((base, query)) => {
            let table = query.split('&').find_map(|kv| {
                kv.split_once('=')
                    .filter(|(k, _)| *k == "table")
                    .map(|(_, v)| v)
            });
            (base, table)
        }
        None => (uri, None),
    }
}

impl MiniAppMcpServer {
    /// Build the list of advertised resources.
    ///
    /// In multi-table mode (no default_table) the schema URIs are listed once
    /// each with a description explaining the `?table=<name>` query parameter.
    /// The `docs://` resources are table-independent and always listed.
    fn resource_list(&self) -> Vec<rmcp::model::Resource> {
        let mut resources = Vec::new();

        // Take a snapshot of the registry for this call. The Guard is dropped
        // before any allocations that could be considered an .await boundary
        // (this is a sync method, so no await occurs anyway).
        let registry = self.tables.load_full();

        // Schema resources — emitted once per mounted table when a default
        // table is set (legacy mode), otherwise emitted once with a
        // query-param description.
        let default_table = registry.default_table();
        if let Some(default) = default_table {
            // Legacy / single-table mode: emit concrete URIs for the default table.
            let yaml_uri = format!("{URI_SCHEMA_YAML}?table={default}");
            let json_uri = format!("{URI_SCHEMA_JSON}?table={default}");
            let js_uri = format!("{URI_SCHEMA_JSON_SCHEMA}?table={default}");
            resources.push(
                RawResource::new(yaml_uri, "Schema YAML")
                    .with_description("Raw schema.yaml file content (read at request time).")
                    .with_mime_type("application/yaml")
                    .no_annotation(),
            );
            resources.push(
                RawResource::new(json_uri, "Schema JSON")
                    .with_description(
                        "SchemaConfig serialised as JSON — same content as the `info` tool.",
                    )
                    .with_mime_type("application/json")
                    .no_annotation(),
            );
            resources.push(
                RawResource::new(js_uri, "JSON Schema")
                    .with_description(
                        "JSON Schema (draft-07) derived from SchemaConfig.fields — \
                         use this to validate `data` arguments before calling `create`/`update`.",
                    )
                    .with_mime_type("application/schema+json")
                    .no_annotation(),
            );
        } else {
            // Multi-table mode: emit one URI per table per schema resource type.
            let mut table_names: Vec<&str> = registry.table_names().collect();
            table_names.sort(); // deterministic ordering for tests
            for table in &table_names {
                let yaml_uri = format!("{URI_SCHEMA_YAML}?table={table}");
                let json_uri = format!("{URI_SCHEMA_JSON}?table={table}");
                let js_uri = format!("{URI_SCHEMA_JSON_SCHEMA}?table={table}");
                resources.push(
                    RawResource::new(yaml_uri, format!("Schema YAML ({table})"))
                        .with_description(format!(
                            "Raw schema.yaml for table '{table}' (read at request time)."
                        ))
                        .with_mime_type("application/yaml")
                        .no_annotation(),
                );
                resources.push(
                    RawResource::new(json_uri, format!("Schema JSON ({table})"))
                        .with_description(format!(
                            "SchemaConfig for table '{table}' serialised as JSON.",
                        ))
                        .with_mime_type("application/json")
                        .no_annotation(),
                );
                resources.push(
                    RawResource::new(js_uri, format!("JSON Schema ({table})"))
                        .with_description(format!(
                            "JSON Schema (draft-07) for table '{table}' — \
                             use this to validate `data` arguments.",
                        ))
                        .with_mime_type("application/schema+json")
                        .no_annotation(),
                );
            }
        }

        // Documentation resources — always present, table-independent.
        resources.push(
            RawResource::new(URI_DOCS_QUICKSTART, "Quickstart")
                .with_description(
                    "Agent quickstart — server identity, multi-table vs legacy \
                     mode detection, first-call recipe, and pointers to the other \
                     `docs://` resources. Distinct from the human-facing project \
                     README on GitHub (which is not served as an MCP resource).",
                )
                .with_mime_type("text/markdown")
                .no_annotation(),
        );
        resources.push(
            RawResource::new(URI_DOCS_TOOLS, "Tools Reference")
                .with_description(
                    "Cheat sheet listing all 18 tools with descriptions and input shapes.",
                )
                .with_mime_type("text/markdown")
                .no_annotation(),
        );
        resources.push(
            RawResource::new(URI_DOCS_ERRORS, "Error Code Reference")
                .with_description("Reference table of all error codes returned by this server.")
                .with_mime_type("text/markdown")
                .no_annotation(),
        );
        resources.push(
            RawResource::new(URI_DOCS_FILTERS, "Filter Construction Guide")
                .with_description(
                    "Guide for constructing filter objects (Eq/In/Like/Or/And) used by \
                     the `list` tool, `alias_create`, and `row_materialize`.",
                )
                .with_mime_type("text/markdown")
                .no_annotation(),
        );

        resources
    }

    /// Inner implementation of `read_resource` — tested directly to avoid
    /// `RequestContext` construction issues in tests (rmcp 1.5 makes
    /// `RequestContext` `#[non_exhaustive]` so it cannot be built in external
    /// crates).
    async fn read_resource_impl(&self, uri: &str) -> Result<ReadResourceResult, McpError> {
        let (base_uri, table_query) = parse_table_query(uri);

        // Take a full Arc snapshot of the registry so we don't hold a Guard
        // across any .await points (await-holding-lock prevention).
        let registry = self.tables.load_full();

        let contents = match base_uri {
            URI_SCHEMA_YAML => {
                let entry = registry
                    .resolve(table_query)
                    .map_err(crate::miniapp_error_to_mcp_error)?;
                let text = std::fs::read_to_string(entry.schema_path.as_ref()).map_err(|e| {
                    McpError::internal_error(format!("failed to read schema.yaml: {e}"), None)
                })?;
                ResourceContents::text(text, uri).with_mime_type("application/yaml")
            }
            URI_SCHEMA_JSON => {
                let entry = registry
                    .resolve(table_query)
                    .map_err(crate::miniapp_error_to_mcp_error)?;
                let text = serde_json::to_string_pretty(entry.schema.as_ref())
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                ResourceContents::text(text, uri).with_mime_type("application/json")
            }
            URI_SCHEMA_JSON_SCHEMA => {
                let entry = registry
                    .resolve(table_query)
                    .map_err(crate::miniapp_error_to_mcp_error)?;
                let js = res::derive_json_schema(entry.schema.as_ref());
                let text = serde_json::to_string_pretty(&js)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                ResourceContents::text(text, uri).with_mime_type("application/schema+json")
            }
            URI_DOCS_QUICKSTART => {
                ResourceContents::text(res::QUICKSTART, uri).with_mime_type("text/markdown")
            }
            URI_DOCS_TOOLS => {
                ResourceContents::text(res::TOOLS_DOC, uri).with_mime_type("text/markdown")
            }
            URI_DOCS_ERRORS => {
                ResourceContents::text(res::ERRORS_DOC, uri).with_mime_type("text/markdown")
            }
            URI_DOCS_FILTERS => {
                ResourceContents::text(res::FILTERS_DOC, uri).with_mime_type("text/markdown")
            }
            _ => {
                return Err(McpError::resource_not_found(
                    format!("unknown resource URI: {uri}"),
                    None,
                ));
            }
        };
        Ok(ReadResourceResult::new(vec![contents]))
    }
}

// =============================================================================
// ServerHandler — get_info + resource dispatch
// =============================================================================

#[tool_handler]
impl ServerHandler for MiniAppMcpServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::V_2025_03_26;
        info.capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .build();
        info.server_info.name = "mini-app-mcp".to_string();
        info.server_info.title = Some("Mini App MCP — Agent-First CRUD Store".to_string());
        info.server_info.description = Some(
            "Agent-First CRUD store backed by SQLite. \
             Supports multiple tables via User→Project schema chain. \
             18 tools: info, create, get, list, update, delete, reload, \
             schema_create, schema_update, schema_delete, schema_batch, data_snapshot, \
             row_materialize, alias_create, alias_list, alias_run, alias_delete."
                .to_string(),
        );
        info.server_info.version = env!("CARGO_PKG_VERSION").to_string();
        info.instructions = Some(
            "Agent-First CRUD store backed by SQLite.\n\
             \n\
             Table shape is defined entirely in schema.yaml; no field names are \
             hard-coded in the server.\n\
             \n\
             ## Multi-table mode\n\
             \n\
             When `MINI_APP_USER_DIR` and/or `MINI_APP_PROJECT_DIR` are set (or \
             default to `~/.mini-app/` and `./.mini-app/`), the server mounts \
             all tables discovered there. In this mode the `table` argument is \
             **required** for every tool call. Omitting `table` returns a \
             TABLE_REQUIRED error (data.code = \"TABLE_REQUIRED\").\n\
             \n\
             ## Legacy single-table mode\n\
             \n\
             When `MINI_APP_SCHEMA` and `MINI_APP_DB` are set (legacy env vars), \
             the server mounts that single table and the `table` argument may be \
             **omitted** — the default table is used automatically.\n\
             \n\
             ## Tool reference\n\
             \n\
             - `info`: Return the parsed schema (table name + field definitions).\n\
             - `create`: Insert a new row. The `data` argument must be a JSON \
             object whose fields conform to schema.yaml.\n\
             - `get`: Fetch a single row by id.\n\
             - `list`: List rows with optional limit/offset pagination.\n\
             - `update`: Replace the data of an existing row by id.\n\
             - `delete`: Delete a row by id.\n\
             - `reload`: Reload all schemas from configured directories.\n\
             - `schema_create`: Create a new table schema and register it.\n\
             - `schema_update`: Update an existing table schema.\n\
             - `schema_delete`: Delete a table schema (DB file is preserved).\n\
             - `schema_batch`: Batch schema operations with SAVEPOINT atomicity.\n\
             - `data_snapshot`: Create per-table SQLite snapshot dumps in \
             `_snapshots/`. Supports `table`, `scope`, and `dry_run` parameters.\n\
             \n\
             All schema tools accept an optional `table` argument. Specify the \
             table name when running in multi-table mode."
                .to_string(),
        );
        info
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult::with_all_items(self.resource_list()))
    }

    async fn read_resource(
        &self,
        req: ReadResourceRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        self.read_resource_impl(&req.uri).await
    }
}

// =============================================================================
// Parameters types
// =============================================================================

/// Parameters for the `info` tool.
#[derive(Deserialize, JsonSchema)]
pub struct InfoParams {
    /// Name of the table to return schema for.
    ///
    /// In multi-table mode this argument is required; omitting it returns a
    /// TABLE_REQUIRED error. In legacy single-table mode (`MINI_APP_SCHEMA` +
    /// `MINI_APP_DB`) this may be omitted and the single configured table is
    /// used automatically.
    pub table: Option<String>,
}

/// JSON Schema for a `data` field that must arrive as a JSON object.
///
/// schemars 1.x emits a permissive schema (no `type` field) for
/// `serde_json::Value`. Anthropic's tool-use serializer treats untyped
/// params as opaque and stringifies them, so the server then sees a
/// `Value::String` and rejects with `value must be a JSON object`.
/// Forcing `"type": "object"` on the public schema keeps the param shape
/// honest for the client. See diagnosis 2026-05-07.
fn data_object_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "object",
        "description": "JSON object whose fields conform to schema.yaml.",
        "additionalProperties": true
    })
}

/// Parameters for the `create` tool.
#[derive(Deserialize, JsonSchema)]
struct CreateParams {
    /// JSON object whose fields conform to schema.yaml.
    #[schemars(schema_with = "data_object_schema")]
    data: serde_json::Value,
    /// Name of the table to insert into.
    ///
    /// In multi-table mode this argument is required; omitting it returns a
    /// TABLE_REQUIRED error. In legacy single-table mode (`MINI_APP_SCHEMA` +
    /// `MINI_APP_DB`) this may be omitted and the single configured table is
    /// used automatically.
    table: Option<String>,
}

/// Parameters for the `get` tool.
#[derive(Deserialize, JsonSchema)]
struct GetParams {
    /// Row id (UUID string).
    id: String,
    /// Name of the table to fetch from.
    ///
    /// In multi-table mode this argument is required; omitting it returns a
    /// TABLE_REQUIRED error. In legacy single-table mode (`MINI_APP_SCHEMA` +
    /// `MINI_APP_DB`) this may be omitted and the single configured table is
    /// used automatically.
    table: Option<String>,
    /// Optional field projection applied to the `data` object of the returned row.
    ///
    /// Use `{"mode":"list","fields":["field1","field2"]}` to select a named
    /// subset of fields, or `{"mode":"all"}` to request all fields (same as
    /// omitting this argument).  When omitted the full `data` object is returned
    /// (backward-compatible).  Unknown field names return
    /// `VALIDATION_ERROR` (data.code="VALIDATION_ERROR").
    #[serde(default)]
    fields: Option<FieldSelector>,
}

/// Parameters for the `list` tool.
#[derive(Deserialize, JsonSchema)]
struct ListParams {
    /// Maximum rows to return (default 100, max 1000).
    limit: Option<u32>,
    /// Number of rows to skip from the start.
    offset: Option<u32>,
    /// Name of the table to list from.
    ///
    /// In multi-table mode this argument is required; omitting it returns a
    /// TABLE_REQUIRED error. In legacy single-table mode (`MINI_APP_SCHEMA` +
    /// `MINI_APP_DB`) this may be omitted and the single configured table is
    /// used automatically.
    table: Option<String>,
    /// Optional server-side filter.  Supports `Eq`, `In`, `Like`,
    /// `ArrayContains`, `ArrayNotContains`, `Or`, and `And` variants.
    /// All field names must be present in the table's `schema.yaml` and
    /// values must match the schema-declared type.  Omitting this argument
    /// (or passing `null`) returns all rows unfiltered (backward-compatible).
    #[serde(default)]
    filter: Option<ListFilter>,
    /// Optional field projection applied to the `data` object of each returned row.
    ///
    /// Use `{"mode":"list","fields":["field1","field2"]}` to select a named
    /// subset of fields, or `{"mode":"all"}` to request all fields (same as
    /// omitting this argument).  When omitted the full `data` object is returned
    /// (backward-compatible).  Unknown field names return
    /// `VALIDATION_ERROR` (data.code="VALIDATION_ERROR").
    #[serde(default)]
    fields: Option<FieldSelector>,
}

/// Parameters for the `update` tool.
#[derive(Deserialize, JsonSchema)]
struct UpdateParams {
    /// Row id (UUID string).
    id: String,
    /// JSON object whose fields conform to schema.yaml.
    #[schemars(schema_with = "data_object_schema")]
    data: serde_json::Value,
    /// Name of the table to update in.
    ///
    /// In multi-table mode this argument is required; omitting it returns a
    /// TABLE_REQUIRED error. In legacy single-table mode (`MINI_APP_SCHEMA` +
    /// `MINI_APP_DB`) this may be omitted and the single configured table is
    /// used automatically.
    table: Option<String>,
    /// Optional update mode.
    ///
    /// - `"merge"` (default): RFC 7396 shallow merge. Fields absent from `data`
    ///   are preserved. A `null` value deletes an optional field; a `null` on a
    ///   required field returns a Validation error.
    /// - `"replace"`: replace the entire `data` object with the supplied value
    ///   (pre-breaking-change behavior).
    #[serde(default)]
    mode: Option<UpdateMode>,
}

/// Parameters for the `delete` tool.
#[derive(Deserialize, JsonSchema)]
struct DeleteParams {
    /// Row id (UUID string).
    id: String,
    /// Name of the table to delete from.
    ///
    /// In multi-table mode this argument is required; omitting it returns a
    /// TABLE_REQUIRED error. In legacy single-table mode (`MINI_APP_SCHEMA` +
    /// `MINI_APP_DB`) this may be omitted and the single configured table is
    /// used automatically.
    table: Option<String>,
}

/// Parameters for the `alias_create` tool.
#[derive(Deserialize, JsonSchema)]
struct AliasCreateParams {
    /// Legacy single-table source. Backward-compat alias for
    /// `sources = { "kind": "single", "value": <table> }`. When `sources`
    /// is `None` and `table` is `Some`, the alias is silently normalised
    /// to `SourceSpec::Single(<table>)` before being stored. Mutually
    /// exclusive with `sources` (supplying both is an error).
    table: Option<String>,
    /// Phase 2 source-table specifier. `Single(<table>)` / `Multi([..])` /
    /// `Pattern(<glob>)`. Takes precedence over `table` when supplied.
    /// Required when `aggregator` is used with multi-table sources.
    sources: Option<SourceSpec>,
    /// Optional aggregator primitive (Phase 2). `Count` / `Sum` / `Avg` /
    /// `Min` / `Max` / `GroupBy`. When supplied, `alias_run` dispatches
    /// to `execute_aggregate` instead of the plain `Store::list` path.
    aggregator: Option<AliasAggregator>,
    /// Unique name for this alias (global namespace within the chosen
    /// scope).
    name: String,
    /// The filter to store for this alias.  Mutually exclusive with
    /// `filter_template`; exactly one of the two must be supplied.
    filter: Option<ListFilter>,
    /// A MiniJinja template string that renders to a valid filter JSON object.
    /// Use `{{ param_name }}` placeholders for values injected at run time via
    /// `alias_run`'s `params` argument.  Mutually exclusive with `filter`;
    /// exactly one of the two must be supplied.
    filter_template: Option<String>,
    /// Ordered list of parameter names that `alias_run` must supply in its
    /// `params` object when executing this alias.  Only meaningful when
    /// `filter_template` is set; ignored for plain `filter` aliases.
    params_schema: Option<Vec<String>>,
    /// Default limit applied when `alias_run` is called without a runtime
    /// `limit` override.  If omitted, `Store::list` applies its own default
    /// (100 rows).
    limit: Option<u32>,
    /// Optional human-readable description for this alias.
    description: Option<String>,
    /// Target alias storage scope. `Some(Project)` writes to the
    /// project-local `_global.db`; `Some(User)` writes to the user-wide
    /// `_global.db`. When omitted (default), the server selects
    /// `Project` if that scope is mounted (= legacy backward-compatible
    /// behaviour) and falls back to `User` otherwise. This lets callers
    /// opt into User scope explicitly without setting
    /// `MINI_APP_PROJECT_DIR`, and lets the server still write
    /// successfully when only one scope is mounted (the common Claude
    /// Code default env, where Project scope unmounts when the CWD has
    /// no `.mini-app/` directory).
    scope: Option<AliasScope>,
    /// Optional default field-projection to store with this alias.
    /// When set, `alias_run` will use this projection if no run-time
    /// `fields` argument is supplied.  `null` / omitted means no stored
    /// default — all fields are returned (Crux #3: never coerced to an
    /// empty list by the storage or run layer).
    fields: Option<FieldSelector>,
}

/// Parameters for the `alias_list` tool.
#[derive(Deserialize, JsonSchema)]
struct AliasListParams {
    /// Name of the table whose alias namespace to list.
    ///
    /// In multi-table mode this argument is required; omitting it returns a
    /// TABLE_REQUIRED error. In legacy single-table mode (`MINI_APP_SCHEMA` +
    /// `MINI_APP_DB`) this may be omitted and the single configured table is
    /// used automatically.
    table: Option<String>,
}

/// Parameters for the `alias_run` tool.
#[derive(Deserialize, JsonSchema)]
struct AliasRunParams {
    /// Name of the table whose alias namespace to query.
    ///
    /// In multi-table mode this argument is required; omitting it returns a
    /// TABLE_REQUIRED error. In legacy single-table mode (`MINI_APP_SCHEMA` +
    /// `MINI_APP_DB`) this may be omitted and the single configured table is
    /// used automatically.
    table: Option<String>,
    /// Name of the alias to run.
    name: String,
    /// Runtime limit override.  When supplied, takes precedence over the
    /// alias's stored `default_limit`.  When omitted, the stored
    /// `default_limit` is used; if that is also absent, `Store::list`
    /// applies its own default (100 rows).
    limit: Option<u32>,
    /// Runtime offset (number of rows to skip).  Not stored in the alias;
    /// must be supplied at execution time when pagination is needed.
    offset: Option<u32>,
    /// Parameter values to inject into a parameterized alias's
    /// `filter_template`.  Must be a JSON object whose keys match the names
    /// declared in the alias's `params_schema`.  Required when the alias was
    /// created with `filter_template`; ignored for plain `filter` aliases.
    params: Option<serde_json::Value>,
    /// Optional field projection applied to the `data` object of each returned row.
    ///
    /// Use `{"mode":"list","fields":["field1","field2"]}` to select a named
    /// subset of fields, or `{"mode":"all"}` to request all fields (same as
    /// omitting this argument).  When omitted the full `data` object is returned
    /// (backward-compatible).  Unknown field names return
    /// `VALIDATION_ERROR` (data.code="VALIDATION_ERROR").
    #[serde(default)]
    fields: Option<FieldSelector>,
}

/// Parameters for the `query_aggregate` tool.
#[derive(Deserialize, JsonSchema)]
struct QueryAggregateParams {
    /// Source-table specifier — `{ "kind": "single", "value": "..." }` or
    /// `{ "kind": "multi", "value": ["...", "..."] }`. `Multi` UNION ALLs
    /// the listed tables before aggregation via SQLite ATTACH DATABASE.
    sources: crate::aggregator::SourceSpec,
    /// Optional pre-aggregation filter applied via WHERE within every
    /// source sub-query (re-uses the same `ListFilter` shape that the
    /// `list` tool accepts).
    #[serde(default)]
    filter: Option<ListFilter>,
    /// Aggregator primitive — `Count` / `Sum` / `Avg` / `Min` / `Max` /
    /// `GroupBy { by_field, having?, inner? }`.
    aggregator: crate::aggregator::AliasAggregator,
}

/// Parameters for the `alias_delete` tool.
#[derive(Deserialize, JsonSchema)]
struct AliasDeleteParams {
    /// Name of the table whose alias namespace to delete from.
    ///
    /// In multi-table mode this argument is required; omitting it returns a
    /// TABLE_REQUIRED error. In legacy single-table mode (`MINI_APP_SCHEMA` +
    /// `MINI_APP_DB`) this may be omitted and the single configured table is
    /// used automatically.
    table: Option<String>,
    /// Name of the alias to delete.
    name: String,
}

/// Result returned by the `reload` tool.
///
/// Reports the outcome of the registry reload: how many tables are now mounted,
/// which table names were newly added, and which were removed.
#[derive(Serialize, JsonSchema)]
pub struct ReloadResult {
    /// Total number of tables mounted after the reload.
    pub mounted: usize,
    /// Table names that were added (present in new registry, absent in old).
    pub added: Vec<String>,
    /// Table names that were removed (absent in new registry, present in old).
    pub removed: Vec<String>,
}

// =============================================================================
// Tool implementations
// =============================================================================

#[tool_router]
impl MiniAppMcpServer {
    /// Return the schema configuration (table name and field definitions).
    ///
    /// Crux constraint: field definitions come exclusively from the parsed
    /// schema.yaml; no field is hard-coded in this method.
    #[tool(
        name = "info",
        description = "Return the parsed schema for the given `table` (table name + field definitions). \
                       In multi-table mode, `table` is required; omitting it returns a \
                       TABLE_REQUIRED error (data.code=\"TABLE_REQUIRED\"). \
                       In legacy single-table mode (`MINI_APP_SCHEMA`+`MINI_APP_DB`), `table` may be omitted. \
                       If an unknown table name is specified, returns TABLE_NOT_FOUND (data.code=\"TABLE_NOT_FOUND\").",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn tool_info(
        &self,
        Parameters(params): Parameters<InfoParams>,
    ) -> Result<String, String> {
        let (_store, schema) = self
            .resolve_table(params.table.as_deref())
            .map_err(|e| e.to_string())?;
        serde_json::to_string_pretty(schema.as_ref()).map_err(|e| e.to_string())
    }

    /// Create a new row.
    ///
    /// Crux constraint: the `data` argument is a generic JSON object passed
    /// directly to `Store::create` — no field-specific access is performed here.
    #[tool(
        name = "create",
        description = "Create a new row. The `data` argument must be a JSON object matching schema.yaml. \
                       In multi-table mode, `table` is required; omitting it returns a \
                       TABLE_REQUIRED error (data.code=\"TABLE_REQUIRED\"). \
                       In legacy single-table mode (`MINI_APP_SCHEMA`+`MINI_APP_DB`), `table` may be omitted. \
                       If an unknown table name is specified, returns TABLE_NOT_FOUND (data.code=\"TABLE_NOT_FOUND\").",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn tool_create(
        &self,
        Parameters(params): Parameters<CreateParams>,
    ) -> Result<String, String> {
        let (store, _schema) = self
            .resolve_table(params.table.as_deref())
            .map_err(|e| e.to_string())?;
        let record = store.create(params.data).await.map_err(|e| e.to_string())?;
        serde_json::to_string(&record).map_err(|e| e.to_string())
    }

    /// Get a single row by id.
    #[tool(
        name = "get",
        description = "Fetch a single row by its UUID id. \
                       In multi-table mode, `table` is required; omitting it returns a \
                       TABLE_REQUIRED error (data.code=\"TABLE_REQUIRED\"). \
                       In legacy single-table mode (`MINI_APP_SCHEMA`+`MINI_APP_DB`), `table` may be omitted. \
                       If an unknown table name is specified, returns TABLE_NOT_FOUND (data.code=\"TABLE_NOT_FOUND\"). \
                       Optional `fields` argument supports field projection on the `data` object. \
                       Use {\"mode\":\"list\",\"fields\":[\"field1\",\"field2\"]} to select specific fields, \
                       or {\"mode\":\"all\"} (same as omitting). Unknown field names return \
                       VALIDATION_ERROR (data.code=\"VALIDATION_ERROR\"). \
                       UUID prefix match: if `id` is shorter than 36 characters, a prefix match is attempted \
                       (SELECT id WHERE id LIKE '<prefix>%'). 0 matches returns NOT_FOUND error \
                       (data.code=\"NOT_FOUND\"). 2 or more matches returns AMBIGUOUS_ID error \
                       (data.code=\"AMBIGUOUS_ID\", data.candidates=[candidate id list]). \
                       A full 36-character UUID bypasses prefix match and uses exact lookup.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tool_get(&self, Parameters(params): Parameters<GetParams>) -> Result<String, String> {
        let (store, schema) = self
            .resolve_table(params.table.as_deref())
            .map_err(|e| e.to_string())?;
        let record = store.get(&params.id).await.map_err(|e| e.to_string())?;
        let records = materialize::apply_projection(vec![record], &params.fields, &schema)
            .map_err(|e| e.to_string())?;
        let record = records.into_iter().next().ok_or_else(|| {
            "apply_projection returned empty vec (invariant violation)".to_string()
        })?;
        serde_json::to_string(&record).map_err(|e| e.to_string())
    }

    /// List rows with optional pagination and server-side filtering.
    #[tool(
        name = "list",
        description = "List rows ordered by created_at descending. Supports limit (default 100, max 1000) and offset. \
                       Optional `filter` argument supports Eq/In/Like/ArrayContains/ArrayNotContains/Or/And composition over schema-validated fields. \
                       In multi-table mode, `table` is required; omitting it returns a \
                       TABLE_REQUIRED error (data.code=\"TABLE_REQUIRED\"). \
                       In legacy single-table mode (`MINI_APP_SCHEMA`+`MINI_APP_DB`), `table` may be omitted. \
                       If an unknown table name is specified, returns TABLE_NOT_FOUND (data.code=\"TABLE_NOT_FOUND\"). \
                       Optional `fields` argument supports field projection on the `data` object of each row. \
                       Use {\"mode\":\"list\",\"fields\":[\"field1\",\"field2\"]} to select specific fields, \
                       or {\"mode\":\"all\"} (same as omitting). Unknown field names return \
                       VALIDATION_ERROR (data.code=\"VALIDATION_ERROR\").",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tool_list(
        &self,
        Parameters(params): Parameters<ListParams>,
    ) -> Result<String, String> {
        let (store, schema) = self
            .resolve_table(params.table.as_deref())
            .map_err(|e| e.to_string())?;
        if let Some(ref f) = params.filter {
            f.validate(&schema).map_err(|e| e.to_string())?;
        }
        let records = store
            .list(params.limit, params.offset, params.filter)
            .await
            .map_err(|e| e.to_string())?;
        let records = materialize::apply_projection(records, &params.fields, &schema)
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&records).map_err(|e| e.to_string())
    }

    /// Update an existing row by id.
    ///
    /// Crux constraint: the `data` argument is passed generically to
    /// `Store::update` — no field-specific access is performed here.
    #[tool(
        name = "update",
        description = "Update an existing row by id. Optional `mode` argument: \"merge\" (default, RFC 7396 shallow merge) \
                       or \"replace\" (replace entire data with the `data` argument). \
                       Merge mode: fields absent from `data` are preserved, a null value deletes an optional field, \
                       and a null value on a required field returns a Validation error. \
                       The `data` argument must be a JSON object matching schema.yaml. \
                       In multi-table mode, `table` is required; omitting it returns a \
                       TABLE_REQUIRED error (data.code=\"TABLE_REQUIRED\"). \
                       In legacy single-table mode (`MINI_APP_SCHEMA`+`MINI_APP_DB`), `table` may be omitted. \
                       If an unknown table name is specified, returns TABLE_NOT_FOUND (data.code=\"TABLE_NOT_FOUND\"). \
                       UUID prefix match: if `id` is shorter than 36 characters, a prefix match is attempted \
                       (SELECT id WHERE id LIKE '<prefix>%'). 0 matches returns NOT_FOUND error \
                       (data.code=\"NOT_FOUND\"). 2 or more matches returns AMBIGUOUS_ID error \
                       (data.code=\"AMBIGUOUS_ID\", data.candidates=[candidate id list]). \
                       A full 36-character UUID bypasses prefix match and uses exact lookup.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tool_update(
        &self,
        Parameters(params): Parameters<UpdateParams>,
    ) -> Result<String, String> {
        let (store, _schema) = self
            .resolve_table(params.table.as_deref())
            .map_err(|e| e.to_string())?;
        let mode = params.mode.unwrap_or(UpdateMode::Merge);
        let record = store
            .update(&params.id, params.data, mode)
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&record).map_err(|e| e.to_string())
    }

    /// Delete a row by id.
    #[tool(
        name = "delete",
        description = "Delete the row with the given id. Returns an error if the row does not exist. \
                       In multi-table mode, `table` is required; omitting it returns a \
                       TABLE_REQUIRED error (data.code=\"TABLE_REQUIRED\"). \
                       In legacy single-table mode (`MINI_APP_SCHEMA`+`MINI_APP_DB`), `table` may be omitted. \
                       If an unknown table name is specified, returns TABLE_NOT_FOUND (data.code=\"TABLE_NOT_FOUND\"). \
                       UUID prefix match: if `id` is shorter than 36 characters, a prefix match is attempted \
                       (SELECT id WHERE id LIKE '<prefix>%'). 0 matches returns NOT_FOUND error \
                       (data.code=\"NOT_FOUND\"). 2 or more matches returns AMBIGUOUS_ID error \
                       (data.code=\"AMBIGUOUS_ID\", data.candidates=[candidate id list]). \
                       A full 36-character UUID bypasses prefix match and uses exact lookup.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tool_delete(
        &self,
        Parameters(params): Parameters<DeleteParams>,
    ) -> Result<String, String> {
        let (store, _schema) = self
            .resolve_table(params.table.as_deref())
            .map_err(|e| e.to_string())?;
        store.delete(&params.id).await.map_err(|e| e.to_string())?;
        serde_json::to_string(&serde_json::json!({ "deleted": params.id }))
            .map_err(|e| e.to_string())
    }

    /// Reload all schemas from the configured directories.
    ///
    /// Re-scans `MINI_APP_USER_DIR` / `MINI_APP_PROJECT_DIR` and, if the legacy
    /// `MINI_APP_SCHEMA` + `MINI_APP_DB` pair is set, re-mounts that single-table
    /// entry as well.  The active registry is replaced atomically via `ArcSwap`;
    /// in-flight requests continue against the old snapshot and complete
    /// normally. No file watcher is used; this tool must be invoked explicitly.
    ///
    /// Concurrent `reload` calls use last-write-wins semantics (the last
    /// `store()` call wins).
    #[tool(
        name = "reload",
        description = "Reload all schemas from the configured directories. \
                       Re-scans MINI_APP_USER_DIR / MINI_APP_PROJECT_DIR and re-mounts the \
                       legacy MINI_APP_SCHEMA + MINI_APP_DB single-table entry if those env \
                       vars are set. The active registry is replaced atomically; in-flight \
                       requests continue against the old snapshot and complete normally. \
                       No file watcher is used — explicit invocation only.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub async fn tool_reload(&self) -> Result<String, McpError> {
        // Early-reject: servers constructed via `new_single` (or any path that
        // leaves all four mount-config fields as None) have no directory to
        // re-scan.  Proceeding would call `mount_from_dirs(None, None)`, produce
        // an empty registry, and atomically overwrite the original table — data
        // loss without error.  Reject loudly instead.
        let config = Arc::clone(&self.mount_config);
        if config.user_dir.is_none()
            && config.project_dir.is_none()
            && config.schema_path.is_none()
            && config.db_path.is_none()
        {
            return Err(crate::miniapp_error_to_mcp_error(MiniAppError::Config(
                "reload not configured: server was constructed via new_single without a mount \
                 config"
                    .into(),
            )));
        }

        // Capture old registry for diff computation before the swap.
        let old_registry = self.tables.load_full();

        // Rebuild the registry via the shared helper.  This handles the
        // spawn_blocking + block_on + ArcSwap.store() sequence (design Y1).
        schema_tools::rebuild_registry(&config, &self.tables)
            .await
            .map_err(|e| {
                let msg = format!("reload: {e}");
                tracing::error!(%msg);
                crate::miniapp_error_to_mcp_error(e)
            })?;

        // Compute diff against the new registry snapshot.
        let new_registry = self.tables.load_full();
        let (mut added, mut removed) = registry_diff(&old_registry, &new_registry);
        let mounted = new_registry.table_count();

        // Sort for deterministic output.
        added.sort();
        removed.sort();

        tracing::info!(
            mounted,
            added = ?added,
            removed = ?removed,
            "registry reloaded"
        );

        let result = ReloadResult {
            mounted,
            added,
            removed,
        };
        serde_json::to_string(&result)
            .map_err(|e| crate::miniapp_error_to_mcp_error(MiniAppError::Schema(e.to_string())))
    }

    /// Create a new schema (schema.yaml + DB directory) in the given scope.
    ///
    /// Writes `{scope_root}/{table}/schema.yaml` via atomic tmp+rename and
    /// opens (creates) the backing SQLite database.  The registry is rebuilt
    /// atomically via ArcSwap after a successful write.
    ///
    /// # scope
    /// `"project"` → `MINI_APP_PROJECT_DIR` (default `./.mini-app/`).
    /// `"user"` → `MINI_APP_USER_DIR` (default `~/.mini-app/`).
    ///
    /// # dry_run
    /// When `dry_run=true`, checks whether the schema path is absent and returns
    /// an `affects` object without writing any file or modifying the registry.
    ///
    /// # Backup
    /// No backup is written for `schema_create` (no prior YAML to back up).
    ///
    /// Returns `SCHEMA_EXISTS` (`data.code`) if the schema already exists.
    #[tool(
        name = "schema_create",
        description = "Create a new table schema (schema.yaml + DB) in the given scope. \
                       scope: 'project' (MINI_APP_PROJECT_DIR) or 'user' (MINI_APP_USER_DIR). \
                       fields: list of {name, type, required} field definitions. \
                       dry_run=true: verify path is absent and return affects without writing. \
                       Returns SCHEMA_EXISTS (data.code) when schema already exists. \
                       No automatic DDL migrations are applied — the DB is created empty. \
                       Triggers an atomic registry rebuild after successful write. \
                       Optional title and description fields can be supplied to attach human-readable docs to the table.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub async fn tool_schema_create(
        &self,
        Parameters(params): Parameters<SchemaCreateParams>,
    ) -> Result<String, String> {
        schema_tools::do_schema_create(&self.mount_config, &self.tables, params)
            .await
            .map_err(|e| e.to_string())
    }

    /// Overwrite an existing schema.yaml with new field definitions.
    ///
    /// Backs up the current YAML and DB to `{scope_root}/_backup/` before
    /// writing.  Rebuilds the registry atomically.  **No DDL is applied** to
    /// the existing SQLite table (Crux: no automatic DDL migration).
    ///
    /// # dry_run
    /// When `dry_run=true`, computes the field diff (added / removed /
    /// type-changed) and reports `rows_unchanged` without touching any file.
    ///
    /// # Backup retention
    /// After a successful write, backup pairs beyond the retention limit
    /// (`MINI_APP_BACKUP_RETENTION`, default 10) are purged.
    ///
    /// Returns `TABLE_NOT_FOUND` (`data.code`) when `table` is not mounted.
    #[tool(
        name = "schema_update",
        description = "Overwrite an existing table schema with new field definitions. \
                       scope: 'project' or 'user'. fields: full replacement field list. \
                       dry_run=true: return field diff (fields_added/removed/type_changed) \
                       without writing. \
                       NO DDL is applied to the existing SQLite table — column structure \
                       change is the operator's responsibility. \
                       Backs up {table}.{ts}.yaml + {table}.{ts}.db to {scope_root}/_backup/ \
                       before writing. Retention default: 10 pairs (MINI_APP_BACKUP_RETENTION). \
                       Returns TABLE_NOT_FOUND (data.code) when table is not mounted. \
                       Optional title and description fields can be supplied to attach human-readable docs to the table.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn tool_schema_update(
        &self,
        Parameters(params): Parameters<SchemaUpdateParams>,
    ) -> Result<String, String> {
        schema_tools::do_schema_update(&self.mount_config, &self.tables, params)
            .await
            .map_err(|e| e.to_string())
    }

    /// Remove a schema.yaml and unmount the table from the registry.
    ///
    /// Backs up the YAML and DB pair to `{scope_root}/_backup/` before
    /// removal.  **The DB file is NOT deleted** — the operator must remove
    /// `{scope_root}/{table}/{table}.db` explicitly (Crux: no automatic DDL
    /// migration).  Rebuilds the registry atomically after removal.
    ///
    /// # dry_run
    /// When `dry_run=true`, counts orphaned rows and reports `would_remove_yaml`
    /// without removing any file.
    ///
    /// Use `dry_run=true` first to preview the number of rows that will become
    /// inaccessible after schema removal.
    ///
    /// Returns `TABLE_NOT_FOUND` (`data.code`) when `table` is not mounted.
    #[tool(
        name = "schema_delete",
        description = "Remove a table schema (schema.yaml) and unmount it from the registry. \
                       scope: 'project' or 'user'. \
                       dry_run=true: return rows_orphaned + would_remove_yaml without deleting. \
                       Use dry_run=true first — rows become inaccessible after deletion. \
                       THE DB FILE IS NOT DELETED — remove {scope_root}/{table}/{table}.db \
                       manually if needed (Crux: no automatic DDL migration). \
                       Backs up {table}.{ts}.yaml + {table}.{ts}.db to {scope_root}/_backup/ \
                       before removing. Retention default: 10 pairs (MINI_APP_BACKUP_RETENTION). \
                       Returns TABLE_NOT_FOUND (data.code) when table is not mounted.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub async fn tool_schema_delete(
        &self,
        Parameters(params): Parameters<SchemaDeleteParams>,
    ) -> Result<String, String> {
        schema_tools::do_schema_delete(&self.mount_config, &self.tables, params)
            .await
            .map_err(|e| e.to_string())
    }

    /// Execute a list of ops atomically under a single SQLite SAVEPOINT.
    ///
    /// All ops must target the **same table** (architecture constraint: SQLite
    /// SAVEPOINT is per-connection and each table has its own connection).
    /// A multi-table batch returns `VALIDATION_ERROR` without entering a SAVEPOINT.
    ///
    /// # Op types
    /// - `query`: raw SQL executed inside SAVEPOINT (schema validation bypassed).
    /// - `schema_create / schema_update / schema_delete`: YAML writes deferred
    ///   until SAVEPOINT commit; no YAML change occurs on rollback.
    ///
    /// # Atomicity (Crux: schema_batch SAVEPOINT atomicity)
    /// Any op failure rolls back all preceding ops, including schema mutations.
    /// YAML is only written after SAVEPOINT commit succeeds.
    ///
    /// # dry_run (Crux: dry_run side-effect-free guarantee)
    /// When `dry_run=true`, per-op affects are computed without any FS or DB writes.
    ///
    /// # No DDL migration (Crux: no automatic DDL migration)
    /// `schema_update` / `schema_delete` inside a batch only rewrite the YAML and
    /// rebuild the registry — no `ALTER TABLE` or `DROP TABLE` is ever issued.
    #[tool(
        name = "schema_batch",
        description = "Execute ops[] atomically under a single SQLite SAVEPOINT. \
                       All ops must target the same table (single-table constraint). \
                       Op types: query (raw SQL inside SAVEPOINT — schema validation bypassed), \
                       schema_create, schema_update, schema_delete (YAML writes deferred to commit). \
                       On any op failure: SAVEPOINT rolled back, all preceding ops reverted, \
                       YAML never written (all-or-nothing). \
                       dry_run=true: compute affects per op without any FS or DB write. \
                       No DDL migration: schema_update/delete only rewrites YAML + rebuilds registry. \
                       Registry is rebuilt once at batch end (not per op). \
                       Returns BATCH_ABORTED (data.code) with op_index on failure. \
                       Returns VALIDATION_ERROR when ops[] target multiple tables.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub async fn tool_schema_batch(
        &self,
        Parameters(params): Parameters<SchemaBatchParams>,
    ) -> Result<String, String> {
        schema_tools::execute_batch(&self.mount_config, &self.tables, params)
            .await
            .map_err(|e| e.to_string())
    }

    /// Create per-table SQLite snapshot dumps in `{scope_root}/_snapshots/`.
    ///
    /// Uses `rusqlite::Connection::backup` (hot backup API) with a fresh
    /// source connection — the source DB remains open and writable during the
    /// snapshot (Crux: rusqlite hot backup API).
    ///
    /// Snapshot retention is controlled exclusively by `MINI_APP_SNAPSHOT_RETENTION`
    /// (Crux: snapshot retention isolation); the `_backup/` directory and
    /// `MINI_APP_BACKUP_RETENTION` are never touched.
    ///
    /// # dry_run
    /// When `dry_run=true`, returns `affects` metadata (target tables, row
    /// counts, would-purge counts) **without** creating, modifying, or
    /// deleting any file or database state (Crux: dry_run zero-write
    /// guarantee).
    ///
    /// # scope
    /// `"project"` → tables mounted from `MINI_APP_PROJECT_DIR`.
    /// `"user"` → tables mounted from `MINI_APP_USER_DIR`.
    /// Omit to snapshot all mounted tables.
    ///
    /// # table
    /// Name of a single table to snapshot.  Omit to snapshot all tables in
    /// the given scope.
    #[tool(
        name = "data_snapshot",
        description = "Create per-table SQLite snapshot dump(s) under {scope_root}/_snapshots/. \
                       Schema is not modified. Snapshots use the rusqlite hot backup API so the \
                       source DB stays open and writable during the operation. \
                       scope: 'project' (MINI_APP_PROJECT_DIR) or 'user' (MINI_APP_USER_DIR); \
                       omit to snapshot all mounted tables. \
                       table: name of a single table to snapshot; omit for all tables in scope. \
                       dry_run=true: return affects (target_tables, row_counts, \
                       would_purge_generations) without any FS or DB write. \
                       Retention controlled by MINI_APP_SNAPSHOT_RETENTION (default 10), \
                       strictly separate from MINI_APP_BACKUP_RETENTION.",
        annotations(
            read_only_hint = false,
            idempotent_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    pub async fn tool_data_snapshot(
        &self,
        Parameters(params): Parameters<DataSnapshotParams>,
    ) -> Result<String, String> {
        snapshot::do_data_snapshot(&self.mount_config, &self.tables, params)
            .await
            .map_err(|e| e.to_string())
    }

    /// Write row data from a table to absolute filesystem path(s).
    ///
    /// Selects rows via `selector` (by UUID or by filter predicate), projects
    /// the fields listed in `fields`, serialises to `format`, and writes the
    /// result to `dest`.
    ///
    /// # selector
    /// `{"type": "by_id", "id": "<uuid>"}` — fetch one row.
    /// `{"type": "by_filter", "filter": {...}, "limit": N, "offset": N}` — fetch
    /// rows matching a [`ListFilter`] predicate.
    ///
    /// # fields
    /// `{"mode": "all"}` — all schema fields in declaration order.
    /// `{"mode": "list", "fields": ["f1", "f2"]}` — named subset in specified order.
    ///
    /// # format
    /// `raw` → `.txt` (field values joined by newlines).
    /// `markdown` → `.md` (each field as a heading + body).
    /// `json` → `.json` (JSON object per row; array when concat=true).
    /// `yaml` → `.yaml` (YAML document per row; document stream when concat=true).
    ///
    /// # dest
    /// **Absolute path required** (Agent-First trust model — relative paths are
    /// rejected immediately with `MATERIALIZE_DEST_RELATIVE`).
    /// When `concat=false` (default): treated as a directory; each row is
    /// written to `{dest}/{row_id}.{ext}`.
    /// When `concat=true`: treated as a file path; all rows are concatenated
    /// into a single file.
    ///
    /// # concat
    /// `false` (default): one file per row, `row_id` set in each result entry.
    /// `true`: all rows concatenated into one file, `row_id` is `null`.
    /// Note: `concat=true` with `selector=by_id` is an error.
    ///
    /// # write_mode
    /// `overwrite` (default): existing files are overwritten.
    /// `error`: returns `MATERIALIZE_DEST_INVALID` if the target file exists.
    ///
    /// # dry_run
    /// `true`: validation, projection, serialisation, and SHA-256 computation
    /// run normally, but **no file is written**.  The returned `files` entries
    /// carry would-be `path`, `bytes`, and `sha256` values.
    ///
    /// # Return
    /// `{ "count": N, "files": [{ "path": "...", "bytes": N, "sha256": "...", "row_id": "..." | null }, ...] }`
    #[tool(
        name = "row_materialize",
        description = "Write row data from a table to absolute filesystem path(s). \
                       selector: {type:by_id,id:...} or {type:by_filter,filter:{...},limit?,offset?}. \
                       fields: {mode:all} or {mode:list,fields:[...]}. \
                       format: raw|markdown|json|yaml (extensions .txt/.md/.json/.yaml). \
                       dest: ABSOLUTE path required — relative paths are rejected (Agent-First trust). \
                       When concat=false (default) dest is a directory and each row becomes {dest}/{id}.{ext} with row_id set. \
                       When concat=true dest is a file path and all rows are merged; row_id is null. \
                       concat=true with selector=by_id is an error. \
                       write_mode: overwrite (default) | error (fail if file exists, even with dry_run). \
                       dry_run=true: compute sha256/bytes but do NOT write any file. \
                       Returns {count:N, files:[{path,bytes,sha256,row_id}]}.",
        annotations(
            read_only_hint = false,
            idempotent_hint = true,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn tool_materialize(
        &self,
        Parameters(params): Parameters<MaterializeParams>,
    ) -> Result<String, String> {
        materialize::do_materialize(&self.mount_config, &self.tables, params)
            .await
            .and_then(|r| {
                serde_json::to_string(&r).map_err(|e| {
                    crate::error::MiniAppError::MaterializeFormatError(format!("json result: {e}"))
                })
            })
            .map_err(|e| e.to_string())
    }

    // -------------------------------------------------------------------------
    // Alias tools
    // -------------------------------------------------------------------------

    /// Create a named query alias for a table.
    ///
    /// Registers a `filter` + optional `default_limit` under `name` in the
    /// table's per-table `_aliases` storage.  The alias can later be executed
    /// with `alias_run`.
    ///
    /// Returns `ALIAS_ALREADY_EXISTS` (data.code) if an alias with the same
    /// name already exists for this table.
    ///
    /// In multi-table mode the `table` argument is required; omitting it
    /// returns a TABLE_REQUIRED error (data.code="TABLE_REQUIRED").
    #[tool(
        name = "alias_create",
        description = "Register a named query alias for a table.  Supply either a \
                       `filter` (the same filter object accepted by the `list` tool) \
                       or a `filter_template` (a MiniJinja template string that renders \
                       to a valid filter JSON object using `{{ param_name }}` placeholders). \
                       Exactly one of `filter` or `filter_template` must be supplied; \
                       providing both or neither is an error.  When using \
                       `filter_template`, supply `params_schema` (an array of parameter \
                       name strings) to declare which parameters `alias_run` must \
                       receive.  An optional `limit` is stored as the default row cap \
                       when the alias is run.  An optional `scope` (\"project\" / \
                       \"user\") selects the storage scope explicitly; when omitted, \
                       the server writes to Project scope if mounted, falling back to \
                       User scope otherwise (= safe default for Claude Code-style \
                       single-scope envs).  The alias is scoped exclusively to the \
                       named `table`.  Returns ALIAS_ALREADY_EXISTS if the name is \
                       already taken.  In multi-table mode `table` is required; \
                       omitting it returns TABLE_REQUIRED (data.code=\"TABLE_REQUIRED\").",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn tool_alias_create(
        &self,
        Parameters(params): Parameters<AliasCreateParams>,
    ) -> Result<String, String> {
        // Resolve `sources` independently of `resolve_table` so callers
        // can register Multi / Pattern aliases without a `table` arg
        // (which would otherwise surface TABLE_REQUIRED in multi-table
        // mode). `sources` + `table` are mutually exclusive — supplying
        // both is an error so the chosen source is unambiguous.
        let sources = match (params.sources.clone(), params.table.clone()) {
            (Some(_), Some(_)) => {
                return Err(
                    "`sources` and `table` are mutually exclusive (provide one only)".to_string(),
                );
            }
            (Some(s), None) => s,
            (None, Some(t)) => SourceSpec::Single(t),
            (None, None) => {
                // Last-resort: legacy default table (single-table mode)
                // or TABLE_REQUIRED (multi-table mode). The legacy path
                // is preserved so existing alias_create calls without
                // any source designator keep working.
                let (_, schema) = self.resolve_table(None).map_err(|e| e.to_string())?;
                SourceSpec::Single(schema.table.clone())
            }
        };

        // Schema for filter validation: pick the first source table's
        // schema (Pattern's empty tables() slice → defer to runtime;
        // Phase 2 limitation, same as execute_aggregate's per-table
        // schema assumption).
        let first_table = sources.tables().first().cloned();
        let validation_schema = match &first_table {
            Some(t) => Some(
                self.resolve_table(Some(t.as_str()))
                    .map_err(|e| e.to_string())?
                    .1,
            ),
            None => None,
        };
        // Legacy single-table mode fallback writes through the per-table
        // store. We only need this handle when the global storage is
        // unavailable and a concrete source table can be resolved
        // (Pattern in legacy mode is rejected at runtime).
        let legacy_store = match &first_table {
            Some(t) => Some(
                self.resolve_table(Some(t.as_str()))
                    .map_err(|e| e.to_string())?
                    .0,
            ),
            None => None,
        };

        let (filter_json, params_schema_json): (String, Option<String>) =
            match (params.filter, params.filter_template) {
                (Some(f), None) => {
                    if let Some(schema) = &validation_schema {
                        f.validate(schema).map_err(|e| e.to_string())?;
                    }
                    let json = serde_json::to_string(&f).map_err(|e| e.to_string())?;
                    (json, None)
                }
                (None, Some(tmpl)) => {
                    let schema_json = params
                        .params_schema
                        .map(|s| serde_json::to_string(&s))
                        .transpose()
                        .map_err(|e: serde_json::Error| e.to_string())?;
                    (tmpl, schema_json)
                }
                (Some(_), Some(_)) => {
                    return Err("filter and filter_template are mutually exclusive".to_string());
                }
                (None, None) => {
                    return Err("either filter or filter_template is required".to_string());
                }
            };

        if let Some(global) = self.global_aliases_handle() {
            // Pick the target scope.  Caller-supplied `scope` wins; when
            // omitted, prefer Project if it is mounted (legacy
            // backward-compatible default) and fall back to User
            // otherwise.  This lets `alias_create` succeed in the common
            // Claude Code env where only the User scope (`~/.mini-app/`)
            // is mounted because the CWD has no `.mini-app/` directory.
            let target_scope = match params.scope {
                Some(s) => s,
                None => {
                    if global.path_for_scope(AliasScope::Project).is_some() {
                        AliasScope::Project
                    } else {
                        AliasScope::User
                    }
                }
            };
            // Defensive: if the caller explicitly asked for a scope that
            // is not mounted, surface a clear error before hitting the
            // storage layer's generic "scope is not mounted" config error.
            if global.path_for_scope(target_scope).is_none() {
                return Err(format!(
                    "alias_create: requested scope {target_scope:?} is not mounted on this \
                     server. Either omit the `scope` argument (the server will auto-select a \
                     mounted scope) or configure the corresponding directory \
                     (MINI_APP_PROJECT_DIR / MINI_APP_USER_DIR).",
                ));
            }
            let fields_json = match params.fields {
                Some(ref f) => Some(
                    serde_json::to_string(f)
                        .map_err(|e| format!("alias_create: serialise fields: {e}"))?,
                ),
                None => None,
            };
            let record = AliasRecord::new(
                &params.name,
                sources,
                params.aggregator,
                filter_json,
                params.limit,
                params.description,
                params_schema_json,
                fields_json,
            );
            global
                .alias_create(target_scope, record)
                .await
                .map_err(|e| e.to_string())?;
        } else {
            // Legacy single-table mode: per-table `_aliases`. Pattern
            // sources are rejected here because no concrete store is
            // available to back them.
            if params.fields.is_some() {
                return Err(
                    "alias_create: 'fields' is not supported in legacy single-table mode \
                     (per-table _aliases). Use the global alias path (multi-table mode)."
                        .to_string(),
                );
            }
            let store = legacy_store.ok_or_else(|| {
                "Pattern sources are not supported in legacy single-table mode".to_string()
            })?;
            store
                .alias_create(
                    &params.name,
                    &filter_json,
                    params.limit,
                    params.description,
                    params_schema_json,
                )
                .await
                .map_err(|e| e.to_string())?;
        }
        serde_json::to_string(&serde_json::json!({ "created": params.name }))
            .map_err(|e| e.to_string())
    }

    /// List all named query aliases for a table.
    ///
    /// Returns an array of alias records (name, filter JSON, default_limit,
    /// description) scoped exclusively to the named table.
    ///
    /// In multi-table mode the `table` argument is required; omitting it
    /// returns a TABLE_REQUIRED error (data.code="TABLE_REQUIRED").
    #[tool(
        name = "alias_list",
        description = "List all named query aliases registered for a table.  \
                       Each record includes `name`, `filter` (the stored filter JSON), \
                       `default_limit`, and `description`.  Aliases are scoped \
                       exclusively to the named `table`. \
                       In multi-table mode `table` is required; omitting it returns \
                       TABLE_REQUIRED (data.code=\"TABLE_REQUIRED\").",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tool_alias_list(
        &self,
        Parameters(params): Parameters<AliasListParams>,
    ) -> Result<String, String> {
        if let Some(global) = self.global_aliases_handle() {
            // Phase 2 path: list across both scopes (Project precedence)
            // and optionally narrow to a single legacy `table` source for
            // backward compatibility.
            let mut aliases = global.alias_list().await.map_err(|e| e.to_string())?;
            if let Some(t) = params.table.as_deref() {
                // Phase 2 fix: include Multi / Pattern aliases that
                // reference `t` as well — the prior Single-only retain
                // silently dropped them.
                aliases.retain(|r| r.sources.includes_table(t));
            }
            let values: Vec<serde_json::Value> = aliases.iter().map(alias_record_to_json).collect();
            return serde_json::to_string(&values).map_err(|e| e.to_string());
        }
        // Legacy single-table mode: per-table `_aliases`.
        let (store, _schema) = self
            .resolve_table(params.table.as_deref())
            .map_err(|e| e.to_string())?;
        let aliases = store.alias_list().await.map_err(|e| e.to_string())?;
        let values: Vec<serde_json::Value> = aliases
            .into_iter()
            .map(|a| {
                serde_json::json!({
                    "name": a.name,
                    "filter": a.filter,
                    "default_limit": a.default_limit,
                    "description": a.description,
                    "params_schema": a.params_schema,
                })
            })
            .collect();
        serde_json::to_string(&values).map_err(|e| e.to_string())
    }

    /// Execute a named query alias and return matching rows.
    ///
    /// Looks up the stored filter for `name`, then calls `Store::list` with
    /// the resolved limit and offset.  Runtime `limit` and `offset` override
    /// the alias's stored `default_limit`; if neither is supplied the stored
    /// default is used (falling back to `Store::list`'s own default of 100).
    ///
    /// Returns `ALIAS_NOT_FOUND` (data.code) if no alias with `name` exists
    /// for this table.
    ///
    /// In multi-table mode the `table` argument is required; omitting it
    /// returns a TABLE_REQUIRED error (data.code="TABLE_REQUIRED").
    #[tool(
        name = "alias_run",
        description = "Execute a named query alias and return matching rows.  \
                       The stored filter (or rendered filter template) is replayed \
                       against `Store::list`.  For parameterized aliases created with \
                       `filter_template`, supply `params` (a JSON object) whose keys \
                       match the alias's `params_schema`; omitting `params` for such \
                       aliases returns ALIAS_PARAMS_REQUIRED.  For plain `filter` \
                       aliases `params` is ignored.  Supply a runtime `limit` to \
                       override the alias's stored default_limit, and/or an `offset` \
                       for pagination (offset is never stored in the alias).  The alias \
                       is scoped exclusively to the named `table`. \
                       Returns ALIAS_NOT_FOUND if the alias does not exist. \
                       In multi-table mode `table` is required; omitting it returns \
                       TABLE_REQUIRED (data.code=\"TABLE_REQUIRED\"). \
                       Optional `fields` argument supports field projection on the `data` object of each row. \
                       Use {\"mode\":\"list\",\"fields\":[\"field1\",\"field2\"]} to select specific fields, \
                       or {\"mode\":\"all\"} (same as omitting). Unknown field names return \
                       VALIDATION_ERROR (data.code=\"VALIDATION_ERROR\").",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tool_alias_run(
        &self,
        Parameters(params): Parameters<AliasRunParams>,
    ) -> Result<String, String> {
        // Phase 2: prefer global storage when available. Falls back to
        // per-table store path for legacy single-table mode.
        let (record, table_fallback) = alias_run_resolve_record(self, &params).await?;

        // Delegate all orchestration to the Core SDK.
        let registry = self.tables.load_full();
        let result = mini_app_core::alias_run::execute_alias_run(
            &registry,
            record,
            params.params,
            table_fallback.as_deref(),
            params.limit,
            params.offset,
            params.fields,
        )
        .await
        .map_err(|e| e.to_string())?;

        // Backward-compat JSON serialisation: each variant maps to the
        // same JSON shape that the previous in-handler implementation
        // produced (records array or AliasRunResult object).
        alias_run_value_to_json(result)
    }

    /// Delete a named query alias from a table.
    ///
    /// Removes the alias with `name` from the table's per-table `_aliases`
    /// storage.  Returns `ALIAS_NOT_FOUND` (data.code) if no alias with that
    /// name exists for this table.
    ///
    /// In multi-table mode the `table` argument is required; omitting it
    /// returns a TABLE_REQUIRED error (data.code="TABLE_REQUIRED").
    #[tool(
        name = "alias_delete",
        description = "Delete a named query alias from a table.  \
                       The alias is scoped exclusively to the named `table`; \
                       aliases belonging to other tables are never affected. \
                       Returns ALIAS_NOT_FOUND if the alias does not exist. \
                       In multi-table mode `table` is required; omitting it returns \
                       TABLE_REQUIRED (data.code=\"TABLE_REQUIRED\").",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tool_alias_delete(
        &self,
        Parameters(params): Parameters<AliasDeleteParams>,
    ) -> Result<String, String> {
        if let Some(global) = self.global_aliases_handle() {
            // Phase 2 path: try the mounted Project scope first, fall
            // back to User on not-found (mirrors `alias_get`
            // precedence). When Project scope is unmounted (= single-
            // scope env, common Claude Code default), go directly to
            // the User scope so delete cannot fail with a generic
            // "scope is not mounted" config error.
            if global.path_for_scope(AliasScope::Project).is_some() {
                match global.alias_delete(AliasScope::Project, &params.name).await {
                    Ok(()) => {}
                    Err(MiniAppError::AliasNotFound { .. }) => {
                        global
                            .alias_delete(AliasScope::User, &params.name)
                            .await
                            .map_err(|e| e.to_string())?;
                    }
                    Err(other) => return Err(other.to_string()),
                }
            } else {
                global
                    .alias_delete(AliasScope::User, &params.name)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            return serde_json::to_string(&serde_json::json!({ "deleted": params.name }))
                .map_err(|e| e.to_string());
        }
        // Legacy single-table mode.
        let (store, _schema) = self
            .resolve_table(params.table.as_deref())
            .map_err(|e| e.to_string())?;
        store
            .alias_delete(&params.name)
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&serde_json::json!({ "deleted": params.name }))
            .map_err(|e| e.to_string())
    }

    /// Run a multi-table aggregation across one or more sources.
    ///
    /// Resolves each source via the registry, mounts each backing `.db`
    /// file via `ATTACH DATABASE`, composes a `UNION ALL` inner sub-query
    /// (or a single `SELECT` for `SourceSpec::Single`), and wraps it in an
    /// outer aggregate (with optional `GROUP BY` + `HAVING`). Read-only —
    /// no DB mutation is performed.
    ///
    /// Returns `AGGREGATOR_ERROR` (`data.code`) for structural
    /// inconsistencies (empty sources, ATTACH-limit exceeded, nested
    /// `GroupBy`, non-UTF-8 db path). Returns `VALIDATION_ERROR` for
    /// field / identifier rejections. Returns `TABLE_NOT_FOUND` when any
    /// source name is not mounted.
    #[tool(
        name = "query_aggregate",
        description = "Aggregate rows from one or more tables using \
                       COUNT / SUM / AVG / MIN / MAX / GROUP BY (with \
                       optional HAVING). Single source uses one table; \
                       Multi source UNION ALLs across tables before \
                       aggregation via SQLite ATTACH DATABASE. Read-only \
                       — does not modify any data.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tool_query_aggregate(
        &self,
        Parameters(params): Parameters<QueryAggregateParams>,
    ) -> Result<String, String> {
        let registry = self.tables.load_full();
        let tables = params.sources.tables();
        if tables.is_empty() {
            return Err(
                MiniAppError::Aggregator("sources must contain at least one table".into())
                    .to_string(),
            );
        }
        // Phase 1: use the FIRST source table's schema as the validation
        // basis (caller is responsible for cross-source schema
        // compatibility; per-table validation is Phase 2 carry).
        let first_schema: Arc<SchemaConfig> = {
            let entry = registry
                .resolve(Some(&tables[0]))
                .map_err(|e| e.to_string())?;
            Arc::clone(&entry.schema)
        };
        let result = crate::aggregator::execute_aggregate(
            registry.as_ref(),
            params.sources,
            params.filter,
            params.aggregator,
            &first_schema,
        )
        .await
        .map_err(|e| e.to_string())?;
        serde_json::to_string(&result).map_err(|e| e.to_string())
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::schema::{FieldDef, FieldType};

    // ---------------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------------

    /// Build a single-table server for tests using the legacy adapter.
    /// The schema has two fields: `title` (required) and `state` (optional).
    async fn make_server() -> (MiniAppMcpServer, tempfile::NamedTempFile) {
        use std::io::Write as _;
        let schema_yaml = b"\
table: test_table\n\
fields:\n\
  - name: title\n\
    type: string\n\
    required: true\n\
  - name: state\n\
    type: string\n\
    required: false\n";

        let mut tmp = tempfile::NamedTempFile::new().expect("temp file");
        tmp.write_all(schema_yaml).expect("write schema yaml");

        let schema = SchemaConfig {
            table: "test_table".to_string(),
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
                    name: "state".to_string(),
                    ty: FieldType::String,
                    required: false,
                    description: None,
                },
            ],
            dump: None,
        };
        let store = Store::open(Path::new(":memory:"), schema.clone())
            .await
            .expect("in-memory store must open");
        let schema_path = tmp.path().to_path_buf();
        (
            MiniAppMcpServer::new_single(store, schema, schema_path),
            tmp,
        )
    }

    /// In rmcp 1.5, `RequestContext` and `CallToolRequestParams` are
    /// `#[non_exhaustive]` and cannot be constructed via struct literals in
    /// external crates.  Tests call tool methods directly instead of going
    /// through the `ServerHandler::call_tool` dispatch path.  This helper
    /// creates a row and returns the parsed JSON record.
    async fn do_create(
        server: &MiniAppMcpServer,
        data: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let json = server
            .tool_create(Parameters(CreateParams { data, table: None }))
            .await?;
        Ok(serde_json::from_str(&json).unwrap())
    }

    async fn do_get(server: &MiniAppMcpServer, id: &str) -> Result<serde_json::Value, String> {
        let json = server
            .tool_get(Parameters(GetParams {
                id: id.to_string(),
                table: None,
                fields: None,
            }))
            .await?;
        Ok(serde_json::from_str(&json).unwrap())
    }

    async fn do_list(
        server: &MiniAppMcpServer,
        limit: Option<u32>,
        offset: Option<u32>,
    ) -> Result<serde_json::Value, String> {
        let json = server
            .tool_list(Parameters(ListParams {
                limit,
                offset,
                table: None,
                filter: None,
                fields: None,
            }))
            .await?;
        Ok(serde_json::from_str(&json).unwrap())
    }

    async fn do_update(
        server: &MiniAppMcpServer,
        id: &str,
        data: serde_json::Value,
        mode: Option<UpdateMode>,
    ) -> Result<serde_json::Value, String> {
        let json = server
            .tool_update(Parameters(UpdateParams {
                id: id.to_string(),
                data,
                table: None,
                mode,
            }))
            .await?;
        Ok(serde_json::from_str(&json).unwrap())
    }

    async fn do_delete(server: &MiniAppMcpServer, id: &str) -> Result<serde_json::Value, String> {
        let json = server
            .tool_delete(Parameters(DeleteParams {
                id: id.to_string(),
                table: None,
            }))
            .await?;
        Ok(serde_json::from_str(&json).unwrap())
    }

    // ---------------------------------------------------------------------------
    // T1: list_tools — all 12 tools present with correct annotations.
    // Access via server.tool_router.list_all() to avoid RequestContext.
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn list_tools_contains_all_seventeen() {
        let (server, _tmp) = make_server().await;
        let tools = server.tool_router.list_all();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        for expected in &[
            "info",
            "create",
            "get",
            "list",
            "update",
            "delete",
            "reload",
            "schema_create",
            "schema_update",
            "schema_delete",
            "schema_batch",
            "data_snapshot",
            "row_materialize",
            "alias_create",
            "alias_list",
            "alias_run",
            "alias_delete",
            "query_aggregate",
        ] {
            assert!(
                names.contains(expected),
                "tool '{expected}' missing from list_tools"
            );
        }
        assert_eq!(tools.len(), 18, "expected exactly 18 tools");
    }

    /// RED: ensure create/update tools advertise `data` as an object in their
    /// JSON Schema. Without `"type": "object"` Anthropic's tool-use serializer
    /// stringifies the param and the server rejects it with
    /// `value must be a JSON object`. See diagnosis 2026-05-07.
    #[tokio::test]
    async fn create_and_update_data_param_is_typed_object() {
        let (server, _tmp) = make_server().await;
        let tools = server.tool_router.list_all();
        for name in &["create", "update"] {
            let tool = tools
                .iter()
                .find(|t| t.name.as_ref() == *name)
                .unwrap_or_else(|| panic!("{name} tool must exist"));
            let schema = serde_json::to_value(&tool.input_schema).unwrap();
            let data_schema = schema
                .pointer("/properties/data")
                .unwrap_or_else(|| panic!("{name}.input_schema.properties.data missing"));
            let ty = data_schema.get("type").unwrap_or_else(|| {
                panic!("{name}.input_schema.properties.data missing 'type'; got {data_schema}")
            });
            assert_eq!(
                ty.as_str(),
                Some("object"),
                "{name}.input_schema.properties.data.type must be 'object'; got {ty}"
            );
        }
    }

    #[tokio::test]
    async fn tool_annotations_delete_is_destructive() {
        let (server, _tmp) = make_server().await;
        let tools = server.tool_router.list_all();
        let delete_tool = tools
            .iter()
            .find(|t| t.name == "delete")
            .expect("delete tool must exist");
        let ann = delete_tool
            .annotations
            .as_ref()
            .expect("delete must have annotations");
        assert_eq!(
            ann.destructive_hint,
            Some(true),
            "delete must be destructive"
        );
        assert_eq!(ann.idempotent_hint, Some(true), "delete must be idempotent");
    }

    #[tokio::test]
    async fn tool_annotations_create_is_not_idempotent() {
        let (server, _tmp) = make_server().await;
        let tools = server.tool_router.list_all();
        let create_tool = tools
            .iter()
            .find(|t| t.name == "create")
            .expect("create tool must exist");
        let ann = create_tool
            .annotations
            .as_ref()
            .expect("create must have annotations");
        assert_eq!(
            ann.idempotent_hint,
            Some(false),
            "create must NOT be idempotent"
        );
        assert_eq!(
            ann.destructive_hint,
            Some(false),
            "create must NOT be destructive"
        );
    }

    #[tokio::test]
    async fn tool_annotations_read_only_tools() {
        let (server, _tmp) = make_server().await;
        let tools = server.tool_router.list_all();
        for name in &["info", "get", "list"] {
            let tool = tools
                .iter()
                .find(|t| t.name == *name)
                .unwrap_or_else(|| panic!("tool '{name}' must exist"));
            let ann = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("tool '{name}' must have annotations"));
            assert_eq!(
                ann.read_only_hint,
                Some(true),
                "tool '{name}' must be read_only"
            );
        }
    }

    #[tokio::test]
    async fn tool_annotations_reload() {
        let (server, _tmp) = make_server().await;
        let tools = server.tool_router.list_all();
        let reload_tool = tools
            .iter()
            .find(|t| t.name == "reload")
            .expect("reload tool must exist");
        let ann = reload_tool
            .annotations
            .as_ref()
            .expect("reload must have annotations");
        assert_eq!(
            ann.read_only_hint,
            Some(false),
            "reload must not be read_only"
        );
        assert_eq!(
            ann.idempotent_hint,
            Some(false),
            "reload must not be idempotent"
        );
        assert_eq!(
            ann.destructive_hint,
            Some(false),
            "reload must not be destructive"
        );
    }

    // ---------------------------------------------------------------------------
    // T1b: tool descriptions contain "table" semantics (§K-49 / §1-8-1)
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn tool_descriptions_mention_table_argument() {
        let (server, _tmp) = make_server().await;
        let tools = server.tool_router.list_all();
        for name in &["info", "create", "get", "list", "update", "delete"] {
            let tool = tools
                .iter()
                .find(|t| t.name == *name)
                .unwrap_or_else(|| panic!("tool '{name}' must exist"));
            let desc = tool.description.as_deref().unwrap_or("");
            assert!(
                desc.contains("table"),
                "tool '{name}' description must mention 'table' argument, got: {desc:?}"
            );
        }
    }

    // ---------------------------------------------------------------------------
    // T2: info tool
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn info_tool_returns_schema_json() {
        let (server, _tmp) = make_server().await;
        let json = server
            .tool_info(Parameters(InfoParams { table: None }))
            .await
            .expect("info must succeed");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("info must return valid JSON");
        assert_eq!(parsed["table"], "test_table");
        let fields = parsed["fields"]
            .as_array()
            .expect("fields must be an array");
        assert!(!fields.is_empty());
        // Each field must have name, type, required.
        for f in fields {
            assert!(f.get("name").is_some(), "field must have 'name'");
            assert!(f.get("type").is_some(), "field must have 'type'");
            assert!(f.get("required").is_some(), "field must have 'required'");
        }
    }

    // ---------------------------------------------------------------------------
    // T3: create / get roundtrip
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn create_and_get_roundtrip() {
        let (server, _tmp) = make_server().await;

        let created = do_create(
            &server,
            serde_json::json!({ "title": "hello", "state": "open" }),
        )
        .await
        .expect("create must succeed");
        let id = created["id"].as_str().expect("id must be a string");

        let fetched = do_get(&server, id).await.expect("get must succeed");
        assert_eq!(fetched["id"], id);
        assert_eq!(fetched["data"]["title"], "hello");
    }

    // ---------------------------------------------------------------------------
    // T4: list tool
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn list_tool_returns_array() {
        let (server, _tmp) = make_server().await;

        do_create(&server, serde_json::json!({ "title": "row1" }))
            .await
            .unwrap();
        do_create(&server, serde_json::json!({ "title": "row2" }))
            .await
            .unwrap();

        let rows = do_list(&server, None, None)
            .await
            .expect("list must succeed");
        assert!(rows.is_array(), "list must return a JSON array");
        assert_eq!(rows.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn list_tool_with_limit_and_offset() {
        let (server, _tmp) = make_server().await;
        for i in 0..5 {
            do_create(&server, serde_json::json!({ "title": format!("item-{i}") }))
                .await
                .unwrap();
        }
        let rows = do_list(&server, Some(2), Some(1))
            .await
            .expect("list must succeed");
        assert_eq!(rows.as_array().unwrap().len(), 2);
    }

    // ---------------------------------------------------------------------------
    // T5: update tool
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn update_tool_success() {
        let (server, _tmp) = make_server().await;
        let created = do_create(&server, serde_json::json!({ "title": "original" }))
            .await
            .unwrap();
        let id = created["id"].as_str().unwrap();

        let updated = do_update(&server, id, serde_json::json!({ "title": "updated" }), None)
            .await
            .expect("update must succeed");
        assert_eq!(updated["data"]["title"], "updated");
    }

    // ---------------------------------------------------------------------------
    // T6: delete tool
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn delete_tool_success() {
        let (server, _tmp) = make_server().await;
        let created = do_create(&server, serde_json::json!({ "title": "to-delete" }))
            .await
            .unwrap();
        let id = created["id"].as_str().unwrap();

        let resp = do_delete(&server, id).await.expect("delete must succeed");
        assert_eq!(resp["deleted"], id);
    }

    // ---------------------------------------------------------------------------
    // T7: error paths — tool methods return Err(String) on failure
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn create_missing_required_field_returns_err() {
        let (server, _tmp) = make_server().await;
        // `title` is required; passing empty object must fail.
        let result = do_create(&server, serde_json::json!({})).await;
        assert!(result.is_err(), "validation failure must return Err");
    }

    #[tokio::test]
    async fn get_not_found_returns_err() {
        let (server, _tmp) = make_server().await;
        let result = do_get(&server, "nonexistent-id").await;
        assert!(result.is_err(), "not-found must return Err");
    }

    #[tokio::test]
    async fn delete_not_found_returns_err() {
        let (server, _tmp) = make_server().await;
        let result = do_delete(&server, "nonexistent-id").await;
        assert!(result.is_err(), "not-found must return Err");
    }

    #[tokio::test]
    async fn update_not_found_returns_err() {
        let (server, _tmp) = make_server().await;
        let result = do_update(
            &server,
            "nonexistent-id",
            serde_json::json!({ "title": "x" }),
            None,
        )
        .await;
        assert!(result.is_err(), "not-found must return Err");
    }

    // ---------------------------------------------------------------------------
    // T8: Resources — legacy single-table mode
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn list_resources_legacy_mode_has_schema_and_docs() {
        // In legacy (single-table) mode, resource_list() emits 3 schema URIs
        // (with ?table=<name> query) + 4 docs URIs = 7 total.
        let (server, _tmp) = make_server().await;
        let resources = server.resource_list();
        assert_eq!(
            resources.len(),
            7,
            "expected exactly 7 resources in legacy mode, got: {:?}",
            resources.iter().map(|r| &r.uri).collect::<Vec<_>>()
        );
        // Docs URIs must be present without query string.
        let uris: Vec<&str> = resources.iter().map(|r| r.uri.as_str()).collect();
        for expected in &[
            "docs://quickstart",
            "docs://tools",
            "docs://errors",
            "docs://filters",
        ] {
            assert!(
                uris.contains(expected),
                "URI '{expected}' missing from list"
            );
        }
        // Schema URIs must include ?table=test_table
        for prefix in &[
            "schema://yaml?table=",
            "schema://json?table=",
            "schema://json-schema?table=",
        ] {
            assert!(
                uris.iter().any(|u| u.starts_with(prefix)),
                "No schema URI with prefix '{prefix}' found in list: {uris:?}"
            );
        }
    }

    #[tokio::test]
    async fn read_resource_schema_json_with_table_query_returns_schema() {
        let (server, _tmp) = make_server().await;
        let result = server
            .read_resource_impl("schema://json?table=test_table")
            .await
            .expect("schema://json?table=test_table must succeed");
        assert_eq!(result.contents.len(), 1);
        let text = match &result.contents[0] {
            rmcp::model::ResourceContents::TextResourceContents { text, .. } => text.clone(),
            _ => panic!("expected text contents"),
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&text).expect("schema://json must be valid JSON");
        assert_eq!(parsed["table"], "test_table", "table field must match");
        assert!(parsed["fields"].is_array(), "fields must be an array");
    }

    #[tokio::test]
    async fn read_resource_schema_json_no_query_uses_default_in_legacy_mode() {
        // In legacy single-table mode, schema://json (no query) should succeed
        // because default_table is set.
        let (server, _tmp) = make_server().await;
        let result = server
            .read_resource_impl("schema://json")
            .await
            .expect("schema://json must succeed in legacy mode");
        assert_eq!(result.contents.len(), 1);
        let text = match &result.contents[0] {
            rmcp::model::ResourceContents::TextResourceContents { text, .. } => text.clone(),
            _ => panic!("expected text contents"),
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&text).expect("schema://json must be valid JSON");
        assert_eq!(parsed["table"], "test_table");
    }

    #[tokio::test]
    async fn read_resource_json_schema_has_required_array() {
        let (server, _tmp) = make_server().await;
        let result = server
            .read_resource_impl("schema://json-schema?table=test_table")
            .await
            .expect("schema://json-schema?table=test_table must succeed");
        let text = match &result.contents[0] {
            rmcp::model::ResourceContents::TextResourceContents { text, .. } => text.clone(),
            _ => panic!("expected text contents"),
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&text).expect("json-schema must be valid JSON");
        let required = parsed["required"]
            .as_array()
            .expect("required must be an array");
        assert!(
            required.contains(&serde_json::Value::String("title".to_string())),
            "required must contain 'title' (marked required: true in test schema)"
        );
    }

    #[tokio::test]
    async fn read_resource_quickstart_documents_mode_detection() {
        let (server, _tmp) = make_server().await;
        let result = server
            .read_resource_impl("docs://quickstart")
            .await
            .expect("docs://quickstart must succeed");
        let text = match &result.contents[0] {
            rmcp::model::ResourceContents::TextResourceContents { text, .. } => text.clone(),
            _ => panic!("expected text contents"),
        };
        assert!(
            text.starts_with("# mini-app-mcp"),
            "Quickstart must start with '# mini-app-mcp', got: {:?}",
            &text[..text.len().min(40)]
        );
        assert!(
            text.contains("Multi-table mode") && text.contains("Legacy single-table mode"),
            "Quickstart served via docs://quickstart must document both mode flavours so agents can detect which mode the server is running in"
        );
    }

    #[tokio::test]
    async fn read_resource_unknown_uri_returns_err() {
        let (server, _tmp) = make_server().await;
        let result = server.read_resource_impl("unknown://nope").await;
        assert!(result.is_err(), "unknown URI must return Err");
    }

    // ---------------------------------------------------------------------------
    // T9: Multi-table mode tests
    // ---------------------------------------------------------------------------

    /// Build a multi-table server with two tables (table_a and table_b).
    async fn make_multi_table_server() -> MiniAppMcpServer {
        use crate::mcp::registry::TableEntry;
        use std::collections::HashMap;

        // Build schemas for two tables.
        let schema_a = SchemaConfig {
            table: "table_a".to_string(),
            title: None,
            description: None,
            fields: vec![FieldDef {
                name: "name".to_string(),
                ty: FieldType::String,
                required: true,
                description: None,
            }],
            dump: None,
        };
        let schema_b = SchemaConfig {
            table: "table_b".to_string(),
            title: None,
            description: None,
            fields: vec![FieldDef {
                name: "value".to_string(),
                ty: FieldType::Number,
                required: false,
                description: None,
            }],
            dump: None,
        };

        let store_a = Store::open(Path::new(":memory:"), schema_a.clone())
            .await
            .expect("in-memory store_a");
        let store_b = Store::open(Path::new(":memory:"), schema_b.clone())
            .await
            .expect("in-memory store_b");

        let mut entries: HashMap<String, TableEntry> = HashMap::new();
        entries.insert(
            "table_a".to_string(),
            TableEntry {
                store: Arc::new(store_a),
                schema: Arc::new(schema_a),
                schema_path: Arc::new(PathBuf::from("/fake/table_a/schema.yaml")),
            },
        );
        entries.insert(
            "table_b".to_string(),
            TableEntry {
                store: Arc::new(store_b),
                schema: Arc::new(schema_b),
                schema_path: Arc::new(PathBuf::from("/fake/table_b/schema.yaml")),
            },
        );

        let registry = TableRegistry::from_entries(entries, None);
        let config = Arc::new(Config {
            schema_path: None,
            db_path: None,
            user_dir: None,
            project_dir: None,
            backup_retention: None,
            snapshot_retention: None,
        });
        MiniAppMcpServer::new_multi(registry, config)
    }

    #[tokio::test]
    async fn multi_table_create_with_table_arg_succeeds() {
        let server = make_multi_table_server().await;
        let json = server
            .tool_create(Parameters(CreateParams {
                data: serde_json::json!({ "name": "alice" }),
                table: Some("table_a".to_string()),
            }))
            .await
            .expect("create with table=Some(table_a) must succeed");
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["data"]["name"], "alice");
    }

    #[tokio::test]
    async fn multi_table_create_without_table_arg_returns_table_required() {
        let server = make_multi_table_server().await;
        let result = server
            .tool_create(Parameters(CreateParams {
                data: serde_json::json!({ "name": "bob" }),
                table: None,
            }))
            .await;
        assert!(
            result.is_err(),
            "create with table=None in multi-table mode must fail"
        );
        let err_str = result.unwrap_err();
        assert!(
            err_str.contains("TABLE_REQUIRED") || err_str.contains("table argument"),
            "error must indicate TABLE_REQUIRED, got: {err_str}"
        );
    }

    #[tokio::test]
    async fn multi_table_get_with_nonexistent_table_returns_table_not_found() {
        let server = make_multi_table_server().await;
        let result = server
            .tool_get(Parameters(GetParams {
                id: "some-id".to_string(),
                table: Some("nonexistent".to_string()),
                fields: None,
            }))
            .await;
        assert!(result.is_err(), "get with unknown table must fail");
        let err_str = result.unwrap_err();
        assert!(
            err_str.contains("TABLE_NOT_FOUND") || err_str.contains("table not found"),
            "error must indicate TABLE_NOT_FOUND, got: {err_str}"
        );
    }

    #[tokio::test]
    async fn multi_table_resource_list_has_entries_for_each_table() {
        let server = make_multi_table_server().await;
        let resources = server.resource_list();
        // 2 tables × 3 schema resources + 4 docs = 10
        assert_eq!(
            resources.len(),
            10,
            "expected 10 resources for 2 tables (2×3 schema + 4 docs), got: {:?}",
            resources.iter().map(|r| &r.uri).collect::<Vec<_>>()
        );
        let uris: Vec<&str> = resources.iter().map(|r| r.uri.as_str()).collect();
        // Each table should have its own schema URIs
        assert!(uris.contains(&"schema://json?table=table_a"));
        assert!(uris.contains(&"schema://json?table=table_b"));
    }

    #[tokio::test]
    async fn multi_table_read_resource_schema_json_with_table_query() {
        let server = make_multi_table_server().await;
        let result = server
            .read_resource_impl("schema://json?table=table_a")
            .await
            .expect("schema://json?table=table_a must succeed");
        let text = match &result.contents[0] {
            rmcp::model::ResourceContents::TextResourceContents { text, .. } => text.clone(),
            _ => panic!("expected text contents"),
        };
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["table"], "table_a");
    }

    #[tokio::test]
    async fn multi_table_read_resource_no_query_returns_err() {
        // In multi-table mode (no default_table), schema://json without ?table=
        // must return an error (TableRequired).
        let server = make_multi_table_server().await;
        let result = server.read_resource_impl("schema://json").await;
        assert!(
            result.is_err(),
            "schema://json without ?table= in multi-table mode must fail"
        );
    }

    // ---------------------------------------------------------------------------
    // T10: parse_table_query helper
    // ---------------------------------------------------------------------------

    #[test]
    fn parse_table_query_no_query_string() {
        let (base, table) = parse_table_query("schema://json");
        assert_eq!(base, "schema://json");
        assert_eq!(table, None);
    }

    #[test]
    fn parse_table_query_with_table_param() {
        let (base, table) = parse_table_query("schema://json?table=my_table");
        assert_eq!(base, "schema://json");
        assert_eq!(table, Some("my_table"));
    }

    #[test]
    fn parse_table_query_multiple_params_extracts_table() {
        let (base, table) = parse_table_query("schema://json?foo=bar&table=tbl&baz=qux");
        assert_eq!(base, "schema://json");
        assert_eq!(table, Some("tbl"));
    }

    #[test]
    fn parse_table_query_no_table_param_in_query_string() {
        let (base, table) = parse_table_query("schema://json?foo=bar");
        assert_eq!(base, "schema://json");
        assert_eq!(table, None);
    }

    // ---------------------------------------------------------------------------
    // T11: registry_diff helper
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn registry_diff_added_and_removed() {
        use crate::mcp::registry::TableEntry;
        use std::collections::HashMap;

        let schema_a = SchemaConfig {
            table: "table_a".to_string(),
            title: None,
            description: None,
            fields: vec![],
            dump: None,
        };
        let schema_b = SchemaConfig {
            table: "table_b".to_string(),
            title: None,
            description: None,
            fields: vec![],
            dump: None,
        };
        let schema_c = SchemaConfig {
            table: "table_c".to_string(),
            title: None,
            description: None,
            fields: vec![],
            dump: None,
        };

        // old registry: table_a, table_b
        let mut old_entries: HashMap<String, TableEntry> = HashMap::new();
        old_entries.insert(
            "table_a".to_string(),
            TableEntry {
                store: Arc::new(
                    Store::open(Path::new(":memory:"), schema_a.clone())
                        .await
                        .expect("store_a"),
                ),
                schema: Arc::new(schema_a.clone()),
                schema_path: Arc::new(PathBuf::from("/fake/a/schema.yaml")),
            },
        );
        old_entries.insert(
            "table_b".to_string(),
            TableEntry {
                store: Arc::new(
                    Store::open(Path::new(":memory:"), schema_b.clone())
                        .await
                        .expect("store_b"),
                ),
                schema: Arc::new(schema_b.clone()),
                schema_path: Arc::new(PathBuf::from("/fake/b/schema.yaml")),
            },
        );
        let old_registry = TableRegistry::from_entries(old_entries, None);

        // new registry: table_a, table_c (b removed, c added)
        let mut new_entries: HashMap<String, TableEntry> = HashMap::new();
        new_entries.insert(
            "table_a".to_string(),
            TableEntry {
                store: Arc::new(
                    Store::open(Path::new(":memory:"), schema_a.clone())
                        .await
                        .expect("store_a2"),
                ),
                schema: Arc::new(schema_a),
                schema_path: Arc::new(PathBuf::from("/fake/a/schema.yaml")),
            },
        );
        new_entries.insert(
            "table_c".to_string(),
            TableEntry {
                store: Arc::new(
                    Store::open(Path::new(":memory:"), schema_c.clone())
                        .await
                        .expect("store_c"),
                ),
                schema: Arc::new(schema_c),
                schema_path: Arc::new(PathBuf::from("/fake/c/schema.yaml")),
            },
        );
        let new_registry = TableRegistry::from_entries(new_entries, None);

        let (mut added, mut removed) = registry_diff(&old_registry, &new_registry);
        added.sort();
        removed.sort();
        assert_eq!(added, vec!["table_c"]);
        assert_eq!(removed, vec!["table_b"]);
    }

    // ---------------------------------------------------------------------------
    // T12: reload round-trip integration test
    // Acceptance Criteria #8: mount → schema add → reload → diff verify
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn reload_round_trip_with_new_directory() {
        // Create a temp directory to act as the user_dir.
        let tmp_dir = tempfile::tempdir().expect("tempdir");
        let user_dir = tmp_dir.path();

        // Initial state: table_alpha exists.
        let alpha_dir = user_dir.join("table_alpha");
        std::fs::create_dir_all(&alpha_dir).expect("create table_alpha dir");
        let alpha_schema = alpha_dir.join("schema.yaml");
        std::fs::write(
            &alpha_schema,
            b"table: table_alpha\nfields:\n  - name: x\n    type: string\n    required: false\n",
        )
        .expect("write alpha schema");

        // Mount initial registry.
        let registry = TableRegistry::mount_from_dirs(Some(user_dir), None)
            .await
            .expect("initial mount");
        assert_eq!(registry.table_count(), 1, "should have table_alpha");

        let config = Arc::new(Config {
            schema_path: None,
            db_path: None,
            user_dir: Some(user_dir.to_path_buf()),
            project_dir: None,
            backup_retention: None,
            snapshot_retention: None,
        });
        let server = MiniAppMcpServer::new_multi(registry, Arc::clone(&config));

        // Add a second table: table_beta
        let beta_dir = user_dir.join("table_beta");
        std::fs::create_dir_all(&beta_dir).expect("create table_beta dir");
        let beta_schema = beta_dir.join("schema.yaml");
        std::fs::write(
            &beta_schema,
            b"table: table_beta\nfields:\n  - name: y\n    type: string\n    required: false\n",
        )
        .expect("write beta schema");

        // Call reload.
        let reload_json = server.tool_reload().await.expect("reload must succeed");
        let result: serde_json::Value =
            serde_json::from_str(&reload_json).expect("reload must return valid JSON");

        // After reload: 2 tables mounted.
        assert_eq!(
            result["mounted"],
            serde_json::json!(2),
            "reload must report 2 mounted tables"
        );
        let added = result["added"].as_array().expect("added must be array");
        assert_eq!(added.len(), 1, "one table was added");
        assert!(
            added.contains(&serde_json::json!("table_beta")),
            "table_beta must be in added"
        );
        let removed = result["removed"].as_array().expect("removed must be array");
        assert_eq!(removed.len(), 0, "no tables were removed");

        // Verify the new registry is visible via info tool.
        let info_json = server
            .tool_info(Parameters(InfoParams {
                table: Some("table_beta".to_string()),
            }))
            .await
            .expect("info for table_beta must succeed after reload");
        let info: serde_json::Value =
            serde_json::from_str(&info_json).expect("info must be valid JSON");
        assert_eq!(info["table"], "table_beta");
    }

    // ---------------------------------------------------------------------------
    // T13: reload returns CONFIG_ERROR when server was constructed via new_single
    // (all mount_config fields are None — no directory to re-scan).
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn tool_info_includes_title_and_description() {
        use crate::mcp::registry::TableEntry;
        use std::collections::HashMap;

        // Build a schema with title and description set.
        let schema = SchemaConfig {
            table: "annotated".to_string(),
            title: Some("Annotated Table".to_string()),
            description: Some("A table with metadata for round-trip test.".to_string()),
            fields: vec![FieldDef {
                name: "content".to_string(),
                ty: FieldType::String,
                required: true,
                description: Some("The main content field.".to_string()),
            }],
            dump: None,
        };

        let store = Store::open(Path::new(":memory:"), schema.clone())
            .await
            .expect("in-memory store");

        let mut entries: HashMap<String, TableEntry> = HashMap::new();
        entries.insert(
            "annotated".to_string(),
            TableEntry {
                store: Arc::new(store),
                schema: Arc::new(schema),
                schema_path: Arc::new(PathBuf::from("/fake/annotated/schema.yaml")),
            },
        );

        let registry = TableRegistry::from_entries(entries, None);
        let config = Arc::new(Config {
            schema_path: None,
            db_path: None,
            user_dir: None,
            project_dir: None,
            backup_retention: None,
            snapshot_retention: None,
        });
        let server = MiniAppMcpServer::new_multi(registry, config);

        let json = server
            .tool_info(Parameters(InfoParams {
                table: Some("annotated".to_string()),
            }))
            .await
            .expect("info must succeed");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("info must return valid JSON");

        assert_eq!(
            parsed["title"],
            serde_json::Value::String("Annotated Table".to_string()),
            "tool_info must include title"
        );
        assert_eq!(
            parsed["description"],
            serde_json::Value::String("A table with metadata for round-trip test.".to_string()),
            "tool_info must include description"
        );
        assert_eq!(
            parsed["fields"][0]["description"],
            serde_json::Value::String("The main content field.".to_string()),
            "tool_info field must include description"
        );
    }

    #[tokio::test]
    async fn reload_returns_config_error_on_legacy_server() {
        // `make_server()` uses `new_single` which sets all Config fields to None.
        let (server, _tmp) = make_server().await;

        let result = server.tool_reload().await;
        assert!(
            result.is_err(),
            "reload on a new_single server must return Err"
        );
        // The error string must carry CONFIG_ERROR so callers can identify the
        // rejection without parsing the human-readable message.
        let err_str = result.unwrap_err().to_string();
        assert!(
            err_str.contains("CONFIG_ERROR") || err_str.contains("reload not configured"),
            "error must indicate CONFIG_ERROR or 'reload not configured', got: {err_str}"
        );
    }

    // ---------------------------------------------------------------------------
    // Helper: server with an extra optional `meta` (object) field for nested tests.
    // ---------------------------------------------------------------------------

    async fn make_server_with_meta() -> (MiniAppMcpServer, tempfile::NamedTempFile) {
        use std::io::Write as _;
        let schema_yaml = b"\
table: test_table\n\
fields:\n\
  - name: title\n\
    type: string\n\
    required: true\n\
  - name: state\n\
    type: string\n\
    required: false\n\
  - name: meta\n\
    type: object\n\
    required: false\n";

        let mut tmp = tempfile::NamedTempFile::new().expect("temp file");
        tmp.write_all(schema_yaml).expect("write schema yaml");

        let schema = SchemaConfig {
            table: "test_table".to_string(),
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
                    name: "state".to_string(),
                    ty: FieldType::String,
                    required: false,
                    description: None,
                },
                FieldDef {
                    name: "meta".to_string(),
                    ty: FieldType::Object,
                    required: false,
                    description: None,
                },
            ],
            dump: None,
        };
        let store = Store::open(Path::new(":memory:"), schema.clone())
            .await
            .expect("in-memory store must open");
        let schema_path = tmp.path().to_path_buf();
        (
            MiniAppMcpServer::new_single(store, schema, schema_path),
            tmp,
        )
    }

    // ---------------------------------------------------------------------------
    // Grid tests: merge / replace mode semantics (Crux #1 + #2 end-to-end via tool_update)
    // ---------------------------------------------------------------------------

    /// Crux #2: mode="replace" must produce results byte-for-byte identical to
    /// the pre-breaking-change full-replacement behavior.
    #[tokio::test]
    async fn update_replace_mode_round_trip_identity() {
        let (server, _tmp) = make_server().await;
        let created = do_create(
            &server,
            serde_json::json!({ "title": "original", "state": "open" }),
        )
        .await
        .unwrap();
        let id = created["id"].as_str().unwrap();

        let patch = serde_json::json!({ "title": "replaced" });
        let updated = do_update(&server, id, patch.clone(), Some(UpdateMode::Replace))
            .await
            .expect("replace must succeed");

        // result.data must equal patch exactly (byte-for-byte: only "title" key present).
        assert_eq!(
            updated["data"], patch,
            "Replace mode: result.data must equal the patch object exactly"
        );
        // The "state" field must NOT be present (full replacement, not merge).
        assert!(
            updated["data"].get("state").is_none(),
            "Replace mode: absent patch fields must not appear in result.data"
        );
    }

    /// Crux #1 (a): merge default preserves fields absent from the patch.
    #[tokio::test]
    async fn update_merge_mode_default_preserves_absent_fields() {
        let (server, _tmp) = make_server().await;
        let created = do_create(
            &server,
            serde_json::json!({ "title": "x", "state": "open" }),
        )
        .await
        .unwrap();
        let id = created["id"].as_str().unwrap();

        // Patch only `state`; `title` is absent from the patch.
        let updated = do_update(
            &server,
            id,
            serde_json::json!({ "state": "closed" }),
            None, // Merge default
        )
        .await
        .expect("merge must succeed");

        assert_eq!(
            updated["data"]["title"], "x",
            "Absent field must be preserved"
        );
        assert_eq!(
            updated["data"]["state"], "closed",
            "Patched field must be updated"
        );
    }

    /// Crux #1 (b): null on an optional field deletes it from the stored row.
    #[tokio::test]
    async fn update_merge_mode_deletes_optional_field_on_null() {
        let (server, _tmp) = make_server().await;
        let created = do_create(
            &server,
            serde_json::json!({ "title": "x", "state": "open" }),
        )
        .await
        .unwrap();
        let id = created["id"].as_str().unwrap();

        // `state` is optional; null must delete it.
        let updated = do_update(
            &server,
            id,
            serde_json::json!({ "state": serde_json::Value::Null }),
            Some(UpdateMode::Merge),
        )
        .await
        .expect("null on optional must succeed");

        assert!(
            updated["data"].get("state").is_none(),
            "Optional field set to null must be absent (physically removed) from merged result"
        );
    }

    /// Crux #1 (c): null on a required field must return a Validation error.
    #[tokio::test]
    async fn update_merge_mode_returns_validation_on_null_required() {
        let (server, _tmp) = make_server().await;
        let created = do_create(&server, serde_json::json!({ "title": "x" }))
            .await
            .unwrap();
        let id = created["id"].as_str().unwrap();

        // `title` is required; null must be rejected.
        let result = do_update(
            &server,
            id,
            serde_json::json!({ "title": serde_json::Value::Null }),
            Some(UpdateMode::Merge),
        )
        .await;

        assert!(
            result.is_err(),
            "null on required field must return Err (Validation error)"
        );
    }

    /// Crux #1 (d): post-merge schema validation catches type mismatches.
    #[tokio::test]
    async fn update_merge_mode_runs_post_merge_validation() {
        let (server, _tmp) = make_server().await;
        let created = do_create(&server, serde_json::json!({ "title": "x" }))
            .await
            .unwrap();
        let id = created["id"].as_str().unwrap();

        // `title` is String; passing a Number must trigger post-merge validation error.
        let result = do_update(
            &server,
            id,
            serde_json::json!({ "title": 42 }),
            Some(UpdateMode::Merge),
        )
        .await;

        assert!(
            result.is_err(),
            "type mismatch in patch must be caught by post-merge validation"
        );
    }

    /// Crux #1 (nested): nested objects are replaced wholesale, not deep-merged.
    #[tokio::test]
    async fn update_merge_mode_replaces_nested_object_wholesale() {
        let (server, _tmp) = make_server_with_meta().await;
        let created = do_create(
            &server,
            serde_json::json!({ "title": "x", "meta": { "a": 1, "b": 2 } }),
        )
        .await
        .unwrap();
        let id = created["id"].as_str().unwrap();

        // Patch `meta` with only `a: 9`; `b` must NOT be preserved (no deep merge).
        let updated = do_update(
            &server,
            id,
            serde_json::json!({ "meta": { "a": 9 } }),
            Some(UpdateMode::Merge),
        )
        .await
        .expect("wholesale object replace must succeed");

        assert_eq!(
            updated["data"]["meta"],
            serde_json::json!({ "a": 9 }),
            "Nested object must be replaced wholesale, not deep-merged"
        );
    }

    // ---------------------------------------------------------------------------
    // Field projection integration tests (list / get / alias_run)
    // ---------------------------------------------------------------------------

    async fn do_list_with_fields(
        server: &MiniAppMcpServer,
        fields: Option<FieldSelector>,
    ) -> Result<serde_json::Value, String> {
        let json = server
            .tool_list(Parameters(ListParams {
                limit: None,
                offset: None,
                table: None,
                filter: None,
                fields,
            }))
            .await?;
        Ok(serde_json::from_str(&json).unwrap())
    }

    async fn do_get_with_fields(
        server: &MiniAppMcpServer,
        id: &str,
        fields: Option<FieldSelector>,
    ) -> Result<serde_json::Value, String> {
        let json = server
            .tool_get(Parameters(GetParams {
                id: id.to_string(),
                table: None,
                fields,
            }))
            .await?;
        Ok(serde_json::from_str(&json).unwrap())
    }

    #[tokio::test]
    async fn list_without_fields_returns_all() {
        let (server, _tmp) = make_server().await;
        do_create(
            &server,
            serde_json::json!({"title": "hello", "state": "active"}),
        )
        .await
        .unwrap();
        let rows = do_list_with_fields(&server, None).await.unwrap();
        let rows = rows.as_array().unwrap();
        assert_eq!(rows.len(), 1);
        // Both fields must be present.
        assert!(rows[0]["data"]["title"].is_string());
        assert!(rows[0]["data"]["state"].is_string());
    }

    #[tokio::test]
    async fn list_with_fields_projection() {
        let (server, _tmp) = make_server().await;
        do_create(
            &server,
            serde_json::json!({"title": "hello", "state": "active"}),
        )
        .await
        .unwrap();
        let fields = Some(FieldSelector::List {
            fields: vec!["title".to_string()],
        });
        let rows = do_list_with_fields(&server, fields).await.unwrap();
        let rows = rows.as_array().unwrap();
        assert_eq!(rows.len(), 1);
        // Only "title" should be present in data.
        assert!(rows[0]["data"]["title"].is_string());
        assert!(rows[0]["data"].get("state").is_none() || rows[0]["data"]["state"].is_null());
        // id / created_at / updated_at must be preserved.
        assert!(rows[0]["id"].is_string());
        assert!(rows[0]["created_at"].is_number());
    }

    #[tokio::test]
    async fn list_with_fields_all_returns_all() {
        let (server, _tmp) = make_server().await;
        do_create(
            &server,
            serde_json::json!({"title": "hello", "state": "active"}),
        )
        .await
        .unwrap();
        let fields = Some(FieldSelector::All);
        let rows = do_list_with_fields(&server, fields).await.unwrap();
        let rows = rows.as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0]["data"]["title"].is_string());
        assert!(rows[0]["data"]["state"].is_string());
    }

    #[tokio::test]
    async fn list_with_unknown_field_returns_validation_error() {
        let (server, _tmp) = make_server().await;
        do_create(
            &server,
            serde_json::json!({"title": "hello", "state": "active"}),
        )
        .await
        .unwrap();
        let fields = Some(FieldSelector::List {
            fields: vec!["nonexistent".to_string()],
        });
        let result = do_list_with_fields(&server, fields).await;
        assert!(result.is_err(), "unknown field must return error");
        let err = result.unwrap_err();
        assert!(
            err.contains("VALIDATION_ERROR") || err.contains("nonexistent"),
            "error must mention VALIDATION_ERROR or field name, got: {err}"
        );
    }

    #[tokio::test]
    async fn get_without_fields_returns_all() {
        let (server, _tmp) = make_server().await;
        let created = do_create(
            &server,
            serde_json::json!({"title": "hello", "state": "active"}),
        )
        .await
        .unwrap();
        let id = created["id"].as_str().unwrap();
        let row = do_get_with_fields(&server, id, None).await.unwrap();
        assert!(row["data"]["title"].is_string());
        assert!(row["data"]["state"].is_string());
    }

    #[tokio::test]
    async fn get_with_fields_projection() {
        let (server, _tmp) = make_server().await;
        let created = do_create(
            &server,
            serde_json::json!({"title": "hello", "state": "active"}),
        )
        .await
        .unwrap();
        let id = created["id"].as_str().unwrap();
        let fields = Some(FieldSelector::List {
            fields: vec!["state".to_string()],
        });
        let row = do_get_with_fields(&server, id, fields).await.unwrap();
        // Only "state" should be present.
        assert!(row["data"]["state"].is_string());
        assert!(row["data"].get("title").is_none() || row["data"]["title"].is_null());
        // Metadata must be preserved.
        assert_eq!(row["id"].as_str().unwrap(), id);
        assert!(row["created_at"].is_number());
    }

    #[tokio::test]
    async fn get_with_unknown_field_returns_validation_error() {
        let (server, _tmp) = make_server().await;
        let created = do_create(
            &server,
            serde_json::json!({"title": "hello", "state": "active"}),
        )
        .await
        .unwrap();
        let id = created["id"].as_str().unwrap();
        let fields = Some(FieldSelector::List {
            fields: vec!["nonexistent".to_string()],
        });
        let result = do_get_with_fields(&server, id, fields).await;
        assert!(result.is_err(), "unknown field must return error");
        let err = result.unwrap_err();
        assert!(
            err.contains("VALIDATION_ERROR") || err.contains("nonexistent"),
            "error must mention VALIDATION_ERROR or field name, got: {err}"
        );
    }

    #[tokio::test]
    async fn alias_run_with_fields_projection() {
        let (server, _tmp) = make_server().await;
        do_create(
            &server,
            serde_json::json!({"title": "hello", "state": "active"}),
        )
        .await
        .unwrap();
        // Create a plain alias.
        server
            .tool_alias_create(Parameters(AliasCreateParams {
                table: None,
                sources: None,
                aggregator: None,
                name: "all_rows".to_string(),
                filter: Some(crate::filter::ListFilter::Eq {
                    field: "state".to_string(),
                    value: serde_json::json!("active"),
                }),
                filter_template: None,
                params_schema: None,
                limit: None,
                description: None,
                fields: None,
                scope: None,
            }))
            .await
            .unwrap();
        // Run alias with field projection.
        let json = server
            .tool_alias_run(Parameters(AliasRunParams {
                table: None,
                name: "all_rows".to_string(),
                limit: None,
                offset: None,
                params: None,
                fields: Some(FieldSelector::List {
                    fields: vec!["title".to_string()],
                }),
            }))
            .await
            .unwrap();
        let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
        let rows = rows.as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["data"]["title"], "hello");
        // "state" must not be in projected data.
        assert!(rows[0]["data"].get("state").is_none() || rows[0]["data"]["state"].is_null());
    }

    #[tokio::test]
    async fn alias_run_with_unknown_field_returns_validation_error() {
        let (server, _tmp) = make_server().await;
        do_create(
            &server,
            serde_json::json!({"title": "hello", "state": "active"}),
        )
        .await
        .unwrap();
        server
            .tool_alias_create(Parameters(AliasCreateParams {
                table: None,
                sources: None,
                aggregator: None,
                name: "all_rows2".to_string(),
                filter: Some(crate::filter::ListFilter::Eq {
                    field: "state".to_string(),
                    value: serde_json::json!("active"),
                }),
                filter_template: None,
                params_schema: None,
                limit: None,
                description: None,
                fields: None,
                scope: None,
            }))
            .await
            .unwrap();
        let result = server
            .tool_alias_run(Parameters(AliasRunParams {
                table: None,
                name: "all_rows2".to_string(),
                limit: None,
                offset: None,
                params: None,
                fields: Some(FieldSelector::List {
                    fields: vec!["nonexistent".to_string()],
                }),
            }))
            .await;
        assert!(result.is_err(), "unknown field must return error");
        let err = result.unwrap_err();
        assert!(
            err.contains("VALIDATION_ERROR") || err.contains("nonexistent"),
            "error must mention VALIDATION_ERROR or field name, got: {err}"
        );
    }

    // ---------------------------------------------------------------------------
    // alias_create / alias_delete scope handling — covers the user-only mount
    // environment (= Claude Code default env, where the CWD has no `.mini-app/`
    // dir so Project scope is filtered out at `registry.rs:109-110`). Prior to
    // the v0.12.1 fix, `tool_alias_create` hardcoded `AliasScope::Project` and
    // always failed with "GlobalAliasStorage scope Project is not mounted" in
    // this environment. The new dispatch auto-selects User scope as a fallback
    // and accepts an explicit `scope` parameter from the caller.
    // ---------------------------------------------------------------------------

    async fn make_user_only_server() -> (MiniAppMcpServer, tempfile::TempDir) {
        let tmp_dir = tempfile::tempdir().expect("tempdir");
        let user_dir = tmp_dir.path();
        let table_dir = user_dir.join("test_table");
        std::fs::create_dir_all(&table_dir).expect("create table_dir");
        std::fs::write(
            table_dir.join("schema.yaml"),
            b"table: test_table\nfields:\n  - name: title\n    type: string\n    required: true\n",
        )
        .expect("write schema");
        let registry = TableRegistry::mount_from_dirs(Some(user_dir), None)
            .await
            .expect("mount user-only");
        let config = Arc::new(Config {
            schema_path: None,
            db_path: None,
            user_dir: Some(user_dir.to_path_buf()),
            project_dir: None,
            backup_retention: None,
            snapshot_retention: None,
        });
        let server = MiniAppMcpServer::new_multi(registry, config);
        (server, tmp_dir)
    }

    #[tokio::test]
    async fn alias_create_user_only_mount_default_scope_falls_back_to_user() {
        let (server, _tmp) = make_user_only_server().await;
        let result = server
            .tool_alias_create(Parameters(AliasCreateParams {
                table: Some("test_table".to_string()),
                sources: None,
                aggregator: None,
                name: "u_default".to_string(),
                filter: Some(crate::filter::ListFilter::Eq {
                    field: "title".to_string(),
                    value: serde_json::json!("hi"),
                }),
                filter_template: None,
                params_schema: None,
                limit: None,
                description: None,
                fields: None,
                scope: None,
            }))
            .await;
        assert!(
            result.is_ok(),
            "alias_create with default scope must succeed in user-only mount: {result:?}"
        );
    }

    #[tokio::test]
    async fn alias_create_user_only_mount_explicit_user_scope_succeeds() {
        let (server, _tmp) = make_user_only_server().await;
        let result = server
            .tool_alias_create(Parameters(AliasCreateParams {
                table: Some("test_table".to_string()),
                sources: None,
                aggregator: None,
                name: "u_explicit".to_string(),
                filter: Some(crate::filter::ListFilter::Eq {
                    field: "title".to_string(),
                    value: serde_json::json!("hi"),
                }),
                filter_template: None,
                params_schema: None,
                limit: None,
                description: None,
                fields: None,
                scope: Some(AliasScope::User),
            }))
            .await;
        assert!(
            result.is_ok(),
            "alias_create with explicit User scope must succeed in user-only mount: {result:?}"
        );
    }

    #[tokio::test]
    async fn alias_create_user_only_mount_explicit_project_scope_returns_clear_error() {
        let (server, _tmp) = make_user_only_server().await;
        let result = server
            .tool_alias_create(Parameters(AliasCreateParams {
                table: Some("test_table".to_string()),
                sources: None,
                aggregator: None,
                name: "u_proj_fail".to_string(),
                filter: Some(crate::filter::ListFilter::Eq {
                    field: "title".to_string(),
                    value: serde_json::json!("hi"),
                }),
                filter_template: None,
                params_schema: None,
                limit: None,
                description: None,
                fields: None,
                scope: Some(AliasScope::Project),
            }))
            .await;
        assert!(
            result.is_err(),
            "alias_create with explicit Project scope must fail in user-only mount"
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("not mounted") && err.contains("Project"),
            "error must mention unmounted Project scope, got: {err}"
        );
    }

    #[tokio::test]
    async fn alias_delete_user_only_mount_round_trip() {
        let (server, _tmp) = make_user_only_server().await;
        server
            .tool_alias_create(Parameters(AliasCreateParams {
                table: Some("test_table".to_string()),
                sources: None,
                aggregator: None,
                name: "u_to_delete".to_string(),
                filter: Some(crate::filter::ListFilter::Eq {
                    field: "title".to_string(),
                    value: serde_json::json!("bye"),
                }),
                filter_template: None,
                params_schema: None,
                limit: None,
                description: None,
                fields: None,
                scope: None,
            }))
            .await
            .expect("alias_create must succeed");
        let result = server
            .tool_alias_delete(Parameters(AliasDeleteParams {
                table: Some("test_table".to_string()),
                name: "u_to_delete".to_string(),
            }))
            .await;
        assert!(
            result.is_ok(),
            "alias_delete must succeed in user-only mount: {result:?}"
        );
    }
}
