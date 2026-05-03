# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Multi-table support** (`mcp/registry.rs`, `mcp/server.rs`, `config.rs`) — a single `mini-app-mcp` daemon can now mount and serve multiple SQLite tables. Tables are discovered automatically from two directory layers: User scope (`~/.mini-app/<table>/`) as the base and Project scope (`{project_root}/.mini-app/<table>/`) as an override. A Project-level `schema.yaml` for a given table name fully replaces the User-level one (file-level swap, no field merging). The new `TableRegistry` struct (`mcp/registry.rs`) manages the `HashMap<String, Arc<Store>>` backing this.
- **`table` argument on all tools** (`mcp/server.rs`) — `info`, `create`, `get`, `list`, `update`, and `delete` now accept an optional `table: Option<String>` argument. In multi-table mode the argument is required; omitting it returns `MiniAppError::TableRequired` (`code: "TABLE_REQUIRED"`). Supplying an unknown name returns `MiniAppError::TableNotFound` (`code: "TABLE_NOT_FOUND"`). Tool descriptions and `server_info.instructions` have been updated to document the new semantics (§K-49 / §1-8-1).
- **New error variants** (`error.rs`) — `MiniAppError::TableNotFound { table: String }` and `MiniAppError::TableRequired`; both carry structured `code` fields through `From<MiniAppError> for McpError` so agents can handle them programmatically.
- **New environment variables** (`config.rs`) — `MINI_APP_USER_DIR` (default `~/.mini-app/`) and `MINI_APP_PROJECT_DIR` (default `./.mini-app/`) control the two directory layers. Both are optional; omitting them falls back to the defaults.

### Changed

- **`Config` struct** (`config.rs`) — extended with `user_dir: Option<PathBuf>` and `project_dir: Option<PathBuf>` alongside the existing `schema_path` / `db_path` fields. Legacy single-table mode (`MINI_APP_SCHEMA` + `MINI_APP_DB`) is fully preserved; when those variables are set the server behaves exactly as before with the specified table loaded as the default.
- **`MiniAppMcpServer`** (`mcp/server.rs`) — internal fields replaced by a `TableRegistry`. Legacy single-table startup mounts the one table under `default_table`, preserving all existing tool call semantics for callers that do not pass a `table` argument.

## [0.2.0] - 2026-05-03

### Added

- **MCP Resources** (`mcp/resources.rs`, `mcp/server.rs`) — six read-only Resources exposed alongside the existing tools: `schema://yaml` (raw schema file), `schema://json` (parsed `SchemaConfig` as JSON), `schema://json-schema` (draft-07 JSON Schema derived from `fields[]`, usable for client-side validation of `create` / `update` arguments), `docs://readme` (this README, embedded via `include_str!`), `docs://tools` (tool cheat sheet), and `docs://errors` (error code reference). `ServerCapabilities` now declares `resources` capability.

## [0.1.0] - 2026-05-01

### Added

- **Crate scaffold** — `mini-app-mcp` Rust crate with `Cargo.toml`, `lib.rs`, and `main.rs` (`--mcp` flag entry point via clap).
- **Schema parser** (`schema.rs`) — parses `schema.yaml` at startup into `SchemaConfig` / `FieldDef`; supports field types `string`, `number`, `boolean`, `array`, `object`; enforces `required` constraints. `schema.yaml` is the sole runtime source of truth for all field definitions.
- **Error types** (`error.rs`) — `MiniAppError` enum (Validation / NotFound / Schema / Storage / Io / Config variants) with `thiserror::Error` derive; `From<MiniAppError> for McpError` conversion that produces a structured JSON error object with a machine-readable `code` field on every MCP tool error path.
- **Config** (`config.rs`) — `Config::load()` reads `MINI_APP_SCHEMA` and `MINI_APP_DB` environment variables (with `.mini-app-mcp.env` dotenv fallback) to provide schema and database paths at startup.
- **SQLite store** (`store.rs`) — `Store` struct wrapping `Arc<Mutex<rusqlite::Connection>>`; fixed DDL (`rows` table with `id`, `data`, `created_at`, `updated_at` columns); CRUD methods (`create` / `get` / `list` / `update` / `delete`) bridged to async via `tokio::task::spawn_blocking`; JSON row validation against schema on write paths.
- **MCP server** (`mcp/server.rs`) — `MiniAppMcpServer` implementing `ServerHandler` with six MCP tools: `info`, `create`, `get`, `list`, `update`, `delete`; structured JSON error responses on all error paths; stdio transport via `rmcp`.
- **Dump / file-materialization** (`dump.rs`) — framework-level `on_change` / `on_delete` hooks that write each created or updated row as a Markdown file (format: `# <title>` heading, blank line, body). Enabled per-schema via the `dump:` section in `schema.yaml`. Default output path is `<cwd>/.mini-app/<table>/<id>.md`; overridable with `dump.dir`. Title and body field names are configurable via `dump.title_field` / `dump.body_field` (default `title` / `body`). File I/O runs inside `tokio::task::spawn_blocking` using `std::fs`, consistent with the existing store I/O pattern.
- **`DumpConfig` and `SyncMode`** (`dump.rs`) — new public types embedded in `SchemaConfig.dump` (optional, backward-compatible via `#[serde(default)]`). `SyncMode` accepts `write-only` (default) or `bidirectional` in YAML; bidirectional mode is reserved for a future release and emits a `tracing::warn!` at server startup when configured.
- **`SchemaConfig.dump` field** (`schema.rs`) — `Option<DumpConfig>` field added with `#[serde(default)]`; existing `schema.yaml` files without a `dump:` section continue to deserialize correctly.
- **Store dump hook integration** (`store.rs`) — `Store::create`, `Store::update`, and `Store::delete` now call `dump::on_change` / `dump::on_delete` after each successful database operation. A dump write failure propagates as a CRUD error (prevents silent DB-file divergence). `Store::open` logs a `tracing::warn!` when `sync: bidirectional` is configured but not yet implemented.
