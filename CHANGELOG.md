# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`schema_create` MCP tool** (`mcp/schema_tools.rs`, `mcp/server.rs`) — creates a new `schema.yaml` under the specified scope (`project` or `user`) directory and atomically rebuilds the live table registry. Accepts the full schema definition (table name, fields) as a JSON argument. Returns the path where the schema was written. Fails with `SCHEMA_EXISTS` if a schema for that table name already exists. Supports `dry_run=true` to preview the operation without writing any files. Path-traversal characters in the table name are rejected up front.
- **`schema_update` MCP tool** (`mcp/schema_tools.rs`, `mcp/server.rs`) — replaces an existing `schema.yaml` with a new definition (full overwrite). Backs up the previous YAML and a point-in-time SQLite snapshot to `<scope_root>/_backup/<table>.<unix_secs>.{yaml,db}` before writing. Rebuilds the table registry after the write. Supports `dry_run=true` (returns `{ fields_added, fields_removed }` without touching disk). Idempotent: calling twice with identical args produces the same observable state.
- **`schema_delete` MCP tool** (`mcp/schema_tools.rs`, `mcp/server.rs`) — removes a table's `schema.yaml` by moving it to the `_backup/` directory and removes the table from the live registry. **Does not touch the SQLite database file** — altering or dropping the underlying table remains the operator's explicit responsibility (no automatic DDL migration). Supports `dry_run=true`. Marked `destructive_hint=true`.
- **`schema_batch` MCP tool** (`mcp/schema_tools.rs`, `mcp/server.rs`) — executes an array of operations (`ops[]`) atomically under a single SQLite SAVEPOINT: any op failure rolls back all preceding ops including schema mutations, leaving YAML and DB in the exact state they were before the batch started. All ops must target the same table (cross-table batches are rejected with `VALIDATION` error). YAML writes within a batch are deferred; rename is applied only on SAVEPOINT commit, and tmp files are removed on rollback. Registry is rebuilt once after all ops succeed.
- **Backup module** (`src/backup.rs`) — `write_yaml_backup` / `write_db_backup` functions that write point-in-time copies of a table's schema file and SQLite database to `<scope_root>/_backup/<table>.<unix_secs>.yaml` and `<table>.<unix_secs>.db`. A retention sweep runs immediately after each backup write and deletes the oldest copies beyond the configured limit (default 10, overridable via `MINI_APP_BACKUP_RETENTION`). Backup I/O runs inside `tokio::task::spawn_blocking`.
- **New error variants** (`src/error.rs`) — `SchemaExists { table }` (code: `SCHEMA_EXISTS`), `Backup(String)` (code: `BACKUP_ERROR`), and `BatchAborted { op_index, reason }` (code: `BATCH_ABORTED`). All three carry structured fields through `From<MiniAppError> for McpError` so agents can handle them programmatically.

### Fixed

- **Path-traversal rejection in schema CRUD tools** (`mcp/schema_tools.rs`) — table names containing `/`, `\`, or `..` components are rejected with `MiniAppError::Validation` before any filesystem operation is attempted. This prevents a caller from escaping the configured scope directory by supplying a crafted table name.

## [0.4.0] - 2026-05-04

### Added

- **`reload` MCP tool** (`mcp/server.rs`) — new tool that re-scans `MINI_APP_USER_DIR` / `MINI_APP_PROJECT_DIR` (and re-applies `MINI_APP_SCHEMA` + `MINI_APP_DB` if set) and atomically replaces the live table registry without restarting the server. Returns `{ mounted: usize, added: Vec<String>, removed: Vec<String> }` so callers can observe which tables changed. The swap is performed via `ArcSwap::store()` — in-flight tool calls running against the previous registry complete normally; subsequent calls see the new registry. Limitations: no file-system watcher (explicit invocation only); whole-registry replacement (no per-table partial reload); no schema migration for existing rows; concurrent `reload` calls are last-write-wins.

### Changed

- **WAL journal mode on all SQLite connections** (`store.rs`) — `Store::open` now executes `PRAGMA journal_mode = WAL` immediately after opening every connection. WAL mode is persistent (SQLite retains it across close/reopen) and enables one writer + many concurrent readers, which is required for safe operation during the dual-registry window that exists while `reload` replaces the table registry. Existing `.db` files are migrated transparently on next open. Sidecar files `<db>.db-wal` and `<db>.db-shm` are created alongside each `.db` file; these are managed by SQLite and must not be deleted manually.
- **`MiniAppMcpServer` internals** (`mcp/server.rs`) — `tables` field changed from `Arc<TableRegistry>` to `Arc<ArcSwap<TableRegistry>>` to support atomic hot-reload. `Config` is now retained on the server struct (`Arc<Config>`) so the `reload` tool can re-scan the same directories that were used at startup. All existing tool implementations (`info`, `create`, `get`, `list`, `update`, `delete`) load a snapshot of the registry via `ArcSwap::load()` at the start of each call and release the guard before any `await` point. `TableRegistry` doc comment updated from "immutable, no interior mutability" to "snapshot is immutable; replaced via ArcSwap on reload".
- **`arc-swap` dependency added** (`Cargo.toml`) — `arc-swap = "1"` added to support the wait-free atomic swap of `TableRegistry`.

### Fixed

- **`reload` early-reject on legacy single-table servers** (`mcp/server.rs`) — when `MiniAppMcpServer` is constructed via `new_single` (legacy adapter path), all four `mount_config` fields are `None`. Calling the `reload` tool on such a server previously would re-mount an empty registry and atomically swap out the originally-mounted table, leaving the server inaccessible until restart. `tool_reload` now detects this all-`None` configuration up front and returns `MiniAppError::Config("reload not configured: server was constructed via new_single without a mount config")` without touching the registry.
- **`PRAGMA journal_mode = WAL` read-back warning** (`store.rs`) — SQLite silently falls back to a non-WAL mode (memory / delete) on filesystems that do not support WAL (notably `:memory:` databases, some network filesystems). `Store::open` now reads back the resulting `journal_mode` after issuing the WAL pragma and emits `tracing::warn!(actual_mode = ..., "PRAGMA journal_mode=WAL fell back to non-WAL mode; concurrent reload may hit SQLITE_BUSY")` when the actual mode is not `wal`. The fallback is observable instead of silent; behaviour is unchanged otherwise (no error returned).

## [0.3.1] - 2026-05-03

### Changed

- **Empty-registry start is no longer fatal** (`mcp/server.rs`) — when 0 tables resolve from `MINI_APP_USER_DIR` / `MINI_APP_PROJECT_DIR` and no legacy `MINI_APP_SCHEMA` + `MINI_APP_DB` is set, the server now logs a `tracing::warn!` and proceeds to serve `info` and resources instead of erroring out. Tool calls return `TABLE_REQUIRED` on a per-call basis. This lets `mini-app-mcp` be deployed once into a user-global MCP registry (e.g. `~/.claude.json`) and have table directories added later without restarting the host.
- **Auto-create `MINI_APP_USER_DIR`** (`mcp/server.rs`) — at startup the server runs `tokio::fs::create_dir_all` on the resolved User-scope directory (default `~/.mini-app/`). Failures are logged as a warning, not propagated. Project-scope directory is intentionally left untouched to avoid polluting arbitrary CWDs.

## [0.3.0] - 2026-05-03

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
