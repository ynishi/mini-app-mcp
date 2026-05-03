/// MCP server implementation for mini-app-mcp.
///
/// Exposes 6 tools (`info`, `create`, `get`, `list`, `update`, `delete`) and
/// resources (`schema://yaml`, `schema://json`, `schema://json-schema`,
/// `docs://readme`, `docs://tools`, `docs://errors`) as MCP capabilities over
/// stdio transport.  No HTTP / REST / CLI-CRUD entry points are provided
/// (Crux "MCP-only entry point" constraint).
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
use std::path::PathBuf;
use std::sync::Arc;

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
use serde::Deserialize;

use crate::config::Config;
use crate::error::MiniAppError;
use crate::mcp::registry::TableRegistry;
use crate::mcp::resources as res;
use crate::schema::SchemaConfig;
use crate::store::Store;

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
/// - No tables were mounted (both dir scans yielded nothing and no legacy env).
/// - The transport setup fails.
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

    let server = MiniAppMcpServer::new_multi(Arc::new(registry));
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

// =============================================================================
// MCP Server
// =============================================================================

/// The MCP server for mini-app-mcp.
///
/// Holds an `Arc<TableRegistry>` which is shared across clones.  The server is
/// `Clone` because `rmcp` clones it per connection.
///
/// Use [`MiniAppMcpServer::new_multi`] for multi-table mode and
/// [`MiniAppMcpServer::new_single`] for the legacy single-table adapter (also
/// used in tests).
#[derive(Clone)]
pub struct MiniAppMcpServer {
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
    /// Registry of all mounted tables.
    tables: Arc<TableRegistry>,
}

impl MiniAppMcpServer {
    /// Create a server backed by a pre-built [`TableRegistry`].
    ///
    /// This is the primary constructor for multi-table mode.  The registry
    /// must be built (and validated for ≥1 table) by the caller before
    /// calling this method.
    pub fn new_multi(tables: Arc<TableRegistry>) -> Self {
        Self {
            tool_router: Self::tool_router(),
            tables,
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
        Self {
            tool_router: Self::tool_router(),
            tables: Arc::new(registry),
        }
    }

    /// Resolve a table, falling back to `default_table` when `table` is `None`.
    ///
    /// Returns a pair `(Arc<Store>, Arc<SchemaConfig>)` for the resolved table.
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
        let entry = self.tables.resolve(table)?;
        Ok((Arc::clone(&entry.store), Arc::clone(&entry.schema)))
    }
}

// =============================================================================
// Resource support — private helpers
// =============================================================================

/// Base URIs for schema resources (without query string).
const URI_SCHEMA_YAML: &str = "schema://yaml";
const URI_SCHEMA_JSON: &str = "schema://json";
const URI_SCHEMA_JSON_SCHEMA: &str = "schema://json-schema";
/// Full URIs for documentation resources (no query params).
const URI_DOCS_README: &str = "docs://readme";
const URI_DOCS_TOOLS: &str = "docs://tools";
const URI_DOCS_ERRORS: &str = "docs://errors";

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

        // Schema resources — emitted once per mounted table when a default
        // table is set (legacy mode), otherwise emitted once with a
        // query-param description.
        let default_table = self.tables.default_table();
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
            let mut table_names: Vec<&str> = self.tables.table_names().collect();
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
            RawResource::new(URI_DOCS_README, "README")
                .with_description("README.md — embedded in the binary at compile time.")
                .with_mime_type("text/markdown")
                .no_annotation(),
        );
        resources.push(
            RawResource::new(URI_DOCS_TOOLS, "Tools Reference")
                .with_description(
                    "Cheat sheet listing all 6 tools with descriptions and input shapes.",
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

        resources
    }

    /// Inner implementation of `read_resource` — tested directly to avoid
    /// `RequestContext` construction issues in tests (rmcp 1.5 makes
    /// `RequestContext` `#[non_exhaustive]` so it cannot be built in external
    /// crates).
    async fn read_resource_impl(&self, uri: &str) -> Result<ReadResourceResult, McpError> {
        let (base_uri, table_query) = parse_table_query(uri);

        let contents = match base_uri {
            URI_SCHEMA_YAML => {
                let entry = self.tables.resolve(table_query).map_err(McpError::from)?;
                let text = std::fs::read_to_string(entry.schema_path.as_ref()).map_err(|e| {
                    McpError::internal_error(format!("failed to read schema.yaml: {e}"), None)
                })?;
                ResourceContents::text(text, uri).with_mime_type("application/yaml")
            }
            URI_SCHEMA_JSON => {
                let entry = self.tables.resolve(table_query).map_err(McpError::from)?;
                let text = serde_json::to_string_pretty(entry.schema.as_ref())
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                ResourceContents::text(text, uri).with_mime_type("application/json")
            }
            URI_SCHEMA_JSON_SCHEMA => {
                let entry = self.tables.resolve(table_query).map_err(McpError::from)?;
                let js = res::derive_json_schema(entry.schema.as_ref());
                let text = serde_json::to_string_pretty(&js)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                ResourceContents::text(text, uri).with_mime_type("application/schema+json")
            }
            URI_DOCS_README => {
                ResourceContents::text(res::README, uri).with_mime_type("text/markdown")
            }
            URI_DOCS_TOOLS => {
                ResourceContents::text(res::TOOLS_DOC, uri).with_mime_type("text/markdown")
            }
            URI_DOCS_ERRORS => {
                ResourceContents::text(res::ERRORS_DOC, uri).with_mime_type("text/markdown")
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
             6 tools: info, create, get, list, update, delete."
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
struct InfoParams {
    /// Name of the table to return schema for.
    ///
    /// In multi-table mode this argument is required; omitting it returns a
    /// TABLE_REQUIRED error. In legacy single-table mode (`MINI_APP_SCHEMA` +
    /// `MINI_APP_DB`) this may be omitted and the single configured table is
    /// used automatically.
    table: Option<String>,
}

/// Parameters for the `create` tool.
#[derive(Deserialize, JsonSchema)]
struct CreateParams {
    /// JSON object whose fields conform to schema.yaml.
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
}

/// Parameters for the `update` tool.
#[derive(Deserialize, JsonSchema)]
struct UpdateParams {
    /// Row id (UUID string).
    id: String,
    /// JSON object whose fields conform to schema.yaml.
    data: serde_json::Value,
    /// Name of the table to update in.
    ///
    /// In multi-table mode this argument is required; omitting it returns a
    /// TABLE_REQUIRED error. In legacy single-table mode (`MINI_APP_SCHEMA` +
    /// `MINI_APP_DB`) this may be omitted and the single configured table is
    /// used automatically.
    table: Option<String>,
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
    async fn tool_info(
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
                       If an unknown table name is specified, returns TABLE_NOT_FOUND (data.code=\"TABLE_NOT_FOUND\").",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tool_get(&self, Parameters(params): Parameters<GetParams>) -> Result<String, String> {
        let (store, _schema) = self
            .resolve_table(params.table.as_deref())
            .map_err(|e| e.to_string())?;
        let record = store.get(&params.id).await.map_err(|e| e.to_string())?;
        serde_json::to_string(&record).map_err(|e| e.to_string())
    }

    /// List rows with optional pagination.
    #[tool(
        name = "list",
        description = "List rows ordered by created_at descending. Supports limit (default 100, max 1000) and offset. \
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
    async fn tool_list(
        &self,
        Parameters(params): Parameters<ListParams>,
    ) -> Result<String, String> {
        let (store, _schema) = self
            .resolve_table(params.table.as_deref())
            .map_err(|e| e.to_string())?;
        let records = store
            .list(params.limit, params.offset)
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&records).map_err(|e| e.to_string())
    }

    /// Update an existing row by id.
    ///
    /// Crux constraint: the `data` argument is passed generically to
    /// `Store::update` — no field-specific access is performed here.
    #[tool(
        name = "update",
        description = "Replace the data of an existing row. The `data` argument must be a JSON object matching schema.yaml. \
                       In multi-table mode, `table` is required; omitting it returns a \
                       TABLE_REQUIRED error (data.code=\"TABLE_REQUIRED\"). \
                       In legacy single-table mode (`MINI_APP_SCHEMA`+`MINI_APP_DB`), `table` may be omitted. \
                       If an unknown table name is specified, returns TABLE_NOT_FOUND (data.code=\"TABLE_NOT_FOUND\").",
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
        let record = store
            .update(&params.id, params.data)
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
                       If an unknown table name is specified, returns TABLE_NOT_FOUND (data.code=\"TABLE_NOT_FOUND\").",
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
            fields: vec![
                FieldDef {
                    name: "title".to_string(),
                    ty: FieldType::String,
                    required: true,
                },
                FieldDef {
                    name: "state".to_string(),
                    ty: FieldType::String,
                    required: false,
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
            }))
            .await?;
        Ok(serde_json::from_str(&json).unwrap())
    }

    async fn do_update(
        server: &MiniAppMcpServer,
        id: &str,
        data: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let json = server
            .tool_update(Parameters(UpdateParams {
                id: id.to_string(),
                data,
                table: None,
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
    // T1: list_tools — all 6 tools present with correct annotations.
    // Access via server.tool_router.list_all() to avoid RequestContext.
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn list_tools_contains_all_six() {
        let (server, _tmp) = make_server().await;
        let tools = server.tool_router.list_all();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        for expected in &["info", "create", "get", "list", "update", "delete"] {
            assert!(
                names.contains(expected),
                "tool '{expected}' missing from list_tools"
            );
        }
        assert_eq!(tools.len(), 6, "expected exactly 6 tools");
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

        let updated = do_update(&server, id, serde_json::json!({ "title": "updated" }))
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
        // (with ?table=<name> query) + 3 docs URIs = 6 total.
        let (server, _tmp) = make_server().await;
        let resources = server.resource_list();
        assert_eq!(
            resources.len(),
            6,
            "expected exactly 6 resources in legacy mode, got: {:?}",
            resources.iter().map(|r| &r.uri).collect::<Vec<_>>()
        );
        // Docs URIs must be present without query string.
        let uris: Vec<&str> = resources.iter().map(|r| r.uri.as_str()).collect();
        for expected in &["docs://readme", "docs://tools", "docs://errors"] {
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
    async fn read_resource_readme_starts_with_heading() {
        let (server, _tmp) = make_server().await;
        let result = server
            .read_resource_impl("docs://readme")
            .await
            .expect("docs://readme must succeed");
        let text = match &result.contents[0] {
            rmcp::model::ResourceContents::TextResourceContents { text, .. } => text.clone(),
            _ => panic!("expected text contents"),
        };
        assert!(
            text.starts_with("# mini-app-mcp"),
            "README must start with '# mini-app-mcp', got: {:?}",
            &text[..text.len().min(40)]
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
            fields: vec![FieldDef {
                name: "name".to_string(),
                ty: FieldType::String,
                required: true,
            }],
            dump: None,
        };
        let schema_b = SchemaConfig {
            table: "table_b".to_string(),
            fields: vec![FieldDef {
                name: "value".to_string(),
                ty: FieldType::Number,
                required: false,
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
        MiniAppMcpServer::new_multi(Arc::new(registry))
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
        // 2 tables × 3 schema resources + 3 docs = 9
        assert_eq!(
            resources.len(),
            9,
            "expected 9 resources for 2 tables (2×3 schema + 3 docs), got: {:?}",
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
}
