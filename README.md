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
fields:
  - name: title
    type: string
    required: true
  - name: state
    type: string
    required: false
  - name: tags
    type: array
    required: false
```

Supported types: `string`, `number`, `boolean`, `array`, `object`.

## Configuration

### Multi-table mode (recommended)

| Environment variable | Default | Description |
|---|---|---|
| `MINI_APP_USER_DIR` | `~/.mini-app/` | Base directory for User-scope tables. Each subdirectory is treated as a table name and must contain `schema.yaml` and `<table>.db`. |
| `MINI_APP_PROJECT_DIR` | `./.mini-app/` | Project-scope override directory. A table present here fully replaces the User-scope definition of the same name. |

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
| `get` | Retrieves a single row by `id` |
| `list` | Returns rows with optional `limit` / `offset` pagination |
| `update` | Replaces the `data` of an existing row by `id` |
| `delete` | Removes a row by `id` |
| `reload` | Re-scan `MINI_APP_USER_DIR` / `MINI_APP_PROJECT_DIR` and atomically replace the table registry. Legacy `MINI_APP_SCHEMA` + `MINI_APP_DB` are re-applied if set. Returns `{ mounted, added, removed }`. Limitations: no file watcher (explicit invocation only); whole-registry replace (no per-table partial reload); no schema migration for existing rows. |

## MCP resources

In addition to the 7 tools above, the server exposes 6 read-only **Resources** addressable by URI. Resources are intended for agents that want to fetch the schema definition or reference documentation without invoking a mutating tool.

| URI | MIME | Content |
|---|---|---|
| `schema://yaml` | `application/yaml` | Raw `schema.yaml` file content (read from disk on each request) |
| `schema://json` | `application/json` | Parsed `SchemaConfig` as JSON (same shape the `info` tool returns) |
| `schema://json-schema` | `application/schema+json` | JSON Schema (draft-07) derived from the schema's fields. Use this to validate `data` arguments before calling `create` / `update` |
| `docs://readme` | `text/markdown` | This README, compiled into the binary |
| `docs://tools` | `text/markdown` | Cheat sheet of the 7 MCP tools and their input shapes |
| `docs://errors` | `text/markdown` | Reference table of error codes returned by the server |

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

### Ignoring dump files in git

Add `.mini-app/` (or your custom `dump.dir`) to `.gitignore` if you do not want dump files tracked by version control:

```
.mini-app/
```

## Storage notes

SQLite databases are opened in WAL journal mode for safe concurrent access during `reload`. Sidecar files `<db>.db-wal` and `<db>.db-shm` are created next to each `.db` file — these are managed by SQLite and should not be deleted manually.

## License

MIT OR Apache-2.0
