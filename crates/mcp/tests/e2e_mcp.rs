//! End-to-end tests for the MCP server's stdio JSON-RPC surface.
//!
//! These tests spawn the real `mini-app-mcp --mcp` binary and drive it
//! through stdio, exercising the same path that Claude / any MCP client
//! takes. They cover the layer the in-process unit tests bypass: the
//! JSON Schema advertised in `tools/list` and the `tools/call` payload
//! shape.
//!
//! Origin: 2026-05-07 fix commit `83e968e` (untyped `data` schema bug).
//! Issue: task-mcp `1778128082-25790`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_mini-app-mcp");

const EMO_SCHEMA: &str = "table: emo
fields:
  - name: text
    type: string
    required: true
  - name: tags
    type: array
    required: false
";

/// JSON-RPC client speaking to a spawned `mini-app-mcp --mcp` over stdio.
///
/// The server emits one JSON document per line on stdout. Notifications
/// carry no `id`; method calls echo the request `id`. We dispatch on
/// `id` so that out-of-order notifications cannot desync the test.
struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpClient {
    fn spawn(user_dir: &Path, project_dir: &Path) -> Self {
        let mut child = Command::new(BIN)
            .arg("--mcp")
            .env("MINI_APP_USER_DIR", user_dir)
            .env("MINI_APP_PROJECT_DIR", project_dir)
            .env_remove("MINI_APP_SCHEMA")
            .env_remove("MINI_APP_DB")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn mini-app-mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut c = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        c.handshake();
        c
    }

    fn send_line(&mut self, msg: &Value) {
        let line = serde_json::to_string(msg).unwrap();
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.write_all(b"\n").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Read response lines until one matches `wanted_id`. Notifications
    /// (no `id`) are silently consumed.
    fn recv_for(&mut self, wanted_id: u64) -> Value {
        loop {
            let mut line = String::new();
            let n = self
                .stdout
                .read_line(&mut line)
                .expect("read mcp stdout line");
            assert!(n > 0, "mcp server closed stdout before response");
            let v: Value = serde_json::from_str(line.trim()).unwrap_or_else(|e| {
                panic!("mcp emitted non-JSON line: {line:?} ({e})");
            });
            match v.get("id").and_then(Value::as_u64) {
                Some(id) if id == wanted_id => return v,
                _ => continue,
            }
        }
    }

    fn handshake(&mut self) {
        let id = self.next_id();
        self.send_line(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "mini-app-e2e", "version": "0"},
            }
        }));
        let resp = self.recv_for(id);
        assert!(resp.get("error").is_none(), "initialize failed: {resp:?}");
        self.send_line(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }));
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id();
        self.send_line(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        let resp = self.recv_for(id);
        assert!(resp.get("error").is_none(), "rpc {method} error: {resp:?}");
        resp.get("result").cloned().expect("result missing")
    }

    /// Invoke an MCP tool and decode its single text content payload as
    /// JSON. mini-app-mcp returns `{"content":[{"type":"text","text":"<json>"}]}`.
    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let result = self.rpc("tools/call", json!({"name": name, "arguments": arguments}));
        assert_eq!(
            result.get("isError").and_then(Value::as_bool),
            Some(false),
            "tool {name} returned error: {result:?}"
        );
        let text = result
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("tool {name} produced no text content: {result:?}"));
        serde_json::from_str(text)
            .unwrap_or_else(|e| panic!("tool {name} text was not JSON: {text:?} ({e})"))
    }

    fn list_tools(&mut self) -> Vec<Value> {
        let result = self.rpc("tools/list", json!({}));
        result
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .expect("tools array")
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Build a tempdir layout that mounts a single `emo` table in user-scope.
struct Layout {
    _tmp: TempDir,
    user_dir: PathBuf,
    project_dir: PathBuf,
}

fn make_layout() -> Layout {
    let tmp = TempDir::new().expect("tempdir");
    let user_dir = tmp.path().join("user");
    let project_dir = tmp.path().join("project");
    let emo_dir = user_dir.join("emo");
    std::fs::create_dir_all(&emo_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(emo_dir.join("schema.yaml"), EMO_SCHEMA).unwrap();
    Layout {
        _tmp: tmp,
        user_dir,
        project_dir,
    }
}

// ---------------------------------------------------------------------------
// Schema regression: tools/list must advertise `data` as an object so that
// the Anthropic tool-use serializer transmits an actual JSON object rather
// than stringifying it. Origin: fix commit 83e968e.
// ---------------------------------------------------------------------------

#[test]
fn tools_list_advertises_typed_data_object_for_create_and_update() {
    let layout = make_layout();
    let mut client = McpClient::spawn(&layout.user_dir, &layout.project_dir);
    let tools = client.list_tools();

    for name in ["create", "update"] {
        let tool = tools
            .iter()
            .find(|t| t.get("name").and_then(Value::as_str) == Some(name))
            .unwrap_or_else(|| panic!("tool {name} missing from tools/list"));
        let data_schema = tool
            .pointer("/inputSchema/properties/data")
            .unwrap_or_else(|| panic!("{name}.inputSchema.properties.data missing"));
        assert_eq!(
            data_schema.get("type").and_then(Value::as_str),
            Some("object"),
            "{name}.inputSchema.properties.data.type must be \"object\"; got {data_schema}"
        );
    }
}

#[test]
fn tools_list_contains_all_advertised_tools() {
    let layout = make_layout();
    let mut client = McpClient::spawn(&layout.user_dir, &layout.project_dir);
    let tools = client.list_tools();
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t.get("name").and_then(Value::as_str).unwrap())
        .collect();
    for expected in [
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
        "alias_create",
        "alias_list",
        "alias_run",
        "alias_delete",
    ] {
        assert!(names.contains(&expected), "{expected} missing: {names:?}");
    }
}

// ---------------------------------------------------------------------------
// CRUD round-trip via JSON-RPC: exercises the exact wire format Claude
// uses, including `arguments.data` arriving as a JSON object.
// ---------------------------------------------------------------------------

#[test]
fn create_get_update_list_delete_round_trip_via_stdio() {
    let layout = make_layout();
    let mut client = McpClient::spawn(&layout.user_dir, &layout.project_dir);

    let created = client.call_tool(
        "create",
        json!({
            "table": "emo",
            "data": {"text": "e2e hello", "tags": ["e2e", "smoke"]},
        }),
    );
    let id = created
        .get("id")
        .and_then(Value::as_str)
        .expect("created.id")
        .to_string();
    assert_eq!(
        created.pointer("/data/text").and_then(Value::as_str),
        Some("e2e hello")
    );

    let fetched = client.call_tool("get", json!({"table": "emo", "id": id}));
    assert_eq!(fetched.get("id").and_then(Value::as_str), Some(id.as_str()));
    assert_eq!(
        fetched.pointer("/data/text").and_then(Value::as_str),
        Some("e2e hello")
    );

    let updated = client.call_tool(
        "update",
        json!({
            "table": "emo",
            "id": id,
            "data": {"text": "e2e updated", "tags": ["e2e", "updated"]},
        }),
    );
    assert_eq!(
        updated.pointer("/data/text").and_then(Value::as_str),
        Some("e2e updated")
    );

    let listed = client.call_tool("list", json!({"table": "emo", "limit": 10}));
    let arr = listed.as_array().expect("list returns array");
    assert!(
        arr.iter()
            .any(|row| row.get("id").and_then(Value::as_str) == Some(id.as_str())),
        "list did not contain newly-created id"
    );

    let deleted = client.call_tool("delete", json!({"table": "emo", "id": id}));
    assert_eq!(
        deleted.get("deleted").and_then(Value::as_str),
        Some(id.as_str())
    );
}

// ---------------------------------------------------------------------------
// Negative path: stringifying `data` (the pre-fix client behaviour) must
// still be rejected by the server with the documented message. Locks in
// the contract that the schema-side fix is the only correct solution.
// ---------------------------------------------------------------------------

#[test]
fn create_with_stringified_data_is_rejected() {
    let layout = make_layout();
    let mut client = McpClient::spawn(&layout.user_dir, &layout.project_dir);

    let id = client.next_id();
    client.send_line(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": "create",
            "arguments": {
                "table": "emo",
                "data": "{\"text\":\"stringified\"}",
            }
        }
    }));
    let resp = client.recv_for(id);
    // Either a JSON-RPC error (schema validation by rmcp) or a tool-level
    // error (server rejects non-object). Both are acceptable; what we
    // forbid is a successful insert.
    let is_tool_error = resp
        .pointer("/result/isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let has_rpc_error = resp.get("error").is_some();
    assert!(
        is_tool_error || has_rpc_error,
        "stringified data should be rejected, got: {resp}"
    );
}

// ---------------------------------------------------------------------------
// Alias CRUD round-trip
// ---------------------------------------------------------------------------

/// Exercises alias_create → alias_list → alias_run → alias_delete in order,
/// then verifies that alias_run after deletion returns ALIAS_NOT_FOUND.
#[test]
fn alias_crud_round_trip() {
    let layout = make_layout();
    let mut client = McpClient::spawn(&layout.user_dir, &layout.project_dir);

    // Seed a row so alias_run has something to return.
    client.call_tool(
        "create",
        json!({
            "table": "emo",
            "data": {"text": "alias seed row"},
        }),
    );

    // 1. alias_create — register an alias filtering on the text field.
    // Uses the tagged-enum ListFilter format: {"type": "like", "field": "text", "pattern": "%alias seed%"}.
    let created = client.call_tool(
        "alias_create",
        json!({
            "table": "emo",
            "name": "alias_test",
            "filter": {"type": "like", "field": "text", "pattern": "%alias seed%"},
            "limit": 10,
            "description": "test alias"
        }),
    );
    assert_eq!(
        created.get("created").and_then(Value::as_str),
        Some("alias_test"),
        "alias_create should return created name, got: {created:?}"
    );

    // 2. alias_list — the alias we just created must appear in the list.
    let listed: Vec<Value> =
        serde_json::from_value(client.call_tool("alias_list", json!({"table": "emo"})))
            .expect("alias_list should return a JSON array");
    let names: Vec<&str> = listed
        .iter()
        .filter_map(|a| a.get("name").and_then(Value::as_str))
        .collect();
    assert!(
        names.contains(&"alias_test"),
        "alias_test not found in alias_list, got: {names:?}"
    );

    // 3. alias_run — should return rows matching the stored filter.
    let rows: Vec<Value> = serde_json::from_value(
        client.call_tool("alias_run", json!({"table": "emo", "name": "alias_test"})),
    )
    .expect("alias_run should return a JSON array");
    assert!(
        !rows.is_empty(),
        "alias_run should return at least one row for filter matching 'alias seed' text"
    );

    // 4. alias_delete — remove the alias.
    let deleted = client.call_tool(
        "alias_delete",
        json!({"table": "emo", "name": "alias_test"}),
    );
    assert_eq!(
        deleted.get("deleted").and_then(Value::as_str),
        Some("alias_test"),
        "alias_delete should return deleted name, got: {deleted:?}"
    );

    // 5. alias_run after deletion must return a tool error (ALIAS_NOT_FOUND).
    // The error text contains "alias not found" from the Display impl.
    let id = client.next_id();
    client.send_line(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": "alias_run",
            "arguments": {"table": "emo", "name": "alias_test"}
        }
    }));
    let resp = client.recv_for(id);
    let is_tool_error = resp
        .pointer("/result/isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    assert!(
        is_tool_error,
        "alias_run after delete should return tool error, got: {resp}"
    );
    // Verify the error message contains "alias not found" (from MiniAppError::AliasNotFound Display).
    let text = resp
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        text.contains("alias not found"),
        "expected 'alias not found' in error text, got: {text:?}"
    );
}

/// Verifies that duplicate alias_create returns ALIAS_ALREADY_EXISTS.
#[test]
fn alias_create_duplicate_returns_already_exists() {
    let layout = make_layout();
    let mut client = McpClient::spawn(&layout.user_dir, &layout.project_dir);

    // First create should succeed. Filter on text field using Eq.
    client.call_tool(
        "alias_create",
        json!({
            "table": "emo",
            "name": "dup_alias",
            "filter": {"type": "eq", "field": "text", "value": "dup text"}
        }),
    );

    // Second create with the same name must return ALIAS_ALREADY_EXISTS.
    let id = client.next_id();
    client.send_line(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": "alias_create",
            "arguments": {
                "table": "emo",
                "name": "dup_alias",
                "filter": {"type": "eq", "field": "text", "value": "dup text"}
            }
        }
    }));
    let resp = client.recv_for(id);
    let is_tool_error = resp
        .pointer("/result/isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    assert!(
        is_tool_error,
        "duplicate alias_create should return tool error, got: {resp}"
    );
    // Verify the error message contains "alias already exists" (from MiniAppError::AliasAlreadyExists Display).
    let text = resp
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        text.contains("alias already exists"),
        "expected 'alias already exists' in error text, got: {text:?}"
    );
}

/// Verifies that alias_run with a runtime limit overrides the stored default_limit.
/// Also verifies runtime offset is passed through.
///
/// Crux: alias_run must accept runtime limit/offset that override stored defaults
/// and pass the resolved values through to Store::list rather than replaying
/// the stored parameters verbatim.
#[test]
fn alias_run_limit_override() {
    let layout = make_layout();
    let mut client = McpClient::spawn(&layout.user_dir, &layout.project_dir);

    // Seed 3 rows with a distinctive text prefix.
    for i in 0..3 {
        client.call_tool(
            "create",
            json!({
                "table": "emo",
                "data": {"text": format!("limit_override_row_{i}")},
            }),
        );
    }

    // Create alias with stored default_limit = 1, filter matches the 3 seeded rows.
    client.call_tool(
        "alias_create",
        json!({
            "table": "emo",
            "name": "limit_test_alias",
            "filter": {"type": "like", "field": "text", "pattern": "limit_override_row_%"},
            "limit": 1
        }),
    );

    // alias_run without runtime limit → uses stored default_limit = 1 → 1 row.
    let rows_default: Vec<Value> = serde_json::from_value(client.call_tool(
        "alias_run",
        json!({"table": "emo", "name": "limit_test_alias"}),
    ))
    .expect("alias_run (default limit) should return a JSON array");
    assert_eq!(
        rows_default.len(),
        1,
        "stored default_limit=1 should return 1 row, got: {}",
        rows_default.len()
    );

    // alias_run with runtime limit=3 → overrides stored default_limit → 3 rows.
    let rows_override: Vec<Value> = serde_json::from_value(client.call_tool(
        "alias_run",
        json!({"table": "emo", "name": "limit_test_alias", "limit": 3}),
    ))
    .expect("alias_run (runtime limit=3) should return a JSON array");
    assert_eq!(
        rows_override.len(),
        3,
        "runtime limit=3 should override stored default_limit=1 and return 3 rows, got: {}",
        rows_override.len()
    );

    // alias_run with runtime offset=2 → skip first 2 rows → 1 row remaining.
    let rows_offset: Vec<Value> = serde_json::from_value(client.call_tool(
        "alias_run",
        json!({"table": "emo", "name": "limit_test_alias", "limit": 3, "offset": 2}),
    ))
    .expect("alias_run (limit=3, offset=2) should return a JSON array");
    assert_eq!(
        rows_offset.len(),
        1,
        "limit=3 offset=2 with 3 total rows should return 1 row, got: {}",
        rows_offset.len()
    );
}

// ---------------------------------------------------------------------------
// Parameterized alias (MiniJinja filter_template)
// ---------------------------------------------------------------------------

/// Verifies that a parameterized alias (created with `filter_template` +
/// `params_schema`) can be executed via `alias_run` with a `params` object
/// that substitutes the MiniJinja placeholders.
///
/// Crux #1: render (MiniJinja) → parse (serde_json) → validate (ListFilter)
/// → Store::list pipeline must execute in full.
/// Crux #2: plain `filter` aliases remain unaffected (covered by existing tests).
#[test]
fn alias_parameterized_create_run() {
    let layout = make_layout();
    let mut client = McpClient::spawn(&layout.user_dir, &layout.project_dir);

    // Seed a row with a known text value.
    client.call_tool(
        "create",
        json!({
            "table": "emo",
            "data": {"text": "parameterized_seed_row"},
        }),
    );

    // alias_create with filter_template + params_schema.
    // The template uses {{ project }} as a MiniJinja placeholder.
    let created = client.call_tool(
        "alias_create",
        json!({
            "table": "emo",
            "name": "param_alias",
            "filter_template": r#"{"type": "like", "field": "text", "pattern": "{{ pattern }}"}"#,
            "params_schema": ["pattern"],
        }),
    );
    assert_eq!(
        created.get("created").and_then(Value::as_str),
        Some("param_alias"),
        "alias_create with filter_template should return created name, got: {created:?}"
    );

    // alias_list should include params_schema in the record.
    let listed: Vec<Value> =
        serde_json::from_value(client.call_tool("alias_list", json!({"table": "emo"})))
            .expect("alias_list should return a JSON array");
    let param_alias_entry = listed
        .iter()
        .find(|a| a.get("name").and_then(Value::as_str) == Some("param_alias"))
        .expect("param_alias should appear in alias_list");
    // params_schema should be stored (not null).
    assert!(
        !param_alias_entry
            .get("params_schema")
            .is_none_or(Value::is_null),
        "alias_list should include non-null params_schema for a template alias, got: {param_alias_entry:?}"
    );

    // alias_run with params — MiniJinja renders template → JSON parse → validate → list.
    let rows: Vec<Value> = serde_json::from_value(client.call_tool(
        "alias_run",
        json!({
            "table": "emo",
            "name": "param_alias",
            "params": {"pattern": "%parameterized_seed%"},
        }),
    ))
    .expect("alias_run with params should return a JSON array");
    assert!(
        !rows.is_empty(),
        "alias_run with rendered template should return at least one matching row"
    );
}

/// Verifies that running a parameterized alias without supplying `params`
/// returns ALIAS_PARAMS_REQUIRED.
///
/// Crux #1: the full render-parse-validate pipeline must be invoked; the
/// pipeline cannot be bypassed when params_schema is Some.
/// Crux #2: the error must be distinguishable (ALIAS_PARAMS_REQUIRED code).
#[test]
fn alias_parameterized_run_missing_params() {
    let layout = make_layout();
    let mut client = McpClient::spawn(&layout.user_dir, &layout.project_dir);

    // Create a parameterized alias.
    client.call_tool(
        "alias_create",
        json!({
            "table": "emo",
            "name": "param_alias_err",
            "filter_template": r#"{"type": "eq", "field": "text", "value": "{{ target }}"}"#,
            "params_schema": ["target"],
        }),
    );

    // alias_run without params must return a tool error (ALIAS_PARAMS_REQUIRED).
    let id = client.next_id();
    client.send_line(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": "alias_run",
            "arguments": {
                "table": "emo",
                "name": "param_alias_err"
                // params intentionally omitted
            }
        }
    }));
    let resp = client.recv_for(id);
    let is_tool_error = resp
        .pointer("/result/isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    assert!(
        is_tool_error,
        "alias_run without params on a template alias should return tool error, got: {resp}"
    );
    // Verify the error message contains "alias params required".
    let text = resp
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        text.contains("requires params"),
        "expected 'requires params' in error text, got: {text:?}"
    );
}

// ---------------------------------------------------------------------------
// query_aggregate scenarios (Phase 1 MultiTableQuery Aggregate)
//
// Scenarios (h)-(l) per workspace/tasks/aggregator-phase-1/subtask-3.md:
//   (h) Single source COUNT
//   (i) Multi UNION ALL COUNT (Crux #3: UNION ALL, never JOIN)
//   (j) Multi GROUP BY + HAVING + inner Sum (Crux #2: HAVING positioning)
//   (k) Unknown table → TABLE_NOT_FOUND
//   (l) Empty Multi → AGGREGATOR_ERROR
// ---------------------------------------------------------------------------

const AGG_SCHEMA: &str = "table: agg
fields:
  - name: text
    type: string
    required: true
  - name: tag
    type: string
    required: false
  - name: amount
    type: number
    required: false
";

struct AggLayout {
    _tmp: TempDir,
    user_dir: PathBuf,
    project_dir: PathBuf,
}

/// Build a user-scope layout with one or more same-shape `agg*` tables so
/// `query_aggregate` Multi can `ATTACH DATABASE` each backing `.db` file.
fn make_agg_layout(extra_tables: &[&str]) -> AggLayout {
    let tmp = TempDir::new().expect("tempdir");
    let user_dir = tmp.path().join("user");
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();
    for name in std::iter::once("agg").chain(extra_tables.iter().copied()) {
        let dir = user_dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        // Reuse the AGG_SCHEMA shape but rewrite the table name so every
        // mounted table shares the same field set.
        let schema = AGG_SCHEMA.replace("table: agg", &format!("table: {name}"));
        std::fs::write(dir.join("schema.yaml"), schema).unwrap();
    }
    AggLayout {
        _tmp: tmp,
        user_dir,
        project_dir,
    }
}

fn create_agg_row(client: &mut McpClient, table: &str, tag: &str, amount: f64) {
    let _ = client.call_tool(
        "create",
        json!({
            "table": table,
            "data": { "text": format!("row-{tag}-{amount}"), "tag": tag, "amount": amount }
        }),
    );
}

#[test]
fn query_aggregate_single_count_returns_row_count() {
    // (h) Single source COUNT.
    let layout = make_agg_layout(&[]);
    let mut client = McpClient::spawn(&layout.user_dir, &layout.project_dir);
    create_agg_row(&mut client, "agg", "a", 1.0);
    create_agg_row(&mut client, "agg", "b", 2.0);
    create_agg_row(&mut client, "agg", "a", 3.0);
    let result = client.call_tool(
        "query_aggregate",
        json!({
            "sources": { "kind": "single", "value": "agg" },
            "aggregator": { "kind": "count" }
        }),
    );
    assert_eq!(result.get("kind").and_then(Value::as_str), Some("count"));
    assert_eq!(result.get("value").and_then(Value::as_i64), Some(3));
}

#[test]
fn query_aggregate_multi_count_returns_combined_total() {
    // (i) Multi UNION ALL COUNT — Crux #3 verification.
    // UNION ALL means rows-from-A + rows-from-B; a JOIN would yield A*B.
    let layout = make_agg_layout(&["agg2"]);
    let mut client = McpClient::spawn(&layout.user_dir, &layout.project_dir);
    create_agg_row(&mut client, "agg", "x", 1.0);
    create_agg_row(&mut client, "agg", "x", 2.0);
    create_agg_row(&mut client, "agg2", "y", 10.0);
    create_agg_row(&mut client, "agg2", "y", 20.0);
    create_agg_row(&mut client, "agg2", "y", 30.0);
    let result = client.call_tool(
        "query_aggregate",
        json!({
            "sources": { "kind": "multi", "value": ["agg", "agg2"] },
            "aggregator": { "kind": "count" }
        }),
    );
    assert_eq!(result.get("kind").and_then(Value::as_str), Some("count"));
    assert_eq!(
        result.get("value").and_then(Value::as_i64),
        Some(5),
        "expected UNION ALL semantics (2+3=5), JOIN would have yielded 6"
    );
}

#[test]
fn query_aggregate_groupby_with_having_filters_groups() {
    // (j) Multi GROUP BY + HAVING + inner Sum — Crux #2 verification.
    let layout = make_agg_layout(&["agg2"]);
    let mut client = McpClient::spawn(&layout.user_dir, &layout.project_dir);
    create_agg_row(&mut client, "agg", "a", 5.0);
    create_agg_row(&mut client, "agg", "a", 5.0);
    create_agg_row(&mut client, "agg", "b", 1.0);
    create_agg_row(&mut client, "agg2", "c", 4.0);
    create_agg_row(&mut client, "agg2", "c", 6.0);
    let result = client.call_tool(
        "query_aggregate",
        json!({
            "sources": { "kind": "multi", "value": ["agg", "agg2"] },
            "aggregator": {
                "kind": "group_by",
                "by_field": "tag",
                "having": { "type": "eq", "field": "tag", "value": "a" },
                "inner": { "kind": "sum", "field": "amount" }
            }
        }),
    );
    assert_eq!(result.get("kind").and_then(Value::as_str), Some("groups"));
    let groups = result
        .get("value")
        .and_then(Value::as_array)
        .expect("groups array");
    assert_eq!(
        groups.len(),
        1,
        "HAVING tag='a' should leave 1 group, got {groups:?}"
    );
    let g = &groups[0];
    assert_eq!(g.get("key").and_then(Value::as_str), Some("a"));
    assert_eq!(g.get("count").and_then(Value::as_i64), Some(2));
    let inner = g
        .get("value")
        .and_then(Value::as_f64)
        .expect("inner sum should be a number");
    assert!((inner - 10.0).abs() < 1e-9, "inner sum 5+5=10, got {inner}");
}

#[test]
fn query_aggregate_unknown_table_returns_table_not_found() {
    // (k) Unknown source name → TABLE_NOT_FOUND code in the structured error.
    let layout = make_agg_layout(&[]);
    let mut client = McpClient::spawn(&layout.user_dir, &layout.project_dir);
    let id = client.next_id();
    client.send_line(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": "query_aggregate",
            "arguments": {
                "sources": { "kind": "single", "value": "no_such_table" },
                "aggregator": { "kind": "count" }
            }
        }
    }));
    let resp = client.recv_for(id);
    let text = resp
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        text.contains("TABLE_NOT_FOUND") || text.contains("table not found"),
        "expected TABLE_NOT_FOUND in error text, got: {text:?}"
    );
}

// =============================================================================
// Phase 2 — Global Alias e2e (Multi / Pattern sources via alias_run)
// =============================================================================

/// alias_create with Multi sources + Count aggregator, then alias_run
/// dispatches to execute_aggregate and returns the combined count.
#[test]
fn alias_create_multi_sources_with_count_aggregator_round_trip() {
    let layout = make_agg_layout(&["agg2"]);
    let mut client = McpClient::spawn(&layout.user_dir, &layout.project_dir);
    create_agg_row(&mut client, "agg", "x", 1.0);
    create_agg_row(&mut client, "agg", "x", 2.0);
    create_agg_row(&mut client, "agg2", "y", 10.0);
    create_agg_row(&mut client, "agg2", "y", 20.0);
    create_agg_row(&mut client, "agg2", "y", 30.0);

    // Register a global alias spanning two tables with a Count aggregator.
    // No `table` argument — `sources` is the Phase 2 surface.
    let created = client.call_tool(
        "alias_create",
        json!({
            "name": "agg_combined_count",
            "sources": { "kind": "multi", "value": ["agg", "agg2"] },
            "aggregator": { "kind": "count" },
            "filter": { "type": "like", "field": "tag", "pattern": "%" },
            "description": "combined count across agg + agg2"
        }),
    );
    assert_eq!(
        created.get("created").and_then(Value::as_str),
        Some("agg_combined_count")
    );

    // alias_run must dispatch to execute_aggregate and return Count(5).
    let result = client.call_tool("alias_run", json!({ "name": "agg_combined_count" }));
    assert_eq!(result.get("kind").and_then(Value::as_str), Some("count"));
    assert_eq!(
        result.get("value").and_then(Value::as_i64),
        Some(5),
        "Multi UNION ALL (2+3=5) via alias_run + execute_aggregate, got {result:?}"
    );
}

/// alias_create with Pattern sources + Count aggregator, then alias_run
/// resolves the glob against the live registry's mounted table list
/// (`agg` + `agg2` → Multi(\[agg, agg2\])) before dispatching to
/// execute_aggregate.
#[test]
fn alias_create_pattern_sources_resolves_at_alias_run() {
    let layout = make_agg_layout(&["agg2"]);
    let mut client = McpClient::spawn(&layout.user_dir, &layout.project_dir);
    create_agg_row(&mut client, "agg", "p", 1.0);
    create_agg_row(&mut client, "agg2", "p", 2.0);
    create_agg_row(&mut client, "agg2", "p", 3.0);

    let created = client.call_tool(
        "alias_create",
        json!({
            "name": "agg_glob_count",
            "sources": { "kind": "pattern", "value": "agg*" },
            "aggregator": { "kind": "count" },
            "filter": { "type": "like", "field": "tag", "pattern": "%" }
        }),
    );
    assert_eq!(
        created.get("created").and_then(Value::as_str),
        Some("agg_glob_count")
    );

    let result = client.call_tool("alias_run", json!({ "name": "agg_glob_count" }));
    assert_eq!(result.get("kind").and_then(Value::as_str), Some("count"));
    assert_eq!(
        result.get("value").and_then(Value::as_i64),
        Some(3),
        "Pattern 'agg*' must resolve to [agg, agg2] and count 1+2=3, got {result:?}"
    );
}

/// alias_create with Pattern sources that match zero mounted tables
/// must surface AGGREGATOR_ERROR at alias_run time (the registry-side
/// resolution failure is not deferred to ATTACH DATABASE).
#[test]
fn alias_run_pattern_zero_match_returns_aggregator_error() {
    let layout = make_agg_layout(&[]);
    let mut client = McpClient::spawn(&layout.user_dir, &layout.project_dir);
    client.call_tool(
        "alias_create",
        json!({
            "name": "no_match",
            "sources": { "kind": "pattern", "value": "zzz_*" },
            "aggregator": { "kind": "count" },
            "filter": { "type": "like", "field": "tag", "pattern": "%" }
        }),
    );

    let id = client.next_id();
    client.send_line(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": "alias_run",
            "arguments": { "name": "no_match" }
        }
    }));
    let resp = client.recv_for(id);
    let text = resp
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        text.contains("AGGREGATOR_ERROR") || text.contains("matched zero tables"),
        "expected AGGREGATOR_ERROR for zero-match Pattern, got: {text:?}"
    );
}

/// `sources` and the legacy `table` argument are mutually exclusive on
/// `alias_create`; supplying both must return a structured error.
#[test]
fn alias_create_with_both_sources_and_table_is_rejected() {
    let layout = make_agg_layout(&[]);
    let mut client = McpClient::spawn(&layout.user_dir, &layout.project_dir);
    let id = client.next_id();
    client.send_line(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": "alias_create",
            "arguments": {
                "name": "conflict",
                "table": "agg",
                "sources": { "kind": "single", "value": "agg" },
                "filter": { "type": "like", "field": "tag", "pattern": "%" }
            }
        }
    }));
    let resp = client.recv_for(id);
    let text = resp
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        text.contains("mutually exclusive") || text.contains("sources"),
        "expected mutually-exclusive error, got: {text:?}"
    );
}

// =============================================================================
// Phase 3 — alias_create `fields` e2e (stored field projection)
// =============================================================================

/// (T1 — happy path) alias_create with a stored `fields` projection;
/// alias_run called WITHOUT a run-time `fields` argument must apply the
/// stored projection (Crux #2 fallback).
///
/// Assertion: every returned row contains the projected key ("text") and
/// does NOT contain keys that were projected away ("id", "tags").
#[test]
fn alias_create_with_fields_then_alias_run_uses_stored_fields() {
    let layout = make_layout();
    let mut client = McpClient::spawn(&layout.user_dir, &layout.project_dir);

    // Seed a row so alias_run has something to return.
    client.call_tool(
        "create",
        json!({
            "table": "emo",
            "data": { "text": "projected row", "tags": ["a"] },
        }),
    );

    // alias_create with a List{fields:["text"]} stored projection.
    // Use a "like '%'" filter to match all rows (no valid "all" variant exists).
    let created = client.call_tool(
        "alias_create",
        json!({
            "sources": { "kind": "single", "value": "emo" },
            "name": "emo_text_only",
            "filter": { "type": "like", "field": "text", "pattern": "%" },
            "fields": { "mode": "list", "fields": ["text"] }
        }),
    );
    assert_eq!(
        created.get("created").and_then(Value::as_str),
        Some("emo_text_only"),
        "alias_create should return created name, got: {created:?}"
    );

    // alias_run without run-time fields → must fall back to stored projection.
    let rows: Vec<Value> =
        serde_json::from_value(client.call_tool("alias_run", json!({ "name": "emo_text_only" })))
            .expect("alias_run should return a JSON array");

    assert!(
        !rows.is_empty(),
        "alias_run should return at least one row, got: {rows:?}"
    );
    for row in &rows {
        let data = row.get("data").expect("row.data missing");
        assert!(
            data.get("text").is_some(),
            "stored fields projection must include 'text', got: {data:?}"
        );
        assert!(
            data.get("id").is_none(),
            "stored fields projection must exclude 'id' (not in fields list), got: {data:?}"
        );
        assert!(
            data.get("tags").is_none(),
            "stored fields projection must exclude 'tags' (not in fields list), got: {data:?}"
        );
    }
}

/// (T2 — Crux #3 regression guard) alias_create with NO `fields` stored;
/// alias_run called WITHOUT run-time `fields` must NOT apply an empty
/// projection — all fields must be present (NULL record.fields ≠ empty list).
#[test]
fn alias_create_without_fields_alias_run_returns_all_fields() {
    let layout = make_layout();
    let mut client = McpClient::spawn(&layout.user_dir, &layout.project_dir);

    client.call_tool(
        "create",
        json!({
            "table": "emo",
            "data": { "text": "no projection row", "tags": ["b"] },
        }),
    );

    // Use a "like '%'" filter to match all rows.
    let created = client.call_tool(
        "alias_create",
        json!({
            "sources": { "kind": "single", "value": "emo" },
            "name": "emo_no_fields",
            "filter": { "type": "like", "field": "text", "pattern": "%" }
            // no `fields` key — stored default must be NULL
        }),
    );
    assert_eq!(
        created.get("created").and_then(Value::as_str),
        Some("emo_no_fields"),
        "alias_create should return created name, got: {created:?}"
    );

    let rows: Vec<Value> =
        serde_json::from_value(client.call_tool("alias_run", json!({ "name": "emo_no_fields" })))
            .expect("alias_run should return a JSON array");

    assert!(
        !rows.is_empty(),
        "alias_run should return at least one row, got: {rows:?}"
    );
    // Crux #3: NULL stored fields must never be coerced to an empty projection.
    // All schema fields ("id", "text") must be present in every returned row.
    for row in &rows {
        let data = row.get("data").expect("row.data missing");
        assert!(
            data.get("text").is_some(),
            "NULL stored fields must not suppress 'text', got: {data:?}"
        );
    }
    // The top-level "id" field lives on the row object, not under "data".
    for row in &rows {
        assert!(
            row.get("id").is_some(),
            "NULL stored fields must not suppress row 'id', got: {row:?}"
        );
    }
}

#[test]
fn query_aggregate_empty_multi_returns_aggregator_error() {
    // (l) Empty Multi sources → AGGREGATOR_ERROR.
    let layout = make_agg_layout(&[]);
    let mut client = McpClient::spawn(&layout.user_dir, &layout.project_dir);
    let id = client.next_id();
    client.send_line(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": "query_aggregate",
            "arguments": {
                "sources": { "kind": "multi", "value": [] },
                "aggregator": { "kind": "count" }
            }
        }
    }));
    let resp = client.recv_for(id);
    let text = resp
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        text.contains("AGGREGATOR_ERROR")
            || text.contains("aggregator error")
            || text.contains("at least one"),
        "expected AGGREGATOR_ERROR in error text, got: {text:?}"
    );
}
