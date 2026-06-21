/// Static documentation strings and JSON Schema derivation for MCP Resources.
///
/// Keeping these here keeps `server.rs` lean; only the construction helpers
/// and the `list_resources` / `read_resource` dispatch live in `server.rs`.
use crate::schema::{FieldType, SchemaConfig};

// ---------------------------------------------------------------------------
// Embedded static content
// ---------------------------------------------------------------------------

/// Agent quickstart embedded at compile time so it ships in the binary.
///
/// Served via the `docs://quickstart` MCP resource. Lives at
/// `crates/mcp/QUICKSTART.md` (inside this crate's package root) so
/// `cargo publish` finds it identically in the git tree and in the
/// extracted package tarball.
///
/// This is intentionally **not** the workspace-root `README.md`. The
/// workspace README is human-facing (build / releases / contribution
/// instructions, ~36KB); this file is agent-facing (mode detection,
/// first-call recipe, pointers to the other `docs://` resources, ~70
/// lines).
pub const QUICKSTART: &str = include_str!("../../QUICKSTART.md");

/// Hand-written cheat sheet listing all 18 tools with descriptions / shapes.
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

## `schema_create`
Create a new table schema (`schema.yaml` + SQLite DB) in the given scope. No automatic DDL migrations are applied — the DB is created empty.
- **Input**: `{ "table": "<name>", "scope": "project"|"user", "fields": [{"name": "...", "type": "string|number|boolean|array|object", "required": true|false, "description": "..." (optional)}, ...], "dry_run": true|false (optional) }`
  - `scope`: `"project"` (`MINI_APP_PROJECT_DIR`) or `"user"` (`MINI_APP_USER_DIR`).
  - `dry_run=true`: verify path is absent and return `affects` without writing.
- **Output**: `{ "created": "<table>", "path": "<schema_path>", "affects": {"would_create_yaml": true} }` (or `dry_run` shape when `dry_run=true`)
- Returns `SCHEMA_EXISTS` (`data.code`) if the schema already exists.
- Annotations: `readOnlyHint=false`, `destructiveHint=false`, `idempotentHint=false`

## `schema_update`
Overwrite an existing table schema with new field definitions. **No DDL is applied** to the existing SQLite table — column structure changes are the operator's responsibility.
- **Input**: `{ "table": "<name>", "scope": "project"|"user", "fields": [...], "dry_run": true|false (optional) }`
  - `dry_run=true`: return field diff (`fields_added`, `fields_removed`, `type_changed`) without writing.
- **Output**: `{ "updated": "<table>", "path": "<schema_path>", "diff": {...} }` (or `dry_run` diff shape)
- Returns `TABLE_NOT_FOUND` (`data.code`) when `table` is not mounted.
- Annotations: `readOnlyHint=false`, `destructiveHint=false`, `idempotentHint=true`

## `schema_delete`
Remove a table schema (`schema.yaml`) and unmount it from the registry. **The DB file is not deleted** — remove `{scope_root}/{table}/{table}.db` manually if needed. Use `dry_run=true` first — rows become inaccessible after deletion.
- **Input**: `{ "table": "<name>", "scope": "project"|"user", "dry_run": true|false (optional) }`
  - `dry_run=true`: return `rows_orphaned` + `would_remove_yaml` without deleting.
- **Output**: `{ "deleted": "<table>", "rows_orphaned": N }` (or `dry_run` shape)
- Returns `TABLE_NOT_FOUND` (`data.code`) when `table` is not mounted.
- Annotations: `readOnlyHint=false`, `destructiveHint=true`, `idempotentHint=true`

## `schema_batch`
Execute a list of schema operations atomically under a single SQLite SAVEPOINT. All ops must target the same table. On any op failure the SAVEPOINT is rolled back; YAML is never written (all-or-nothing).
- **Input**: `{ "table": "<name>", "ops": [{"op": "query"|"schema_create"|"schema_update"|"schema_delete", ...}] }`
  - Op types: `query` (raw SQL inside SAVEPOINT — schema validation bypassed), `schema_create`, `schema_update`, `schema_delete`.
- **Output**: `{ "results": [{"op": "...", "ok": true, "affected": N, "affects": {"deleted": N, "inserted": N} (Replace only)}], "committed": true|false }`
- Annotations: `readOnlyHint=false`, `destructiveHint=true`, `idempotentHint=false`

## `alias_create`
Register a named query alias for a table. The alias stores a `filter` expression and an optional `default_limit`. Alias names are unique per table; the alias namespace is isolated to `table` — aliases belonging to other tables are never affected.
- **Input**: `{ "table": "<name>", "name": "<alias>", "filter": {...}, "limit": <u32 optional>, "description": "<str optional>" }`
  - `filter`: the same filter object accepted by the `list` tool (see `docs://filters`).
  - `limit`: stored as the default row cap when the alias is executed via `alias_run`.
- **Output**: `{ "created": "<alias>" }`
- Returns `ALIAS_ALREADY_EXISTS` (`data.code`) if an alias with the same name already exists for this table.
- In multi-table mode `table` is required; omitting it returns `TABLE_REQUIRED`.
- Annotations: `readOnlyHint=false`, `destructiveHint=false`, `idempotentHint=false`

## `alias_list`
List all named query aliases registered for a table. Each record includes `name`, `filter`, `default_limit`, and `description`. Aliases are scoped exclusively to the named `table`.
- **Input**: `{ "table": "<name>" }` (table optional in legacy mode)
- **Output**: array of alias records `[{ "name": "...", "filter": {...}, "default_limit": <number>|null, "description": "<str>"|null }, ...]`
- In multi-table mode `table` is required; omitting it returns `TABLE_REQUIRED`.
- Annotations: `readOnlyHint=true`, `idempotentHint=true`

## `alias_run`
Execute a named query alias and return matching rows. The stored filter is replayed against `Store::list`. Supply a runtime `limit` to override the alias's stored `default_limit`, and/or an `offset` for pagination. The alias is scoped exclusively to the named `table`.
- **Input**: `{ "table": "<name>", "name": "<alias>", "limit": <u32 optional>, "offset": <u32 optional> }`
  - `limit`: overrides the stored `default_limit` at execution time (Crux: runtime override is required — this is not a static macro replay).
  - `offset`: pagination offset; never stored in the alias.
- **Output**: array of records matching the alias filter (same shape as `list`)
- Returns `ALIAS_NOT_FOUND` (`data.code`) if the alias does not exist.
- In multi-table mode `table` is required; omitting it returns `TABLE_REQUIRED`.
- Annotations: `readOnlyHint=true`, `idempotentHint=true`

## `alias_delete`
Delete a named query alias from a table. The alias is scoped exclusively to the named `table`; aliases belonging to other tables are never affected.
- **Input**: `{ "table": "<name>", "name": "<alias>" }` (table optional in legacy mode)
- **Output**: `{ "deleted": "<alias>" }`
- Returns `ALIAS_NOT_FOUND` (`data.code`) if the alias does not exist.
- In multi-table mode `table` is required; omitting it returns `TABLE_REQUIRED`.
- Annotations: `readOnlyHint=false`, `destructiveHint=true`, `idempotentHint=true`

## `query_aggregate`
Aggregate rows from one or more tables using `COUNT` / `SUM` / `AVG` / `MIN` / `MAX` / `GROUP BY` (with optional `HAVING`). Single source uses one table; Multi source UNION ALLs across tables before aggregation via SQLite `ATTACH DATABASE`.
- **Input**: `{ "sources": { "kind": "single"|"multi", "value": "<name>"|["<n1>","<n2>",...] }, "filter": <ListFilter optional>, "aggregator": { "kind": "count"|"sum"|"avg"|"min"|"max"|"group_by", ... } }`
  - `sources.kind="single"` → `value` is a table name string
  - `sources.kind="multi"` → `value` is an array of table names (ATTACH limit 10)
  - `aggregator.kind="sum"|"avg"|"min"|"max"` requires `"field": "<name>"`
  - `aggregator.kind="group_by"` requires `"by_field": "<name>"` plus optional `"having": <ListFilter>` and `"inner": <AliasAggregator>`
- **Output**: externally-tagged result `{ "kind": "count"|"value"|"groups"|"rows", "value": ... }`
  - `count` → `i64`
  - `value` → JSON number / null (scalar aggregate)
  - `groups` → `[{ "key": ..., "count": N, "value": <inner result>? }, ...]`
- Returns `AGGREGATOR_ERROR` (`data.code`) for empty sources, ATTACH-limit exceeded, nested `GroupBy`, or non-UTF-8 db path.
- Returns `VALIDATION_ERROR` for field / identifier rejections, and `TABLE_NOT_FOUND` when any source name is not mounted.
- Read-only — does not modify any data.
- Annotations: `readOnlyHint=true`, `idempotentHint=true`
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
| `ALIAS_NOT_FOUND` | 404 | The named alias does not exist in the table's `_aliases` storage. Also includes `"name": "<alias>"` in `data`. |
| `ALIAS_ALREADY_EXISTS` | 409 | `alias_create` was called but an alias with the same name already exists for this table. Also includes `"name": "<alias>"` in `data`. |

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

/// Guide for constructing `filter` objects used by the `list` tool, `alias_create`, and `row_materialize`.
pub const FILTERS_DOC: &str = r#"# mini-app-mcp — Filter Construction Guide

Filters are JSON objects passed to the `list` tool's `filter` argument, stored in `alias_create`,
and used in `row_materialize`'s `by_filter` selector.  All filter types are schema-validated
before any SQL executes.

## `Eq` — exact equality

Match rows where `field` equals `value`.

```json
{ "type": "eq", "field": "status", "value": "open" }
```

- `field`: must be a field name defined in `schema.yaml`.
- `value`: must match the field's declared JSON type (`string`, `number`, `boolean`).

## `In` — membership test

Match rows where `field` equals any element in `values`.

```json
{ "type": "in", "field": "status", "values": ["open", "pending"] }
```

- `values`: non-empty array; all elements must match the field's declared type.

## `Like` — SQL LIKE pattern match

Match rows where `field` matches a SQL LIKE pattern.  Only `string`-typed fields are accepted.

```json
{ "type": "like", "field": "title", "pattern": "%migration%" }
```

- `%`: matches any sequence of characters.
- `_`: matches any single character.
- Restricted to `string`-typed fields; non-string fields are rejected with `VALIDATION_ERROR`.

## `ArrayContains` — array element containment

Match rows where the JSON array at `field` contains at least one element equal to `value`.

```json
{ "type": "array_contains", "field": "tags", "value": "rust" }
```

- `field`: must be an `array`-typed field in `schema.yaml`.
- `value`: must be a scalar (string, number, or boolean); `null`, objects, and arrays are rejected.
- Restricted to `array`-typed fields; non-array fields are rejected with `VALIDATION_ERROR`.

## `ArrayNotContains` — array element exclusion

Match rows where the JSON array at `field` does **not** contain any element equal to `value`.

```json
{ "type": "array_not_contains", "field": "read_by", "value": "mia" }
```

- `field`: must be an `array`-typed field in `schema.yaml`.
- `value`: must be a scalar (string, number, or boolean); `null`, objects, and arrays are rejected.
- Restricted to `array`-typed fields; non-array fields are rejected with `VALIDATION_ERROR`.
- **Note on NULL/non-array rows**: if `field` is `NULL` or not a valid JSON array in a given
  row, SQLite's `json_each` returns zero rows → `NOT EXISTS` is `true` → those rows **are**
  matched.  Add an `Eq`/`In` guard if you need to exclude rows with missing/non-array values.

## `Or` — logical OR

Match rows that satisfy **at least one** of the nested filters.

```json
{
  "type": "or",
  "filters": [
    { "type": "eq", "field": "status", "value": "open" },
    { "type": "eq", "field": "status", "value": "pending" }
  ]
}
```

## `And` — logical AND

Match rows that satisfy **all** of the nested filters.

```json
{
  "type": "and",
  "filters": [
    { "type": "eq", "field": "project", "value": "my-project" },
    { "type": "in", "field": "status", "values": ["open", "pending"] }
  ]
}
```

## Composition example

```json
{
  "type": "and",
  "filters": [
    { "type": "eq", "field": "project", "value": "my-project" },
    { "type": "array_contains", "field": "tags", "value": "urgent" },
    {
      "type": "or",
      "filters": [
        { "type": "eq", "field": "status", "value": "open" },
        { "type": "like", "field": "title", "pattern": "%urgent%" }
      ]
    }
  ]
}
```

## Storing filters as aliases

Use `alias_create` to store a filter for reuse:

```json
{
  "tool": "alias_create",
  "table": "issues",
  "name": "recent_open",
  "filter": { "type": "eq", "field": "status", "value": "open" },
  "limit": 20,
  "description": "Open issues, capped at 20"
}
```

Then execute it with `alias_run`, optionally overriding `limit` and `offset` at runtime:

```json
{ "tool": "alias_run", "table": "issues", "name": "recent_open", "limit": 50, "offset": 0 }
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
    fn quickstart_starts_with_heading() {
        assert!(
            QUICKSTART.starts_with("# mini-app-mcp"),
            "QUICKSTART must start with '# mini-app-mcp'"
        );
    }

    #[test]
    fn quickstart_documents_mode_detection() {
        assert!(
            QUICKSTART.contains("Multi-table mode"),
            "QUICKSTART must document Multi-table mode so agents know when `table` is required"
        );
        assert!(
            QUICKSTART.contains("Legacy single-table mode"),
            "QUICKSTART must document Legacy single-table mode"
        );
        assert!(
            QUICKSTART.contains("TABLE_REQUIRED"),
            "QUICKSTART must document the TABLE_REQUIRED error agents use to detect multi-table mode"
        );
    }

    #[test]
    fn quickstart_points_to_other_docs() {
        for uri in &["docs://tools", "docs://errors", "docs://filters"] {
            assert!(
                QUICKSTART.contains(uri),
                "QUICKSTART must point to '{uri}' so agents can discover the other documentation resources"
            );
        }
    }

    #[test]
    fn tools_doc_contains_all_seventeen_tools() {
        for tool in &[
            "info",
            "create",
            "get",
            "list",
            "update",
            "delete",
            "reload",
            "data_snapshot",
            "row_materialize",
            "schema_create",
            "schema_update",
            "schema_delete",
            "schema_batch",
            "alias_create",
            "alias_list",
            "alias_run",
            "alias_delete",
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
            "ALIAS_NOT_FOUND",
            "ALIAS_ALREADY_EXISTS",
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

    #[test]
    fn filters_doc_contains_all_filter_types() {
        for filter_type in &[
            "Eq",
            "In",
            "Like",
            "ArrayContains",
            "ArrayNotContains",
            "Or",
            "And",
        ] {
            assert!(
                FILTERS_DOC.contains(&format!("## `{filter_type}`")),
                "FILTERS_DOC must contain section for '{filter_type}'"
            );
        }
    }

    #[test]
    fn filters_doc_mentions_alias_create() {
        assert!(
            FILTERS_DOC.contains("alias_create"),
            "FILTERS_DOC must mention alias_create"
        );
    }
}
