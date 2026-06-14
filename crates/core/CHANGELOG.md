# Changelog — mini-app-core

All notable changes to the `mini-app-core` crate are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

`mini-app-core` is the transport-agnostic DB / schema / store / materialize
library extracted from the `mini-app-mcp` workspace. The companion `mini-app-mcp`
crate (binary, MCP stdio transport) depends on this library via a strict one-way
`mcp → core` dependency boundary — `mini-app-core` carries zero `rmcp` dependency.

## [0.1.0] - 2026-06-14

### Added

- Initial release. Library extracted from `mini-app-mcp` v0.10.0 via workspace
  split so that other consumers (Tauri desktop apps, in-process DB embedders,
  the upcoming `persona-network` primitive) can use the core CRUD layer without
  going through MCP stdio transport.
- Public modules: `schema`, `error`, `config`, `store`, `filter`, `materialize`,
  `dump`, `backup`, `snapshot`, `registry`.
- `MiniAppError` enum with structured error codes (`codes::*`) covering 26
  failure modes (validation / not-found / schema / storage / config / table /
  batch / materialize / alias / ambiguous-id).
- `Store` — SQLite-backed row store with `tokio::task::spawn_blocking` for all
  blocking I/O, RFC 7396 shallow-merge update mode, and UUID prefix matching
  for `get` / `update` / `delete`.
- `TableRegistry` — atomic multi-table registry backed by `arc-swap::ArcSwap`
  for lock-free hot reload.
- `materialize::do_materialize` — row selection + field projection +
  multi-format filesystem output with SHA-256 integrity digest.
- `backup` / `snapshot` — SQLite online backup utilities with retention.
- `ListFilter` — serde-tagged enum (Eq / In / Or / And / Like) with
  schema-validated field filtering and recursive composition.

### Notes

- `impl From<MiniAppError> for rmcp::ErrorData` is intentionally NOT provided
  by this crate (Rust orphan rule, RFC 1023). MCP wire conversion lives in the
  `mini-app-mcp` crate as a `pub(crate) fn miniapp_error_to_mcp_error`
  ACL adapter (Outline rust book §5-1-10 K-orphan-rule pattern).
