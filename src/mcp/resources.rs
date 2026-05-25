/// Static documentation strings and JSON Schema derivation for MCP Resources.
///
/// Keeping these here keeps `server.rs` lean; only the construction helpers
/// and the `list_resources` / `read_resource` dispatch live in `server.rs`.
use crate::schema::{FieldType, SchemaConfig};

// ---------------------------------------------------------------------------
// Embedded static content
// ---------------------------------------------------------------------------

/// README.md embedded at compile time so it ships in the binary.
pub const README: &str = include_str!("../../README.md");

/// Hand-written cheat sheet listing all 12 tools with descriptions / shapes.
pub const TOOLS_DOC: &str = r#"# mini-app-mcp — Tools Reference

## `table` argument (all tools)

All tools accept an optional `table` argument:
- **Multi-table mode** (`MINI_APP_USER_DIR`/`MINI_APP_PROJECT_DIR` set): `table` is **required**.
  Omitting it returns a `TABLE_REQUIRED` error (`data.code="TABLE_REQUIRED"`).
- **Legacy single-table mode** (`MINI_APP_SCHEMA`+`MINI_APP_DB` set): `table` may be **omitted**.
  The single configured table is used automatically.
- Specifying an unknown table name returns `TABLE_NOT_FOUND` (`data.code="TABLE_NOT_FOUND"`).

## `info`
Return the parsed schema (table name and field definitions) for the given `table`.
- **Input**: `{ "table": "<name>" }` (table optional in legacy mode)
- **Output**: JSON object `{ "table": "...", "title": "...", "description": "...", "fields": [...] }` (`title` and `description` are `null` when not set)
- Annotations: `readOnlyHint=true`, `idempotentHint=true`

## `create`
Create a new row in the given `table`.
- **Input**: `{ "data": { ... }, "table": "<name>" }` — `data` must match `schema.yaml`; `table` optional in legacy mode
- **Output**: created record `{ "id": "...", "data": {...}, "created_at": "...", "updated_at": "..." }`
- Annotations: `readOnlyHint=false`, `idempotentHint=false`

## `get`
Fetch a single row by UUID from the given `table`.
- **Input**: `{ "id": "<uuid>", "table": "<name>" }` (table optional in legacy mode)
- **Output**: record `{ "id": "...", "data": {...}, ... }`
- Annotations: `readOnlyHint=true`, `idempotentHint=true`

## `list`
List rows with optional pagination (ordered by `created_at` descending) from the given `table`.
- **Input**: `{ "limit": <u32 optional>, "offset": <u32 optional>, "table": "<name>" }`
  - `limit` default 100, max 1000; `table` optional in legacy mode
- **Output**: array of records `[{ "id": "...", ... }, ...]`
- Annotations: `readOnlyHint=true`, `idempotentHint=true`

## `update`
Update the `data` of an existing row by UUID in the given `table`. Defaults to RFC 7396 shallow merge.
- **Input**: `{ "id": "<uuid>", "data": { ... }, "table": "<name>", "mode": "merge"|"replace" }`
  - `data` must match `schema.yaml`; `table` optional in legacy mode; `mode` optional (default `"merge"`)
- **Merge mode** (default): fields absent from `data` are preserved from the stored row; `null` deletes an optional field; `null` on a required field returns a Validation error. Full schema validation runs on the merged result (RFC 7396).
- **Replace mode** (`"mode": "replace"`): replaces the entire `data` object — identical to the pre-breaking-change behavior.
- **Output**: updated record `{ "id": "...", "data": {...}, ... }`
- Annotations: `readOnlyHint=false`, `idempotentHint=true`

## `delete`
Delete the row with the given UUID from the given `table`. Returns an error if the row does not exist.
- **Input**: `{ "id": "<uuid>", "table": "<name>" }` (table optional in legacy mode)
- **Output**: `{ "deleted": "<uuid>" }`
- Annotations: `readOnlyHint=false`, `destructiveHint=true`, `idempotentHint=true`

## `reload`
Re-scan `MINI_APP_USER_DIR` / `MINI_APP_PROJECT_DIR` and atomically replace the table registry. Legacy `MINI_APP_SCHEMA` + `MINI_APP_DB` are re-applied if set. In-flight requests complete against the previous snapshot.
- **Input**: `{}` — no arguments required (`table` argument is ignored)
- **Output**: `{ "mounted": N, "added": ["table1", ...], "removed": ["table2", ...] }`
- **Limitations**: no file watcher (explicit invocation only); whole-registry replace (no per-table partial reload); no schema migration for existing rows.
- Annotations: `readOnlyHint=false`, `destructiveHint=false`, `idempotentHint=false`

## `data_snapshot`
Create per-table SQLite snapshot dump(s) under `{scope_root}/_snapshots/`. Uses the rusqlite hot backup API so the source DB remains open and writable during the operation. Schema is not modified.
- **Input**: `{ "table": "<name>" (optional), "scope": "project"|"user" (optional), "dry_run": true|false (optional) }`
  - `table`: target a single table; omit to snapshot all tables in the given scope.
  - `scope`: restrict to `"project"` (`MINI_APP_PROJECT_DIR`) or `"user"` (`MINI_APP_USER_DIR`); omit for all scopes.
  - `dry_run=true`: return `affects` (target tables, row counts, would-purge counts) **without** any FS or DB write.
- **Output (dry_run=false)**: `{ "snapshotted": [{"table": "...", "scope": "...", "snapshot_path": "...", "unix_secs": N}, ...], "purged": [{"table": "...", "generations_removed": N}, ...] }`
- **Output (dry_run=true)**: `{ "dry_run": true, "affects": { "target_tables": [...], "row_counts": {"table": N}, "would_purge_generations": {"table": N} } }`
- **Retention**: controlled by `MINI_APP_SNAPSHOT_RETENTION` (default 10); strictly separate from `MINI_APP_BACKUP_RETENTION`.
- Annotations: `readOnlyHint=false`, `destructiveHint=false`, `idempotentHint=false`

## `row_materialize`
Write row data from a table to absolute filesystem path(s) with multi-format serialisation and SHA-256 integrity digest.
- **Input**: `{ "table": "<name>" (optional), "selector": {...}, "fields": {...}, "format": "...", "dest": "<absolute-path>", "concat": true|false (optional), "write_mode": "overwrite"|"error" (optional), "dry_run": true|false (optional) }`
  - `selector`: `{"type": "by_id", "id": "<uuid>"}` or `{"type": "by_filter", "filter": {...}, "limit": N, "offset": N}`.
  - `fields`: `{"mode": "all"}` (all schema fields in declaration order) or `{"mode": "list", "fields": ["f1", "f2"]}` (named subset in specified order).
  - `format`: `raw` (`.txt`, field values joined by newlines) | `markdown` (`.md`, field headings) | `json` (`.json`, JSON object/array) | `yaml` (`.yaml`, YAML document/stream).
  - `dest`: **absolute path required** — relative paths are rejected immediately (`MATERIALIZE_DEST_RELATIVE`). When `concat=false` this is a directory; when `concat=true` it is the output file path.
  - `concat`: `false` (default) — one file per row at `{dest}/{id}.{ext}`, `row_id` is set in each result. `true` — all rows concatenated into one file at `{dest}`, `row_id` is `null`. `concat=true` with `selector=by_id` is an error.
  - `write_mode`: `overwrite` (default) | `error` (return `MATERIALIZE_DEST_INVALID` if target file already exists; checked even with `dry_run=true`).
  - `dry_run`: `true` — validation, projection, serialisation, and SHA-256 computation run normally but **no file is written**; returned `files` carry would-be `path`, `bytes`, and `sha256` values.
- **Output**: `{ "count": N, "files": [{ "path": "<abs-path>", "bytes": N, "sha256": "<64-char-hex>", "row_id": "<uuid>" | null }, ...] }`
  - `sha256` is always a 64-character lower-hex SHA-256 digest (never empty, even with `dry_run=true`).
  - `row_id` is `null` when `concat=true`; the source row UUID when `concat=false`.
- Annotations: `readOnlyHint=false`, `destructiveHint=true`, `idempotentHint=true`
"#;

/// Hand-written reference table of all error codes from `src/error.rs`.
pub const ERRORS_DOC: &str = r#"# mini-app-mcp — Error Code Reference

All MCP errors carry a structured JSON `data` object with at minimum:
```json
{ "code": "<CODE>", "message": "<human text>" }
```

## Error Codes

| Code | HTTP-like | Returned when |
|---|---|---|
| `VALIDATION_ERROR` | 422 | A required field is missing or a value has the wrong JSON type. Also includes `"field": "<name>"` in `data`. |
| `NOT_FOUND` | 404 | No row with the given `id` exists. Also includes `"id": "<id>"` in `data`. |
| `SCHEMA_ERROR` | 500 | `schema.yaml` cannot be parsed or is structurally invalid (startup error). |
| `STORAGE_ERROR` | 500 | Underlying SQLite operation failed. |
| `IO_ERROR` | 500 | File open / read failed (startup error). |
| `CONFIG_ERROR` | 500 | Environment-variable or `.env` configuration is invalid (startup error). |
| `TABLE_NOT_FOUND` | 404 | The specified `table` name is not mounted in the server. Also includes `"table": "<name>"` in `data`. |
| `TABLE_REQUIRED` | 422 | Multi-table mode requires a `table` argument but it was omitted. |
| `SNAPSHOT_ERROR` | 500 | SQLite snapshot creation or purge failed. |
| `MATERIALIZE_DEST_RELATIVE` | 422 | The `dest` path supplied to `row_materialize` is not absolute. Absolute paths are required (Agent-First trust model). Also includes `"path": "<path>"` in `data`. |
| `MATERIALIZE_DEST_INVALID` | 422 | The `dest` path is absolute but invalid (e.g. parent directory cannot be created, or file already exists when `write_mode=error`). Also includes `"path"` and `"reason"` in `data`. |
| `MATERIALIZE_IO_ERROR` | 500 | A filesystem I/O error occurred during `row_materialize` (file write failure). |
| `MATERIALIZE_SHA256_ERROR` | 500 | SHA-256 computation failed during `row_materialize` (e.g. blocking task panic). |
| `MATERIALIZE_ROW_NOT_FOUND` | 404 | The row id specified in a `by_id` selector was not found. Also includes `"id": "<id>"` in `data`. |
| `MATERIALIZE_EMPTY_RESULT` | 404 | The `by_filter` selector matched zero rows. |
| `MATERIALIZE_FORMAT_ERROR` | 500 | Serialisation to the requested format failed during `row_materialize`. |
| `MATERIALIZE_FIELD_UNKNOWN` | 422 | A projected field name is not present in the table schema. Also includes `"field": "<name>"` in `data`. |
| `MATERIALIZE_INVALID_PARAM` | 422 | `row_materialize` parameters are structurally invalid (e.g. `concat=true` with `selector=by_id`). Also includes `"field"` and `"reason"` in `data`. |

## Validation Error Example
```json
{
  "code": "VALIDATION_ERROR",
  "message": "validation error on field 'title': required field missing",
  "field": "title"
}
```

## Not-Found Error Example
```json
{
  "code": "NOT_FOUND",
  "message": "row not found: abc-123",
  "id": "abc-123"
}
```

## Table-Not-Found Error Example
```json
{
  "code": "TABLE_NOT_FOUND",
  "message": "table not found: my_table",
  "table": "my_table"
}
```

## Table-Required Error Example
```json
{
  "code": "TABLE_REQUIRED",
  "message": "table argument is required in multi-table mode"
}
```
"#;

// ---------------------------------------------------------------------------
// JSON Schema derivation
// ---------------------------------------------------------------------------

/// Derives a JSON Schema (draft-07) object from a [`SchemaConfig`].
///
/// Type mapping:
/// - `String` → `"string"`
/// - `Number` → `"number"`
/// - `Boolean` → `"boolean"`
/// - `Array`   → `"array"`
/// - `Object`  → `"object"`
///
/// The generated schema uses `additionalProperties: true` to stay compatible
/// with the Agent-First extensibility constraint (unknown keys are accepted).
pub fn derive_json_schema(schema: &SchemaConfig) -> serde_json::Value {
    let mut properties = serde_json::Map::new();
    let mut required: Vec<serde_json::Value> = Vec::new();

    for field in &schema.fields {
        let type_str = match field.ty {
            FieldType::String => "string",
            FieldType::Number => "number",
            FieldType::Boolean => "boolean",
            FieldType::Array => "array",
            FieldType::Object => "object",
        };
        properties.insert(field.name.clone(), serde_json::json!({ "type": type_str }));
        if field.required {
            required.push(serde_json::Value::String(field.name.clone()));
        }
    }

    serde_json::json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": schema.table,
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{FieldDef, FieldType, SchemaConfig};

    fn make_schema() -> SchemaConfig {
        SchemaConfig {
            table: "items".to_string(),
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
                    name: "count".to_string(),
                    ty: FieldType::Number,
                    required: false,
                    description: None,
                },
            ],
            dump: None,
        }
    }

    #[test]
    fn derive_json_schema_has_correct_title() {
        let schema = make_schema();
        let js = derive_json_schema(&schema);
        assert_eq!(js["title"], "items");
    }

    #[test]
    fn derive_json_schema_required_array_contains_required_fields() {
        let schema = make_schema();
        let js = derive_json_schema(&schema);
        let req = js["required"].as_array().expect("required must be array");
        assert!(req.contains(&serde_json::Value::String("title".to_string())));
        assert!(!req.contains(&serde_json::Value::String("count".to_string())));
    }

    #[test]
    fn derive_json_schema_type_mapping() {
        let schema = SchemaConfig {
            table: "t".to_string(),
            title: None,
            description: None,
            fields: vec![
                FieldDef {
                    name: "s".to_string(),
                    ty: FieldType::String,
                    required: false,
                    description: None,
                },
                FieldDef {
                    name: "n".to_string(),
                    ty: FieldType::Number,
                    required: false,
                    description: None,
                },
                FieldDef {
                    name: "b".to_string(),
                    ty: FieldType::Boolean,
                    required: false,
                    description: None,
                },
                FieldDef {
                    name: "a".to_string(),
                    ty: FieldType::Array,
                    required: false,
                    description: None,
                },
                FieldDef {
                    name: "o".to_string(),
                    ty: FieldType::Object,
                    required: false,
                    description: None,
                },
            ],
            dump: None,
        };
        let js = derive_json_schema(&schema);
        assert_eq!(js["properties"]["s"]["type"], "string");
        assert_eq!(js["properties"]["n"]["type"], "number");
        assert_eq!(js["properties"]["b"]["type"], "boolean");
        assert_eq!(js["properties"]["a"]["type"], "array");
        assert_eq!(js["properties"]["o"]["type"], "object");
    }

    #[test]
    fn readme_starts_with_heading() {
        assert!(
            README.starts_with("# mini-app-mcp"),
            "README must start with '# mini-app-mcp'"
        );
    }

    #[test]
    fn tools_doc_contains_all_seven_tools() {
        for tool in &[
            "info", "create", "get", "list", "update", "delete", "reload",
        ] {
            assert!(
                TOOLS_DOC.contains(&format!("## `{tool}`")),
                "TOOLS_DOC must contain section for '{tool}'"
            );
        }
    }

    #[test]
    fn errors_doc_contains_all_error_codes() {
        for code in &[
            "VALIDATION_ERROR",
            "NOT_FOUND",
            "SCHEMA_ERROR",
            "STORAGE_ERROR",
            "IO_ERROR",
            "CONFIG_ERROR",
            "TABLE_NOT_FOUND",
            "TABLE_REQUIRED",
            "SNAPSHOT_ERROR",
            "MATERIALIZE_DEST_RELATIVE",
            "MATERIALIZE_DEST_INVALID",
            "MATERIALIZE_IO_ERROR",
            "MATERIALIZE_SHA256_ERROR",
            "MATERIALIZE_ROW_NOT_FOUND",
            "MATERIALIZE_EMPTY_RESULT",
            "MATERIALIZE_FORMAT_ERROR",
            "MATERIALIZE_FIELD_UNKNOWN",
            "MATERIALIZE_INVALID_PARAM",
        ] {
            assert!(
                ERRORS_DOC.contains(code),
                "ERRORS_DOC must contain code '{code}'"
            );
        }
    }

    #[test]
    fn tools_doc_contains_data_snapshot() {
        assert!(
            TOOLS_DOC.contains("## `data_snapshot`"),
            "TOOLS_DOC must contain section for 'data_snapshot'"
        );
    }

    #[test]
    fn tools_doc_contains_row_materialize() {
        assert!(
            TOOLS_DOC.contains("## `row_materialize`"),
            "TOOLS_DOC must contain section for 'row_materialize'"
        );
    }
}
