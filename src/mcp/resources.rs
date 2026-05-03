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

/// Hand-written cheat sheet listing all 6 tools with descriptions / shapes.
pub const TOOLS_DOC: &str = r#"# mini-app-mcp — Tools Reference

## `info`
Return the parsed schema (table name and field definitions).
- **Input**: *(no parameters)*
- **Output**: JSON object `{ "table": "...", "fields": [...] }`
- Annotations: `readOnlyHint=true`, `idempotentHint=true`

## `create`
Create a new row in the table.
- **Input**: `{ "data": { ... } }` — JSON object matching `schema.yaml`
- **Output**: created record `{ "id": "...", "data": {...}, "created_at": "...", "updated_at": "..." }`
- Annotations: `readOnlyHint=false`, `idempotentHint=false`

## `get`
Fetch a single row by UUID.
- **Input**: `{ "id": "<uuid>" }`
- **Output**: record `{ "id": "...", "data": {...}, ... }`
- Annotations: `readOnlyHint=true`, `idempotentHint=true`

## `list`
List rows with optional pagination (ordered by `created_at` descending).
- **Input**: `{ "limit": <u32 optional>, "offset": <u32 optional> }`
  - `limit` default 100, max 1000
- **Output**: array of records `[{ "id": "...", ... }, ...]`
- Annotations: `readOnlyHint=true`, `idempotentHint=true`

## `update`
Replace the `data` of an existing row by UUID.
- **Input**: `{ "id": "<uuid>", "data": { ... } }` — `data` must match `schema.yaml`
- **Output**: updated record `{ "id": "...", "data": {...}, ... }`
- Annotations: `readOnlyHint=false`, `idempotentHint=true`

## `delete`
Delete the row with the given UUID. Returns an error if the row does not exist.
- **Input**: `{ "id": "<uuid>" }`
- **Output**: `{ "deleted": "<uuid>" }`
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
        properties.insert(
            field.name.clone(),
            serde_json::json!({ "type": type_str }),
        );
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
            fields: vec![
                FieldDef {
                    name: "title".to_string(),
                    ty: FieldType::String,
                    required: true,
                },
                FieldDef {
                    name: "count".to_string(),
                    ty: FieldType::Number,
                    required: false,
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
            fields: vec![
                FieldDef { name: "s".to_string(), ty: FieldType::String, required: false },
                FieldDef { name: "n".to_string(), ty: FieldType::Number, required: false },
                FieldDef { name: "b".to_string(), ty: FieldType::Boolean, required: false },
                FieldDef { name: "a".to_string(), ty: FieldType::Array, required: false },
                FieldDef { name: "o".to_string(), ty: FieldType::Object, required: false },
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
    fn tools_doc_contains_all_six_tools() {
        for tool in &["info", "create", "get", "list", "update", "delete"] {
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
        ] {
            assert!(
                ERRORS_DOC.contains(code),
                "ERRORS_DOC must contain code '{code}'"
            );
        }
    }
}
