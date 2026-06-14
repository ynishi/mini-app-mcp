# mini-app-mcp

Agent-First CRUD store MCP server — `schema.yaml` driven, SQLite backend, multi-table in a single daemon.

## What it does

`mini-app-mcp` is a lightweight MCP server that manages one or more SQLite tables in a single running process. The shape of each table is defined entirely by a `schema.yaml` file; no migrations, no REST API, no GUI. CRUD is exposed exclusively as MCP tools, making it a natural backend for agents that need structured persistent storage.

## Design principles

- **`schema.yaml` as sole schema authority** — field names, types, and required constraints are read from YAML at startup. No field is hard-coded in application code.
- **Multi-table in one daemon** — a single server process discovers and mounts all tables found under the configured User and Project scope directories. A dedicated legacy mode (`MINI_APP_SCHEMA` + `MINI_APP_DB`) preserves the original single-table behaviour.
- **MCP-only entry point** — there is no HTTP/REST/CLI CRUD interface. All reads and writes go through MCP tools.
- **Structured JSON errors** — every error response carries a machine-readable `code` field so agents can handle failures programmatically.

## schema.yaml format

```yaml
table: issues
title: "Issue tracker"
description: "Tracks bugs and feature requests for a project."
fields:
  - name: title
    type: string
    required: true
    description: "Short one-line summary of the issue."
  - name: state
    type: string
    required: false
  - name: tags
    type: array
    required: false
```

The optional `title` and `description` keys at the table level provide human- and AI-readable metadata about the table. Each field entry may also carry an optional `description` string. All three keys follow the OpenAPI 3.1 / JSON Schema 2020-12 naming convention and are included in `info` tool output and the `schema://json` resource.

Supported types: `string`, `number`, `boolean`, `array`, `object`.

## Configuration

### Multi-table mode (recommended)

| Environment variable | Default | Description |
|---|---|---|
| `MINI_APP_USER_DIR` | `~/.mini-app/` | Base directory for User-scope tables. Each subdirectory is treated as a table name and must contain `schema.yaml` and `<table>.db`. |
| `MINI_APP_PROJECT_DIR` | `./.mini-app/` | Project-scope override directory. A table present here fully replaces the User-scope definition of the same name. |
| `MINI_APP_BACKUP_RETENTION` | `10` | Maximum number of backup copies (YAML + DB snapshot pairs) to retain per table under `_backup/`. Older copies beyond this limit are deleted immediately after each backup write. |
| `MINI_APP_SNAPSHOT_RETENTION` | `10` | Maximum number of snapshot generations to retain per table under `_snapshots/`. Strictly separate from `MINI_APP_BACKUP_RETENTION`; purges only `_snapshots/` files and never touches `_backup/`. |

Tables are discovered at startup by scanning both directories. Project-scope definitions take precedence over User-scope definitions for the same table name.

### Legacy single-table mode

| Environment variable | Default | Description |
|---|---|---|
| `MINI_APP_SCHEMA` | `./schema.yaml` | Path to the schema definition file |
| `MINI_APP_DB` | *(none — must be set)* | Path to the SQLite database file |

When `MINI_APP_SCHEMA` and `MINI_APP_DB` are set the server starts in legacy mode, mounting exactly one table. The `table` argument on all tools may be omitted in this mode.

All variables can also be placed in a `.mini-app-mcp.env` file in the working directory.

## MCP tools

All tools accept an optional `table` argument that selects the target table. In multi-table mode the argument is required; omitting it returns error code `TABLE_REQUIRED`. Supplying an unknown table name returns error code `TABLE_NOT_FOUND`. In legacy single-table mode the argument may be omitted.

| Tool | Description |
|---|---|
| `info` | Returns the parsed schema (table name, field definitions) as JSON |
| `create` | Inserts a new row; validates the `data` object against the schema |
| `get` | Retrieves a single row by `id`. If `id` is shorter than 36 characters it is treated as a UUID prefix: zero matches return `NOT_FOUND`; two or more matches return `AMBIGUOUS_ID` with a candidate list. A full 36-character UUID always uses the exact-match path. Accepts an optional `fields` selector to project the returned `data` object to a named subset of schema fields. |
| `list` | Returns rows with optional `limit` / `offset` pagination. Accepts an optional `fields` selector to project the returned `data` objects to a named subset of schema fields. |
| `update` | Updates an existing row by `id`. If `id` is shorter than 36 characters it is treated as a UUID prefix (see `get` for resolution rules). Default mode is **merge** (RFC 7396): absent fields are preserved from the stored row, `null` values delete optional fields or raise a Validation error for required ones. Pass `"mode": "replace"` for full replacement (pre-0.9 behaviour). |
| `delete` | Removes a row by `id`. If `id` is shorter than 36 characters it is treated as a UUID prefix (see `get` for resolution rules). |
| `reload` | Re-scan `MINI_APP_USER_DIR` / `MINI_APP_PROJECT_DIR` and atomically replace the table registry. Legacy `MINI_APP_SCHEMA` + `MINI_APP_DB` are re-applied if set. Returns `{ mounted, added, removed }`. Limitations: no file watcher (explicit invocation only); whole-registry replace (no per-table partial reload); no schema migration for existing rows; concurrent `reload` calls are last-write-wins. |
| `schema_create` | Create a new `schema.yaml` under the specified `scope` (`project` or `user`) and register the table live. Pass `dry_run: true` to preview without writing. Fails with `SCHEMA_EXISTS` if the table already exists. |
| `schema_update` | Replace an existing table's `schema.yaml` with a new definition (full overwrite). Backs up the previous YAML and a SQLite snapshot to `_backup/` before writing. Pass `dry_run: true` to preview field changes without touching disk. |
| `schema_delete` | Remove a table's `schema.yaml` (moved to `_backup/`) and unregister it from the live registry. **Does not alter or drop the SQLite table** — DDL changes remain the operator's responsibility. Pass `dry_run: true` to preview. |
| `schema_batch` | Execute an array of `ops[]` atomically under a single SQLite SAVEPOINT. Any op failure rolls back all preceding ops, leaving YAML and DB untouched. All ops must target the same table. Returns per-op results or a `BATCH_ABORTED` error with the index of the failing op. |
| `data_snapshot` | Create a point-in-time SQLite snapshot of one or all mounted tables using the SQLite hot backup API. Snapshots are written to `<scope_root>/_snapshots/<table>.<unix_secs>.db`. Pass `table` and/or `scope` to limit the target set; omit both to snapshot all mounted tables. Pass `dry_run: true` to preview the operation (target tables, row counts, would-purge count) without creating any files. Retention is controlled by `MINI_APP_SNAPSHOT_RETENTION` (default 10), independent of `MINI_APP_BACKUP_RETENTION`. |
| `row_materialize` | Write one or more rows to arbitrary absolute paths on the local filesystem. Select rows by `id` or by a `ListFilter` expression. Choose output format (`raw`, `markdown`, `json`, `yaml`), field projection (`All` or a named subset), and whether to write one file per row (`concat=false`, default) or concatenate all rows into a single file (`concat=true`). Returns `{ count, files: [{path, bytes, sha256, row_id}] }` — every file entry includes a SHA-256 hex digest of the written bytes. Pass `dry_run: true` to compute results without writing. |
| `alias_create` | Register a named query alias for a table. Accepts `name`, either `filter` (a `ListFilter` expression) or `filter_template` (a MiniJinja template string — mutually exclusive with `filter`), optional `params_schema` (array of parameter name strings for a templated alias), optional `default_limit`, and optional `description`. Alias names are unique per table; duplicate names return `ALIAS_ALREADY_EXISTS`. Aliases are scoped per table and stored in the table's own SQLite database. |
| `alias_list` | Return all aliases registered for a table as a JSON array of `{ name, filter, default_limit, description, params_schema }` objects. |
| `alias_run` | Execute a stored alias by name. Accepts optional runtime `limit` and `offset` that override the stored `default_limit` at call time. For parameterized aliases (those created with `filter_template`), also accepts a `params` object whose key-value pairs are injected into the template. Accepts an optional `fields` selector to project the returned `data` objects to a named subset of schema fields. If `params_schema` is set and `params` is omitted, returns `ALIAS_PARAMS_REQUIRED`. Template render failures return `ALIAS_TEMPLATE_ERROR`. Returns the same shape as the `list` tool. Returns `ALIAS_NOT_FOUND` for an unknown name. |
| `alias_delete` | Delete a named alias for a table. Returns `ALIAS_NOT_FOUND` if the alias does not exist. |

## MCP resources

In addition to the 17 tools above, the server exposes 7 read-only **Resources** addressable by URI. Resources are intended for agents that want to fetch the schema definition or reference documentation without invoking a mutating tool.

| URI | MIME | Content |
|---|---|---|
| `schema://yaml` | `application/yaml` | Raw `schema.yaml` file content (read from disk on each request) |
| `schema://json` | `application/json` | Parsed `SchemaConfig` as JSON (same shape the `info` tool returns) |
| `schema://json-schema` | `application/schema+json` | JSON Schema (draft-07) derived from the schema's fields. Use this to validate `data` arguments before calling `create` / `update` |
| `docs://quickstart` | `text/markdown` | Agent quickstart (mode detection + first-call recipe + pointers to the other `docs://` resources), compiled into the binary. Distinct from this human-facing README — read this resource from inside the MCP session |
| `docs://tools` | `text/markdown` | Cheat sheet of all 17 MCP tools and their input shapes |
| `docs://errors` | `text/markdown` | Reference table of error codes returned by the server |
| `docs://filters` | `text/markdown` | Guide for constructing filter objects (Eq/In/Like/Or/And) used by `list`, `alias_create`, and `row_materialize` |

The `info` tool and `schema://json` resource return equivalent content but serve different purposes: `info` is a callable tool (good for one-off introspection in a conversation), while resources are URI-addressable and can be subscribed to or cached by the client.

## Usage

Start the server via the `--mcp` flag (required; the binary has no other entry point).

### Multi-table mode

Place each table's `schema.yaml` and `<table>.db` under `~/.mini-app/<table>/` (User scope) or `./.mini-app/<table>/` (Project scope), then start without any extra environment variables:

```sh
mini-app-mcp --mcp
```

Register it once in `.mcp.json` to serve all mounted tables:

```json
{
  "mcpServers": {
    "mini-app": {
      "command": "mini-app-mcp",
      "args": ["--mcp"]
    }
  }
}
```

### Legacy single-table mode

```sh
MINI_APP_SCHEMA=./schema.yaml MINI_APP_DB=./issues.db mini-app-mcp --mcp
```

Or configure via `.mini-app-mcp.env`:

```
MINI_APP_SCHEMA=./schema.yaml
MINI_APP_DB=./issues.db
```

## Dump / file materialization

`mini-app-mcp` can write each created or updated row to disk as a Markdown file.  This is useful for agents that read context from files, for version-controlling records with `git`, or for quick human inspection.

### How it works

After every successful `create` or `update` call the server writes (or overwrites) a file:

```
<dump-dir>/<id>.md
```

The file format is:

```markdown
# <title-field value>

<body-field value>
```

`delete` does **not** remove the dump file by default (the record stays on disk as an archive).

### Enabling dump in schema.yaml

Add a `dump:` section to your `schema.yaml`:

```yaml
table: issues
fields:
  - name: title
    type: string
    required: true
  - name: body
    type: string
    required: false
dump:
  dir: ./issues          # optional; default: <cwd>/.mini-app/<table>/
  title_field: title     # optional; default: title
  body_field: body       # optional; default: body
  sync: write-only       # optional; default: write-only
```

| Key | Default | Description |
|---|---|---|
| `dump.dir` | `<cwd>/.mini-app/<table>/` | Directory where `<id>.md` files are written. Relative paths are resolved from the server's working directory. |
| `dump.title_field` | `title` | Field name in the stored JSON row to use as the Markdown heading. |
| `dump.body_field` | `body` | Field name in the stored JSON row to use as the Markdown body. |
| `dump.sync` | `write-only` | Sync direction. Only `write-only` is implemented. Setting `bidirectional` is accepted without error but logs a warning and behaves as `write-only`. |

## Schema management

`mini-app-mcp` exposes four tools for managing table schemas at runtime without restarting the server.

### Creating a table

```
schema_create(scope="project", table="notes", title="Notes", description="Personal notes.", fields=[...])
```

The tool writes `<scope_dir>/notes/schema.yaml` and immediately registers the new table in the live registry. Calling it again for the same table name returns `SCHEMA_EXISTS`. The optional `title` and `description` arguments are written to the YAML file and are immediately visible via `info` or `schema://json`. Individual field entries may also include a `description` string.

### Updating a schema

```
schema_update(scope="project", table="notes", title="Notes", description="Updated notes.", fields=[...])
```

Before overwriting, the server backs up the existing `schema.yaml` and a point-in-time SQLite snapshot to `<scope_dir>/_backup/notes.<timestamp>.yaml` and `<scope_dir>/_backup/notes.<timestamp>.db`. The live registry is refreshed after the write. No DDL migration is applied — the underlying table structure is unchanged.

### Deleting a schema

```
schema_delete(scope="project", table="notes")
```

The `schema.yaml` is moved to `_backup/` and the table is unregistered from the live registry. **The SQLite database file is not modified.** Dropping or altering the table remains the operator's responsibility.

### Atomic batch operations

```
schema_batch(ops=[
  { "op": "create", "table": "notes", "scope": "project", "fields": [...] },
  { "op": "data_insert", "table": "notes", "data": { "title": "first note" } }
])
```

All ops execute under a single SQLite SAVEPOINT. If any op fails the entire batch rolls back — YAML files are not written and the registry is not changed. All ops in one batch must target the same table.

### dry_run preview

All four schema tools accept `dry_run: true`. In dry-run mode the tool computes and returns affect counts (`rows`, `fields_added`, `fields_removed`) without writing to any YAML file, SQLite table, or backup directory.

### Backup retention

Backup files accumulate in `<scope_dir>/_backup/`. The server automatically deletes the oldest copies beyond the retention limit (default 10 pairs per table). Override the limit with `MINI_APP_BACKUP_RETENTION`.

### Data snapshots

`data_snapshot` creates a standalone SQLite snapshot of one or more mounted tables without touching any YAML file or altering the schema:

```
data_snapshot(table="notes", scope="project")
data_snapshot()                       # snapshot all mounted tables
data_snapshot(dry_run=true)           # preview without writing
```

Snapshots are written to `<scope_root>/_snapshots/<table>.<unix_secs>.db` using the SQLite hot backup API (`rusqlite::Connection::backup`), so the source database remains open and writable during the operation. The retention limit (default 10) is controlled independently via `MINI_APP_SNAPSHOT_RETENTION` and never interacts with `_backup/`.

## Row materialization

`row_materialize` exports rows from any mounted table to the local filesystem in a format your agent or toolchain can consume directly — without re-reading the database.

### Selecting rows

```
row_materialize(table="notes", selector={"type": "ById", "id": "<uuid>"}, ...)
row_materialize(table="notes", selector={"type": "ByFilter", "filter": {"type": "eq", "field": "state", "value": "done"}}, ...)
```

`ById` fetches exactly one row by primary key. `ByFilter` accepts any `ListFilter` expression (the same `eq` / `in` / `like` / `or` / `and` combinators available in `list`).

### Output formats

| `format` | Extension | Description |
|---|---|---|
| `raw` | `.txt` | Field values joined by newlines in schema order (or specified order when projecting) |
| `markdown` | `.md` | Each field rendered as a Markdown heading + value block |
| `json` | `.json` | `serde_json::to_string_pretty` — single object per row, or JSON array when `concat=true` |
| `yaml` | `.yaml` | YAML document stream — one document per row, separated by `---` |

### Destination and concat mode

```
# One file per row, written to /tmp/export/{id}.md
row_materialize(table="notes", format="markdown", dest="/tmp/export", concat=false)

# All rows concatenated into a single file
row_materialize(table="notes", format="json", dest="/tmp/notes.json", concat=true)
```

When `concat=false` (default), `dest` is treated as a directory and each row is written to `{dest}/{id}.{ext}`. The directory is created with `create_dir_all` if it does not exist.

When `concat=true`, `dest` is a file path. Raw rows are separated by `\n\n`, Markdown rows by `---\n`, and YAML rows by `---\n`. JSON uses a top-level array.

**The destination path must be absolute.** Relative paths are rejected immediately with `MATERIALIZE_DEST_RELATIVE`. No project-root sandbox is applied — any absolute path is permitted, giving agents full filesystem reach.

### Field projection

```
row_materialize(..., fields={"type": "All"})                              # default: all schema fields
row_materialize(..., fields={"type": "List", "fields": ["title", "state"]})  # named subset
```

Unknown field names return `MATERIALIZE_FIELD_UNKNOWN` before any file is written.

### Integrity: SHA-256 in every response

Every entry in `files[]` includes a `sha256` field — a 64-character hex digest of the exact bytes written. Agents can use this for idempotency checks or to verify content after transfer without re-reading the file.

```json
{
  "count": 2,
  "files": [
    { "path": "/tmp/export/abc.md", "bytes": 142, "sha256": "e3b0c44…", "row_id": "abc" },
    { "path": "/tmp/export/def.md", "bytes": 198, "sha256": "a87ff6…", "row_id": "def" }
  ]
}
```

`row_id` is the source row's primary key for per-row files and `null` for concatenated output.

### Dry run

```
row_materialize(..., dry_run=true)
```

Dry-run mode runs all validation, projection, serialization, and SHA-256 computation but skips `std::fs::write`. The response shape is identical to a real write — path, byte count, and digest are all populated as "would-be" values.

### Write mode

```
row_materialize(..., write_mode="Overwrite")   # default: overwrite existing files
row_materialize(..., write_mode="Error")       # fail with MATERIALIZE_DEST_INVALID if dest exists
```

### Ignoring dump files in git

Add `.mini-app/` (or your custom `dump.dir`) to `.gitignore` if you do not want dump files tracked by version control:

```
.mini-app/
```

## Relation graph usage

`mini-app-mcp` is table-agnostic, but a common pattern is to use a single table as a **relation graph layer** for agents that need typed edges between nodes (e.g. persona-AI memory: `sister_of`, `member_of_studio`, `mother_of`).

The reference schema lives at `examples/schemas/relations.schema.yaml`:

| Field | Type | Required | Notes |
|---|---|---|---|
| `from` | string | yes | Source node id |
| `to` | string | yes | Target node id |
| `type` | string | yes | Edge label (`sister_of`, `member_of_studio`, ...) |
| `ts` | number | yes | Unix seconds when asserted |
| `strength` | number | no | Edge weight in `[0.0, 1.0]`, defaults to `1.0` |
| `metadata` | object | no | Free-form attributes |
| `source_entry_id` | string | no | Foreign reference to a journal / log entry |

### Bulk insert (N edges atomically)

Use `schema_batch` with `query` ops to register N edges in one SAVEPOINT:

```json
{
  "ops": [
    {
      "op": "query",
      "sql": "INSERT INTO rows (id, data, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
      "params": ["<uuid>", "{\"from\":\"persona-x\",\"to\":\"a\",\"type\":\"sister_of\",\"ts\":1779330000,\"strength\":1.0}", "1779330000", "1779330000"]
    }
  ]
}
```

Any op failure rolls back the entire batch.

### Bulk replace (edge set full replacement)

Use the dedicated `replace` op to swap an edge set in one atomic SAVEPOINT. The server handles UUID generation, timestamps, and JSON serialization; the caller supplies the `match` scope and `items` list only.

```json
{
  "ops": [
    {
      "op": "replace",
      "table": "relations",
      "match": {"from": "persona-x", "type": "sister_of"},
      "items": [
        {"from": "persona-x", "to": "a", "type": "sister_of", "ts": 1779330000, "strength": 1.0},
        {"from": "persona-x", "to": "b", "type": "sister_of", "ts": 1779330000, "strength": 1.0},
        {"from": "persona-x", "to": "c", "type": "sister_of", "ts": 1779330000, "strength": 0.8}
      ]
    }
  ]
}
```

The `replace` op runs DELETE WHERE (match) + N INSERTs inside a single SAVEPOINT. Both phases roll back together on any failure. The response includes per-op affects:

```json
{
  "committed": true,
  "ops_executed": 1,
  "affects": {"deleted": 5, "inserted": 3}
}
```

**Semantics**:

- Each key in `match` becomes a separate `json_extract(data, '$.key') = value` predicate joined by AND. Match value must be a scalar (string/number/bool/null).
- **Empty `match` (`{}`) is rejected with VALIDATION_ERROR** to prevent accidental full-table wipes.
- Each item in `items` is validated against the table's schema before any SQL runs.
- The `query` op (raw SQL escape hatch shown in [Bulk insert](#bulk-insert-n-edges-atomically)) remains available for cases the `replace` op does not cover.

### Listing edges (sub-1000 scope)

The `list` tool returns rows in `created_at DESC` order with `limit` capped at 1000. An optional `filter` argument enables server-side row filtering over schema-validated fields.

#### Filter variants

| type | description |
|---|---|
| `eq` | Equality match: `{"type": "eq", "field": "...", "value": ...}` |
| `in` | Set membership: `{"type": "in", "field": "...", "values": [...]}` |
| `like` | Partial-match on string fields: `{"type": "like", "field": "...", "pattern": "..."}`. `%` matches any substring; `_` matches any single character. Restricted to `string`-typed fields. |
| `or` | OR composition: `{"type": "or", "filters": [...]}` |
| `and` | AND composition: `{"type": "and", "filters": [...]}` |

`or` and `and` accept `Vec<ListFilter>` recursively, enabling nested compositions to arbitrary depth.

**Constraints**: field names must be registered in the table's `schema.yaml` (unknown fields return a `VALIDATION_ERROR`). Values are type-checked against the schema field's declared type — the same typed scalar validation applied to `BatchOp::Replace` match values. Omitting `filter` returns all rows within `limit`/`offset` (full backward compatibility).

#### Mailbox-style OR filter example

Fetch messages addressed to "alice" or "broadcast" in a single call:

```json
{
  "filter": {
    "type": "or",
    "filters": [
      {"type": "eq", "field": "to", "value": "alice"},
      {"type": "eq", "field": "to", "value": "broadcast"}
    ]
  }
}
```

For small graphs where client-side filtering is sufficient:

```python
edges = mini_app.list(table="relations", limit=1000)
sisters_of_x = [
    e for e in edges
    if e["data"]["from"] == "persona-x" and e["data"]["type"] == "sister_of"
]
```

## Update semantics: merge vs replace

The `update` tool supports two modes controlled by the optional `"mode"` field.

### Merge mode (default)

```json
update(table="notes", id="<uuid>", data={"state": "done"})
// or equivalently:
update(table="notes", id="<uuid>", data={"state": "done"}, mode="merge")
```

Merge mode follows [RFC 7396 (JSON Merge Patch)](https://datatracker.ietf.org/doc/html/rfc7396) shallow semantics:

- **Absent fields are preserved** — keys not present in `data` keep their stored values.
- **Non-null values overwrite** — a key present in `data` with a non-null value replaces the stored value for that key. Nested objects are replaced wholesale (no deep merge).
- **`null` deletes an optional field** — if a key's value is `null` and the field is `required: false` in the schema, the field is removed from the stored row.
- **`null` on a required field is a Validation error** — if a key's value is `null` and the field is `required: true`, the call fails with `VALIDATION_ERROR` before any write occurs.
- **Full schema validation runs on the merged result** — after merging, the complete merged object is validated against the schema. Any constraint violation (missing required field, type mismatch) returns a Validation error and the row is not updated.

### Replace mode

```json
update(table="notes", id="<uuid>", data={...}, mode="replace")
```

Replace mode performs a full replacement: `data` is validated against the schema and then written as-is, completely overwriting the stored row. This is identical to the behaviour of `update` before version 0.9. Callers that relied on the old default and send partial `data` objects should switch to `mode="replace"` to restore the original behaviour.

### Choosing a mode

| Mode | When to use |
|---|---|
| `merge` (default) | Partial updates — only send the fields you want to change. Existing fields you omit are untouched. |
| `replace` | Full rewrites — you supply the complete intended state of the row. Omitted fields are deleted. |

## Field projection

`list`, `get`, and `alias_run` all accept an optional `fields` argument that limits which schema fields appear in the returned `data` object. `id`, `created_at`, and `updated_at` are always included in the response envelope regardless of the selector.

### Selector shapes

```json
{"mode": "all"}                              // default: return all schema fields
{"mode": "list", "fields": ["title", "state"]}   // return only the named fields
```

Omitting `fields` is fully backward-compatible — existing callers receive complete rows without any change.

### Usage examples

```
list(table="issues", fields={"mode": "list", "fields": ["title", "state"]})
get(table="issues", id="<uuid>", fields={"mode": "list", "fields": ["title"]})
alias_run(table="issues", name="recent_open", fields={"mode": "list", "fields": ["title", "state"]})
```

### Validation

Unknown field names are rejected with `VALIDATION_ERROR` before any query executes. Field name validation consults `schema.yaml`'s canonical field definitions — not the actual keys present in stored rows — so projection errors are caught reliably even when the field is simply absent from a particular row.

## Query aliases

Query aliases let you save a `ListFilter` expression (or a MiniJinja filter template) under a short name and replay it — with optional per-call `limit` / `offset` overrides — without repeating the filter JSON every time.

### Creating a static alias

```
alias_create(
  table="notes",
  name="recent_open",
  filter={"type": "eq", "field": "state", "value": "open"},
  default_limit=20,
  description="Open notes, newest first"
)
```

The alias is stored in the table's own SQLite database under `_aliases`. Alias names are unique per table; calling `alias_create` again with the same name returns `ALIAS_ALREADY_EXISTS`.

### Creating a parameterized alias (filter template)

Parameterized aliases use a [MiniJinja](https://docs.rs/minijinja/) template string instead of a fixed filter. At run time the caller supplies parameter values that are rendered into the template before execution.

```
alias_create(
  table="issues",
  name="by_state",
  filter_template='{"type": "eq", "field": "state", "value": "{{ state }}"}',
  params_schema=["state"],
  default_limit=50,
  description="Filter issues by state (parameterized)"
)
```

`filter_template` and `filter` are mutually exclusive — exactly one must be supplied. `params_schema` is an optional array of parameter names; supply it to document which keys `alias_run` expects in its `params` object.

### Running an alias

**Static alias** (no parameters):

```
alias_run(table="notes", name="recent_open")
# → uses stored default_limit=20

alias_run(table="notes", name="recent_open", limit=5)
# → runtime limit overrides the stored default_limit

alias_run(table="notes", name="recent_open", limit=10, offset=20)
# → pagination with runtime overrides
```

**Parameterized alias** (with `params`):

```
alias_run(table="issues", name="by_state", params={"state": "open"})
# → renders template → {"type": "eq", "field": "state", "value": "open"}
# → validates as ListFilter → executes Store::list

alias_run(table="issues", name="by_state", params={"state": "open"}, limit=10)
# → runtime limit override with params injection
```

`alias_run` resolves the stored filter or renders the template and passes the result to `Store::list`. If `limit` is supplied at call time it overrides `default_limit`; if omitted, `default_limit` from the alias is used. `offset` is always a runtime-only argument (not stored).

**Error cases for parameterized aliases**:

| Situation | Error code |
|---|---|
| `params_schema` is set but `params` is omitted | `ALIAS_PARAMS_REQUIRED` |
| MiniJinja render fails (bad syntax or missing variable) | `ALIAS_TEMPLATE_ERROR` |
| Rendered output is not valid JSON or not a valid `ListFilter` | `VALIDATION_ERROR` |

### Listing and deleting aliases

```
alias_list(table="notes")
# → [{"name": "recent_open", "filter": {...}, "default_limit": 20, "description": "...", "params_schema": null}]

alias_list(table="issues")
# → [{"name": "by_state", "filter": null, "default_limit": 50, "description": "...", "params_schema": ["state"]}]

alias_delete(table="notes", name="recent_open")
```

### Alias isolation

Each table's `_aliases` storage is physically separate: aliases for `notes` live in `notes.db`, aliases for `issues` live in `issues.db`. There is no global alias namespace and no way for an alias operation to read or write aliases belonging to a different table.

## Aggregation

`query_aggregate` runs `COUNT` / `SUM` / `AVG` / `MIN` / `MAX` / `GROUP BY` over one or many tables in a single tool call. Multi-table sources are joined with literal `UNION ALL` via SQLite `ATTACH DATABASE`, eliminating the N+1 round trips that result when callers fetch per-table rows and reduce client-side. The tool is read-only and idempotent.

### Shape

```jsonc
{
  "sources":   { "kind": "single", "value": "<table>" },          // or "multi" + ["t1","t2",...]
  "filter":    null,                                              // optional, same ListFilter shape as `list`
  "aggregator": { "kind": "count" }                               // or sum / avg / min / max / group_by
}
```

Result is externally-tagged so callers can dispatch on `kind`:

```jsonc
{ "kind": "count",  "value": 42 }
{ "kind": "value",  "value": 3.14 }
{ "kind": "groups", "value": [ { "key": "a", "count": 12, "value": 7.0 }, ... ] }
```

### Single-source aggregation

```jsonc
// COUNT all rows in `emo`.
{ "name": "query_aggregate", "arguments": {
    "sources":    { "kind": "single", "value": "emo" },
    "aggregator": { "kind": "count" }
}}

// SUM the `amount` field.
{ "name": "query_aggregate", "arguments": {
    "sources":    { "kind": "single", "value": "ledger" },
    "aggregator": { "kind": "sum", "field": "amount" }
}}
```

### Multi-table aggregation

`Multi` mounts every backing `.db` file via SQLite `ATTACH DATABASE` (limit 10) and composes a `UNION ALL` between per-table sub-queries before applying the outer aggregate.

```jsonc
// COUNT rows across two same-shape tables.
{ "name": "query_aggregate", "arguments": {
    "sources":    { "kind": "multi", "value": ["events_a", "events_b"] },
    "aggregator": { "kind": "count" }
}}

// GROUP BY tag with HAVING and an inner SUM(amount).
{ "name": "query_aggregate", "arguments": {
    "sources":    { "kind": "multi", "value": ["events_a", "events_b"] },
    "aggregator": {
        "kind":     "group_by",
        "by_field": "tag",
        "having":   { "type": "eq", "field": "tag", "value": "a" },
        "inner":    { "kind": "sum", "field": "amount" }
    }
}}
```

Caller is responsible for ensuring all `Multi` sources share a compatible schema; per-table field validation is a Phase 2 carry. Aggregator-specific errors (empty sources, ATTACH-limit exceeded, nested `GroupBy`, non-UTF-8 db path) return `AGGREGATOR_ERROR` (`data.code`); unknown table names return `TABLE_NOT_FOUND`; field / identifier rejections return `VALIDATION_ERROR`.

## Global Alias (Phase 2)

Phase 2 unifies aliases across tables in a single global storage
(`<project_dir>/_global.db` + `<user_dir>/_global.db`, with lookup
precedence Project → User). The `alias_create` MCP tool accepts new
optional `sources` and `aggregator` arguments so one alias can span
multiple tables and stored aggregate logic:

```jsonc
// Multi-table alias with COUNT aggregator (replays via execute_aggregate).
{ "name": "alias_create", "arguments": {
    "name":       "combined_events_count",
    "sources":    { "kind": "multi", "value": ["events_a", "events_b"] },
    "aggregator": { "kind": "count" },
    "filter":     { "type": "like", "field": "tag", "pattern": "%" }
}}

// Pattern source — resolved against the live registry's table list at
// alias_run time. Matches all currently-mounted `events_*` tables.
{ "name": "alias_create", "arguments": {
    "name":       "all_events_count",
    "sources":    { "kind": "pattern", "value": "events_*" },
    "aggregator": { "kind": "count" },
    "filter":     { "type": "like", "field": "tag", "pattern": "%" }
}}

// alias_run dispatches to execute_aggregate transparently.
{ "name": "alias_run", "arguments": { "name": "all_events_count" } }
// → { "kind": "count", "value": <sum across all events_* tables> }
```

The legacy `table` argument is still accepted and is silently
normalised to `sources = { "kind": "single", "value": "<table>" }`;
specifying both `table` and `sources` is an error.

**Migration**: existing per-table `_aliases` rows are copied into the
project-scope `_global_aliases` automatically on every
`TableRegistry::mount_from_dirs` call. The migration is lossless
(all five legacy fields preserved + `sources = Single(<table>)` filled
in) and idempotent (`INSERT OR IGNORE` skips collisions), so it is safe
to run on every server restart. Per-table `_aliases` tables are not
deleted, preserving a rollback path until a future minor release.

**Phase 2 limitations**

- `Multi` / `Pattern` source aliases require an `aggregator`; running
  them without one returns a structured error (the per-table `list`
  path cannot serve cross-table rows).
- Pattern glob supports `*` only (one or more); `?` / `[]` are reserved
  for a future revision.
- Pattern matching that resolves to zero tables returns
  `AGGREGATOR_ERROR` at `alias_run` time (early surface — does not
  defer to `ATTACH DATABASE`).
- Filter validation uses the schema of the first source table only;
  cross-table schema divergence is the caller's responsibility (same
  constraint as `query_aggregate`).
- Legacy single-table mode (no `MINI_APP_USER_DIR` / `MINI_APP_PROJECT_DIR`)
  falls back to the per-table `Store::alias_*` path; `Pattern` sources
  are rejected in this mode.

## Storage notes

SQLite databases are opened in WAL journal mode for safe concurrent access during `reload`. Sidecar files `<db>.db-wal` and `<db>.db-shm` are created next to each `.db` file — these are managed by SQLite and should not be deleted manually.

## MCP Registry

`mini-app-mcp` is listed on the [MCP Registry](https://registry.modelcontextprotocol.io). The OCI image is published to GitHub Container Registry on every tagged release.

#### Docker (no Rust toolchain required)

```json
{
  "mcpServers": {
    "mini-app": {
      "command": "docker",
      "args": [
        "run", "-i", "--rm",
        "-v", "/path/to/data:/data",
        "ghcr.io/ynishi/mini-app-mcp:latest"
      ]
    }
  }
}
```

## License

MIT OR Apache-2.0
