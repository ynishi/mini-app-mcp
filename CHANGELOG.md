# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
