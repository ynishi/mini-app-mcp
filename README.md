# mini-app-mcp

Agent-First CRUD store MCP server — 1 daemon per table, `schema.yaml` driven, SQLite backend.

## What it does

`mini-app-mcp` is a lightweight MCP server where each running daemon owns exactly one SQLite table. The table shape is defined entirely by a `schema.yaml` file; no migrations, no REST API, no GUI. CRUD is exposed exclusively as MCP tools, making it a natural backend for agents that need structured persistent storage.

## Design principles

- **1 daemon = 1 table** — start one process per data type (issues, tasks, notes, …).
- **`schema.yaml` as sole schema authority** — field names, types, and required constraints are read from the YAML file at startup. No field is hard-coded in application code.
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

| Environment variable | Default | Description |
|---|---|---|
| `MINI_APP_SCHEMA` | `./schema.yaml` | Path to the schema definition file |
| `MINI_APP_DB` | *(none — must be set)* | Path to the SQLite database file |

Both variables can also be placed in a `.mini-app-mcp.env` file in the working directory.

## MCP tools

| Tool | Description |
|---|---|
| `info` | Returns the parsed schema (table name, field definitions) as JSON |
| `create` | Inserts a new row; validates the `data` object against the schema |
| `get` | Retrieves a single row by `id` |
| `list` | Returns rows with optional `limit` / `offset` pagination |
| `update` | Replaces the `data` of an existing row by `id` |
| `delete` | Removes a row by `id` |

## MCP resources

In addition to the 6 tools above, the server exposes 6 read-only **Resources** addressable by URI. Resources are intended for agents that want to fetch the schema definition or reference documentation without invoking a mutating tool.

| URI | MIME | Content |
|---|---|---|
| `schema://yaml` | `application/yaml` | Raw `schema.yaml` file content (read from disk on each request) |
| `schema://json` | `application/json` | Parsed `SchemaConfig` as JSON (same shape the `info` tool returns) |
| `schema://json-schema` | `application/schema+json` | JSON Schema (draft-07) derived from the schema's fields. Use this to validate `data` arguments before calling `create` / `update` |
| `docs://readme` | `text/markdown` | This README, compiled into the binary |
| `docs://tools` | `text/markdown` | Cheat sheet of the 6 MCP tools and their input shapes |
| `docs://errors` | `text/markdown` | Reference table of error codes returned by the server |

The `info` tool and `schema://json` resource return equivalent content but serve different purposes: `info` is a callable tool (good for one-off introspection in a conversation), while resources are URI-addressable and can be subscribed to or cached by the client.

## Usage

Start the server via the `--mcp` flag (required; the binary has no other entry point):

```sh
MINI_APP_SCHEMA=./schema.yaml MINI_APP_DB=./issues.db mini-app-mcp --mcp
```

Or configure via `.mini-app-mcp.env`:

```
MINI_APP_SCHEMA=./schema.yaml
MINI_APP_DB=./issues.db
```

Then register it as an MCP server in `.mcp.json`:

```json
{
  "mcpServers": {
    "issues": {
      "command": "mini-app-mcp",
      "args": ["--mcp"]
    }
  }
}
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

## License

MIT OR Apache-2.0
