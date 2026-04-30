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

## License

MIT OR Apache-2.0
