/// MCP server implementation for mini-app-mcp.
///
/// Exposes 6 tools (`info`, `create`, `get`, `list`, `update`, `delete`) as
/// MCP tools over stdio transport.  No HTTP / REST / CLI-CRUD entry points are
/// provided (Crux "MCP-only entry point" constraint).
use std::sync::Arc;

use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolRequestParams, CallToolResult, Content, Implementation, ListToolsResult,
        PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    service::{RequestContext, RoleServer},
    tool, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::config::Config;
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
    let server = MiniAppMcpServer::new(store, schema);
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

// =============================================================================
// MCP Server
// =============================================================================

/// The MCP server for mini-app-mcp.
///
/// Holds a reference-counted [`Store`] and the parsed [`SchemaConfig`].
/// The server is `Clone` because `rmcp` clones it per connection.
#[derive(Clone)]
pub struct MiniAppMcpServer {
    tool_router: ToolRouter<Self>,
    store: Arc<Store>,
    schema: Arc<SchemaConfig>,
}

impl MiniAppMcpServer {
    /// Create a new [`MiniAppMcpServer`].
    pub fn new(store: Store, schema: SchemaConfig) -> Self {
        Self {
            tool_router: Self::tool_router(),
            store: Arc::new(store),
            schema: Arc::new(schema),
        }
    }
}

// =============================================================================
// ServerHandler
// =============================================================================

impl ServerHandler for MiniAppMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2025_03_26,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "mini-app-mcp".to_string(),
                title: Some("Mini App MCP — Agent-First CRUD Store".to_string()),
                description: Some(
                    "Agent-First CRUD store for a single table defined in schema.yaml. \
                     6 tools: info, create, get, list, update, delete."
                        .to_string(),
                ),
                version: env!("CARGO_PKG_VERSION").to_string(),
                icons: None,
                website_url: None,
            },
            instructions: Some(
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
            ),
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: self.tool_router.list_all(),
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let tool_ctx = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        self.tool_router.call(tool_ctx).await
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
    async fn tool_info(&self) -> Result<CallToolResult, McpError> {
        let json = serde_json::to_string_pretty(&*self.schema)
            .map_err(|e| crate::error::MiniAppError::Schema(e.to_string()))?;
        let value = serde_json::to_value(&*self.schema).unwrap_or(serde_json::Value::Null);
        Ok(CallToolResult {
            content: vec![Content::text(json)],
            structured_content: Some(value),
            is_error: Some(false),
            meta: None,
        })
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
    ) -> Result<CallToolResult, McpError> {
        let record = self.store.create(params.data).await?;
        let json = serde_json::to_string(&record)
            .map_err(|e| crate::error::MiniAppError::Schema(e.to_string()))?;
        let value = serde_json::to_value(&record).unwrap_or(serde_json::Value::Null);
        Ok(CallToolResult {
            content: vec![Content::text(json)],
            structured_content: Some(value),
            is_error: Some(false),
            meta: None,
        })
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
    async fn tool_get(
        &self,
        Parameters(params): Parameters<GetParams>,
    ) -> Result<CallToolResult, McpError> {
        let record = self.store.get(&params.id).await?;
        let json = serde_json::to_string(&record)
            .map_err(|e| crate::error::MiniAppError::Schema(e.to_string()))?;
        let value = serde_json::to_value(&record).unwrap_or(serde_json::Value::Null);
        Ok(CallToolResult {
            content: vec![Content::text(json)],
            structured_content: Some(value),
            is_error: Some(false),
            meta: None,
        })
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
    ) -> Result<CallToolResult, McpError> {
        let records = self.store.list(params.limit, params.offset).await?;
        let json = serde_json::to_string(&records)
            .map_err(|e| crate::error::MiniAppError::Schema(e.to_string()))?;
        // Wrap array in object: rmcp / MCP spec requires `structuredContent` to be a record (object), not array.
        let value = serde_json::json!({ "items": records });
        Ok(CallToolResult {
            content: vec![Content::text(json)],
            structured_content: Some(value),
            is_error: Some(false),
            meta: None,
        })
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
    ) -> Result<CallToolResult, McpError> {
        let record = self.store.update(&params.id, params.data).await?;
        let json = serde_json::to_string(&record)
            .map_err(|e| crate::error::MiniAppError::Schema(e.to_string()))?;
        let value = serde_json::to_value(&record).unwrap_or(serde_json::Value::Null);
        Ok(CallToolResult {
            content: vec![Content::text(json)],
            structured_content: Some(value),
            is_error: Some(false),
            meta: None,
        })
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
    ) -> Result<CallToolResult, McpError> {
        self.store.delete(&params.id).await?;
        let json = serde_json::json!({ "deleted": params.id });
        Ok(CallToolResult {
            content: vec![Content::text(json.to_string())],
            structured_content: Some(json),
            is_error: Some(false),
            meta: None,
        })
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rmcp::model::{CallToolRequestParams, Extensions, Meta, NumberOrString};
    use rmcp::service::{RequestContext, RoleServer};

    use super::*;
    use crate::schema::{FieldDef, FieldType};

    // ---------------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------------

    async fn make_server() -> MiniAppMcpServer {
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
        MiniAppMcpServer::new(store, schema)
    }

    /// Build a minimal `RequestContext` for unit tests.
    ///
    /// Our tool implementations never access `context.peer` or `context.ct`, so
    /// the `Peer` and `CancellationToken` inside the context are never used.
    /// We construct a real `RunningService` (using a dead-end channel transport)
    /// solely to extract a valid `Peer<RoleServer>`.
    async fn fake_context() -> RequestContext<RoleServer> {
        use futures::channel::mpsc;
        use rmcp::model::JsonRpcMessage;
        use rmcp::model::{ClientNotification, ClientRequest, ClientResult};
        use rmcp::model::{ServerNotification, ServerRequest, ServerResult};

        // Types for the two directions of the transport.
        type ToClient = JsonRpcMessage<ServerRequest, ServerResult, ServerNotification>;
        type FromClient = JsonRpcMessage<ClientRequest, ClientResult, ClientNotification>;

        // A sink that discards everything sent to it (server → client direction).
        let (tx, _rx) = mpsc::channel::<ToClient>(1);
        // A stream that never produces anything (client → server direction).
        let (_tx2, rx2) = mpsc::channel::<FromClient>(1);

        let server = make_server().await;
        let running =
            rmcp::service::serve_directly::<RoleServer, _, _, _, _>(server, (tx, rx2), None);
        let peer = running.peer().clone();

        RequestContext {
            ct: tokio_util::sync::CancellationToken::new(),
            id: NumberOrString::Number(0),
            meta: Meta::new(),
            extensions: Extensions::default(),
            peer,
        }
    }

    // Call a tool by name with the given JSON arguments.
    async fn call(
        server: &MiniAppMcpServer,
        name: &str,
        args: serde_json::Value,
    ) -> CallToolResult {
        let params = CallToolRequestParams {
            name: name.to_owned().into(),
            arguments: args.as_object().cloned(),
            meta: None,
            task: None,
        };
        server
            .call_tool(params, fake_context().await)
            .await
            .expect("call_tool should not return McpError at transport level")
    }

    // ---------------------------------------------------------------------------
    // T1: list_tools — all 6 tools present with correct annotations
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn list_tools_contains_all_six() {
        let server = make_server().await;
        let result = server
            .list_tools(None, fake_context().await)
            .await
            .expect("list_tools must succeed");
        let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_ref()).collect();
        for expected in &["info", "create", "get", "list", "update", "delete"] {
            assert!(
                names.contains(expected),
                "tool '{expected}' missing from list_tools"
            );
        }
        assert_eq!(result.tools.len(), 6, "expected exactly 6 tools");
    }

    #[tokio::test]
    async fn tool_annotations_delete_is_destructive() {
        let server = make_server().await;
        let result = server.list_tools(None, fake_context().await).await.unwrap();
        let delete_tool = result
            .tools
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
        let server = make_server().await;
        let result = server.list_tools(None, fake_context().await).await.unwrap();
        let create_tool = result
            .tools
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
        let server = make_server().await;
        let result = server.list_tools(None, fake_context().await).await.unwrap();
        for name in &["info", "get", "list"] {
            let tool = result
                .tools
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
        let server = make_server().await;
        let result = call(&server, "info", serde_json::json!({})).await;
        assert_eq!(result.is_error, Some(false));
        assert!(!result.content.is_empty());

        // Parse the returned JSON and verify required fields.
        let text = match &result.content[0] {
            c if c.as_text().is_some() => c.as_text().unwrap().text.clone(),
            other => panic!("expected text content, got: {other:?}"),
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&text).expect("info must return valid JSON");
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
        let server = make_server().await;

        let create_result = call(
            &server,
            "create",
            serde_json::json!({ "data": { "title": "hello", "state": "open" } }),
        )
        .await;
        assert_eq!(create_result.is_error, Some(false));

        let text = match &create_result.content[0] {
            c if c.as_text().is_some() => c.as_text().unwrap().text.clone(),
            other => panic!("expected text content, got: {other:?}"),
        };
        let created: serde_json::Value = serde_json::from_str(&text).unwrap();
        let id = created["id"].as_str().expect("id must be a string");

        let get_result = call(&server, "get", serde_json::json!({ "id": id })).await;
        assert_eq!(get_result.is_error, Some(false));
        let get_text = match &get_result.content[0] {
            c if c.as_text().is_some() => c.as_text().unwrap().text.clone(),
            other => panic!("expected text content, got: {other:?}"),
        };
        let fetched: serde_json::Value = serde_json::from_str(&get_text).unwrap();
        assert_eq!(fetched["id"], id);
        assert_eq!(fetched["data"]["title"], "hello");
    }

    // ---------------------------------------------------------------------------
    // T4: list tool
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn list_tool_returns_array() {
        let server = make_server().await;

        // Insert two rows first.
        call(
            &server,
            "create",
            serde_json::json!({ "data": { "title": "row1" } }),
        )
        .await;
        call(
            &server,
            "create",
            serde_json::json!({ "data": { "title": "row2" } }),
        )
        .await;

        let list_result = call(&server, "list", serde_json::json!({})).await;
        assert_eq!(list_result.is_error, Some(false));
        let text = match &list_result.content[0] {
            c if c.as_text().is_some() => c.as_text().unwrap().text.clone(),
            other => panic!("expected text content, got: {other:?}"),
        };
        let rows: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(rows.is_array(), "list must return a JSON array");
        assert_eq!(rows.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn list_tool_with_limit_and_offset() {
        let server = make_server().await;
        for i in 0..5 {
            call(
                &server,
                "create",
                serde_json::json!({ "data": { "title": format!("item-{i}") } }),
            )
            .await;
        }
        let result = call(
            &server,
            "list",
            serde_json::json!({ "limit": 2, "offset": 1 }),
        )
        .await;
        assert_eq!(result.is_error, Some(false));
        let text = match &result.content[0] {
            c if c.as_text().is_some() => c.as_text().unwrap().text.clone(),
            other => panic!("expected text: {other:?}"),
        };
        let rows: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 2);
    }

    // ---------------------------------------------------------------------------
    // T5: update tool
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn update_tool_success() {
        let server = make_server().await;
        let create_result = call(
            &server,
            "create",
            serde_json::json!({ "data": { "title": "original" } }),
        )
        .await;
        let text = match &create_result.content[0] {
            c if c.as_text().is_some() => c.as_text().unwrap().text.clone(),
            other => panic!("{other:?}"),
        };
        let created: serde_json::Value = serde_json::from_str(&text).unwrap();
        let id = created["id"].as_str().unwrap();

        let update_result = call(
            &server,
            "update",
            serde_json::json!({ "id": id, "data": { "title": "updated" } }),
        )
        .await;
        assert_eq!(update_result.is_error, Some(false));
        let update_text = match &update_result.content[0] {
            c if c.as_text().is_some() => c.as_text().unwrap().text.clone(),
            other => panic!("{other:?}"),
        };
        let updated: serde_json::Value = serde_json::from_str(&update_text).unwrap();
        assert_eq!(updated["data"]["title"], "updated");
    }

    // ---------------------------------------------------------------------------
    // T6: delete tool
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn delete_tool_success() {
        let server = make_server().await;
        let create_result = call(
            &server,
            "create",
            serde_json::json!({ "data": { "title": "to-delete" } }),
        )
        .await;
        let text = match &create_result.content[0] {
            c if c.as_text().is_some() => c.as_text().unwrap().text.clone(),
            other => panic!("{other:?}"),
        };
        let created: serde_json::Value = serde_json::from_str(&text).unwrap();
        let id = created["id"].as_str().unwrap();

        let delete_result = call(&server, "delete", serde_json::json!({ "id": id })).await;
        assert_eq!(delete_result.is_error, Some(false));
        let delete_text = match &delete_result.content[0] {
            c if c.as_text().is_some() => c.as_text().unwrap().text.clone(),
            other => panic!("{other:?}"),
        };
        let resp: serde_json::Value = serde_json::from_str(&delete_text).unwrap();
        assert_eq!(resp["deleted"], id);
    }

    // ---------------------------------------------------------------------------
    // T7: error paths — structured JSON errors (Crux #3)
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn create_missing_required_field_returns_is_error_true() {
        let server = make_server().await;
        // `title` is required; passing empty object must fail with is_error=true
        // at the McpError level (From<MiniAppError> conversion).
        //
        // When the tool method returns Err(McpError), rmcp converts it to a
        // CallToolResult with is_error=true.  We call call_tool directly to
        // observe that path.
        let args = serde_json::json!({ "data": {} });
        let params = CallToolRequestParams {
            name: "create".into(),
            arguments: args.as_object().cloned(),
            meta: None,
            task: None,
        };
        // call_tool may return Ok(CallToolResult { is_error: Some(true) }) or
        // Err(McpError).  Both represent tool-level errors.
        match server.call_tool(params, fake_context().await).await {
            Ok(result) => {
                // rmcp wraps McpError as is_error=true content
                assert_eq!(
                    result.is_error,
                    Some(true),
                    "validation failure must produce is_error=true"
                );
            }
            Err(mcp_err) => {
                // The error's data field must be Some and contain a "code" key.
                let data = mcp_err
                    .data
                    .expect("McpError must carry structured data (Crux #3)");
                assert!(
                    data.get("code").is_some(),
                    "structured error must contain 'code' field"
                );
                assert_eq!(data["code"], "VALIDATION_ERROR");
            }
        }
    }

    #[tokio::test]
    async fn get_not_found_returns_error() {
        let server = make_server().await;
        let args = serde_json::json!({ "id": "nonexistent-id" });
        let params = CallToolRequestParams {
            name: "get".into(),
            arguments: args.as_object().cloned(),
            meta: None,
            task: None,
        };
        match server.call_tool(params, fake_context().await).await {
            Ok(result) => {
                assert_eq!(result.is_error, Some(true));
            }
            Err(mcp_err) => {
                let data = mcp_err.data.expect("McpError must carry structured data");
                assert_eq!(data["code"], "NOT_FOUND");
            }
        }
    }

    #[tokio::test]
    async fn delete_not_found_returns_error() {
        let server = make_server().await;
        let args = serde_json::json!({ "id": "nonexistent-id" });
        let params = CallToolRequestParams {
            name: "delete".into(),
            arguments: args.as_object().cloned(),
            meta: None,
            task: None,
        };
        match server.call_tool(params, fake_context().await).await {
            Ok(result) => {
                assert_eq!(result.is_error, Some(true));
            }
            Err(mcp_err) => {
                let data = mcp_err.data.expect("McpError must carry structured data");
                assert_eq!(data["code"], "NOT_FOUND");
            }
        }
    }

    #[tokio::test]
    async fn update_not_found_returns_error() {
        let server = make_server().await;
        let args = serde_json::json!({ "id": "nonexistent-id", "data": { "title": "x" } });
        let params = CallToolRequestParams {
            name: "update".into(),
            arguments: args.as_object().cloned(),
            meta: None,
            task: None,
        };
        match server.call_tool(params, fake_context().await).await {
            Ok(result) => {
                assert_eq!(result.is_error, Some(true));
            }
            Err(mcp_err) => {
                let data = mcp_err.data.expect("McpError must carry structured data");
                assert_eq!(data["code"], "NOT_FOUND");
            }
        }
    }
}
