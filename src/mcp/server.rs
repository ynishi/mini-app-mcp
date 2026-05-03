/// MCP server implementation for mini-app-mcp.
///
/// Exposes 6 tools (`info`, `create`, `get`, `list`, `update`, `delete`) and
/// 6 resources (`schema://yaml`, `schema://json`, `schema://json-schema`,
/// `docs://readme`, `docs://tools`, `docs://errors`) as MCP capabilities over
/// stdio transport.  No HTTP / REST / CLI-CRUD entry points are provided
/// (Crux "MCP-only entry point" constraint).
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        AnnotateAble, ListResourcesResult, PaginatedRequestParams, ProtocolVersion,
        RawResource, ReadResourceRequestParams, ReadResourceResult, ResourceContents,
        ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::stdio,
    ErrorData as McpError, RoleServer,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::config::Config;
use crate::mcp::resources as res;
use crate::schema::{self, SchemaConfig};
use crate::store::Store;

// =============================================================================
// Public entry point
// =============================================================================

/// Load config and schema, open the SQLite store, then serve over stdio.
///
/// # Concurrency
/// `Store` is wrapped in `Arc<Store>` and shared across clones of
/// `MiniAppMcpServer`. `Store` is `Send + Sync` because
/// `Arc<Mutex<rusqlite::Connection>>` satisfies both bounds.
/// Each incoming tool call spawns a `tokio::task::spawn_blocking` closure;
/// concurrent tool calls are serialized at the `Mutex<Connection>` level.
///
/// # Cancel Safety
/// Awaiting `service.waiting()` blocks until the transport closes. Dropping
/// this future does not abort in-flight `spawn_blocking` tasks; the
/// connection will remain open until those tasks complete.
///
/// # Panic
/// Does not panic. All initialization errors are returned as
/// `anyhow::Error`.
pub async fn run() -> anyhow::Result<()> {
    let config = Config::load()?;
    let schema = schema::load_from_path(&config.schema_path)?;
    let store = Store::open(&config.db_path, schema.clone()).await?;
    let server = MiniAppMcpServer::new(store, schema, config.schema_path);
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

// =============================================================================
// MCP Server
// =============================================================================

/// The MCP server for mini-app-mcp.
///
/// Holds a reference-counted [`Store`], the parsed [`SchemaConfig`], and
/// the path to `schema.yaml` so the `schema://yaml` resource can lazily read
/// the file on every request (keeping `schema.yaml` as the true source of
/// truth rather than a snapshot captured at startup).
///
/// The server is `Clone` because `rmcp` clones it per connection.
#[derive(Clone)]
pub struct MiniAppMcpServer {
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
    store: Arc<Store>,
    schema: Arc<SchemaConfig>,
    /// Path to `schema.yaml`, stored so `schema://yaml` can read it lazily.
    schema_path: Arc<PathBuf>,
}

impl MiniAppMcpServer {
    /// Create a new [`MiniAppMcpServer`].
    pub fn new(store: Store, schema: SchemaConfig, schema_path: PathBuf) -> Self {
        Self {
            tool_router: Self::tool_router(),
            store: Arc::new(store),
            schema: Arc::new(schema),
            schema_path: Arc::new(schema_path),
        }
    }
}

// =============================================================================
// ServerHandler
// =============================================================================

// =============================================================================
// Resource support — private helpers
// =============================================================================

/// URIs for all 6 advertised resources.
const URI_SCHEMA_YAML: &str = "schema://yaml";
const URI_SCHEMA_JSON: &str = "schema://json";
const URI_SCHEMA_JSON_SCHEMA: &str = "schema://json-schema";
const URI_DOCS_README: &str = "docs://readme";
const URI_DOCS_TOOLS: &str = "docs://tools";
const URI_DOCS_ERRORS: &str = "docs://errors";

impl MiniAppMcpServer {
    /// Build the static list of advertised resources.
    fn resource_list() -> Vec<rmcp::model::Resource> {
        vec![
            RawResource::new(URI_SCHEMA_YAML, "Schema YAML")
                .with_description("Raw schema.yaml file content (read at request time).")
                .with_mime_type("application/yaml")
                .no_annotation(),
            RawResource::new(URI_SCHEMA_JSON, "Schema JSON")
                .with_description(
                    "SchemaConfig serialised as JSON — same content as the `info` tool.",
                )
                .with_mime_type("application/json")
                .no_annotation(),
            RawResource::new(URI_SCHEMA_JSON_SCHEMA, "JSON Schema")
                .with_description(
                    "JSON Schema (draft-07) derived from SchemaConfig.fields — \
                     use this to validate `data` arguments before calling `create`/`update`.",
                )
                .with_mime_type("application/schema+json")
                .no_annotation(),
            RawResource::new(URI_DOCS_README, "README")
                .with_description("README.md — embedded in the binary at compile time.")
                .with_mime_type("text/markdown")
                .no_annotation(),
            RawResource::new(URI_DOCS_TOOLS, "Tools Reference")
                .with_description(
                    "Cheat sheet listing all 6 tools with descriptions and input shapes.",
                )
                .with_mime_type("text/markdown")
                .no_annotation(),
            RawResource::new(URI_DOCS_ERRORS, "Error Code Reference")
                .with_description("Reference table of all error codes returned by this server.")
                .with_mime_type("text/markdown")
                .no_annotation(),
        ]
    }

    /// Inner implementation of `read_resource` — tested directly to avoid
    /// `RequestContext` construction issues in tests (rmcp 1.5 makes
    /// `RequestContext` `#[non_exhaustive]` so it cannot be built in external
    /// crates).
    async fn read_resource_impl(&self, uri: &str) -> Result<ReadResourceResult, McpError> {
        let contents = match uri {
            URI_SCHEMA_YAML => {
                let text = std::fs::read_to_string(self.schema_path.as_ref()).map_err(|e| {
                    McpError::internal_error(format!("failed to read schema.yaml: {e}"), None)
                })?;
                ResourceContents::text(text, uri).with_mime_type("application/yaml")
            }
            URI_SCHEMA_JSON => {
                let text = serde_json::to_string_pretty(&*self.schema)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                ResourceContents::text(text, uri).with_mime_type("application/json")
            }
            URI_SCHEMA_JSON_SCHEMA => {
                let js = res::derive_json_schema(&self.schema);
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
            "Agent-First CRUD store for a single table defined in schema.yaml. \
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
             - `info`: Return the parsed schema (table name + field definitions).\n\
             - `create`: Insert a new row. The `data` argument must be a JSON object \
             whose fields conform to schema.yaml.\n\
             - `get`: Fetch a single row by id.\n\
             - `list`: List rows with optional limit/offset pagination.\n\
             - `update`: Replace the data of an existing row by id.\n\
             - `delete`: Delete a row by id."
                .to_string(),
        );
        info
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult::with_all_items(Self::resource_list()))
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

/// Parameters for the `create` tool.
#[derive(Deserialize, JsonSchema)]
struct CreateParams {
    /// JSON object whose fields conform to schema.yaml.
    data: serde_json::Value,
}

/// Parameters for the `get` tool.
#[derive(Deserialize, JsonSchema)]
struct GetParams {
    /// Row id (UUID string).
    id: String,
}

/// Parameters for the `list` tool.
#[derive(Deserialize, JsonSchema)]
struct ListParams {
    /// Maximum rows to return (default 100, max 1000).
    limit: Option<u32>,
    /// Number of rows to skip from the start.
    offset: Option<u32>,
}

/// Parameters for the `update` tool.
#[derive(Deserialize, JsonSchema)]
struct UpdateParams {
    /// Row id (UUID string).
    id: String,
    /// JSON object whose fields conform to schema.yaml.
    data: serde_json::Value,
}

/// Parameters for the `delete` tool.
#[derive(Deserialize, JsonSchema)]
struct DeleteParams {
    /// Row id (UUID string).
    id: String,
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
        description = "Return the parsed schema: table name and field definitions loaded from schema.yaml.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tool_info(&self) -> Result<String, String> {
        serde_json::to_string_pretty(&*self.schema).map_err(|e| e.to_string())
    }

    /// Create a new row.
    ///
    /// Crux constraint: the `data` argument is a generic JSON object passed
    /// directly to `Store::create` — no field-specific access is performed here.
    #[tool(
        name = "create",
        description = "Create a new row. The `data` argument must be a JSON object matching schema.yaml.",
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
        let record = self
            .store
            .create(params.data)
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&record).map_err(|e| e.to_string())
    }

    /// Get a single row by id.
    #[tool(
        name = "get",
        description = "Fetch a single row by its UUID id.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn tool_get(&self, Parameters(params): Parameters<GetParams>) -> Result<String, String> {
        let record = self
            .store
            .get(&params.id)
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&record).map_err(|e| e.to_string())
    }

    /// List rows with optional pagination.
    #[tool(
        name = "list",
        description = "List rows ordered by created_at descending. Supports limit (default 100, max 1000) and offset.",
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
        let records = self
            .store
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
        description = "Replace the data of an existing row. The `data` argument must be a JSON object matching schema.yaml.",
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
        let record = self
            .store
            .update(&params.id, params.data)
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&record).map_err(|e| e.to_string())
    }

    /// Delete a row by id.
    #[tool(
        name = "delete",
        description = "Delete the row with the given id. Returns an error if the row does not exist.",
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
        self.store
            .delete(&params.id)
            .await
            .map_err(|e| e.to_string())?;
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
        (MiniAppMcpServer::new(store, schema, schema_path), tmp)
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
            .tool_create(Parameters(CreateParams { data }))
            .await?;
        Ok(serde_json::from_str(&json).unwrap())
    }

    async fn do_get(server: &MiniAppMcpServer, id: &str) -> Result<serde_json::Value, String> {
        let json = server
            .tool_get(Parameters(GetParams { id: id.to_string() }))
            .await?;
        Ok(serde_json::from_str(&json).unwrap())
    }

    async fn do_list(
        server: &MiniAppMcpServer,
        limit: Option<u32>,
        offset: Option<u32>,
    ) -> Result<serde_json::Value, String> {
        let json = server
            .tool_list(Parameters(ListParams { limit, offset }))
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
            }))
            .await?;
        Ok(serde_json::from_str(&json).unwrap())
    }

    async fn do_delete(server: &MiniAppMcpServer, id: &str) -> Result<serde_json::Value, String> {
        let json = server
            .tool_delete(Parameters(DeleteParams { id: id.to_string() }))
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
    // T2: info tool
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn info_tool_returns_schema_json() {
        let (server, _tmp) = make_server().await;
        let json = server.tool_info().await.expect("info must succeed");
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
    // T8: Resources
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn list_resources_returns_six() {
        // MiniAppMcpServer::resource_list() is the pure static list; testing
        // it directly avoids the non-constructible RequestContext.
        let resources = MiniAppMcpServer::resource_list();
        assert_eq!(
            resources.len(),
            6,
            "expected exactly 6 resources, got: {:?}",
            resources.iter().map(|r| &r.uri).collect::<Vec<_>>()
        );
        let uris: Vec<&str> = resources.iter().map(|r| r.uri.as_str()).collect();
        for expected in &[
            "schema://yaml",
            "schema://json",
            "schema://json-schema",
            "docs://readme",
            "docs://tools",
            "docs://errors",
        ] {
            assert!(uris.contains(expected), "URI '{expected}' missing from list");
        }
    }

    #[tokio::test]
    async fn read_resource_schema_json_returns_schema() {
        let (server, _tmp) = make_server().await;
        let result = server
            .read_resource_impl("schema://json")
            .await
            .expect("schema://json must succeed");
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
    async fn read_resource_json_schema_has_required_array() {
        let (server, _tmp) = make_server().await;
        let result = server
            .read_resource_impl("schema://json-schema")
            .await
            .expect("schema://json-schema must succeed");
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
}
