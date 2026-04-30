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
