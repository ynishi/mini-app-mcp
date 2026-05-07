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
        assert!(
            resp.get("error").is_none(),
            "initialize failed: {resp:?}"
        );
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
        let result = self.rpc(
            "tools/call",
            json!({"name": name, "arguments": arguments}),
        );
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
