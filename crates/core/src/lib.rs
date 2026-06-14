//! # mini-app-core
//!
//! Agent-First CRUD store core library. Transport-agnostic DB layer that
//! drives one table per process from `schema.yaml`, backed by SQLite.
//!
//! Transport (MCP / HTTP / CLI) lives in a separate crate (`mini-app-mcp`)
//! that depends on this one. The dependency is strictly one-way:
//! `mcp → core`, never the reverse. Error type conversion to `rmcp::ErrorData`
//! is performed by an ACL adapter (private free function) in the mcp crate to
//! comply with the Rust orphan rule (Outline rust book §5-1-10 K-orphan-rule).

/// Schema definition, runtime loading, and JSON row validation.
pub mod schema;

/// Application-level error type.
pub mod error;

/// Runtime configuration loaded from environment variables.
pub mod config;

/// SQLite-backed row store (async CRUD via `tokio::task::spawn_blocking`).
pub mod store;

/// Update semantics for [`store::Store::update`]: [`UpdateMode::Merge`] (default,
/// RFC 7396 shallow merge) or [`UpdateMode::Replace`] (full replacement).
pub use store::UpdateMode;

/// Server-side row filter for the `list` tool.
pub mod filter;

/// Multi-table registry + atomic reload (Arc-Swap based, K-110-compliant).
pub mod registry;

/// `row_materialize` operation — row selection, field projection, and
/// multi-format filesystem output with SHA-256 integrity digest.
pub mod materialize;

/// Framework-level dump hook utilities (write-only file materialization).
pub mod dump;

/// Backup utilities (YAML + SQLite online backup with retention).
pub mod backup;

/// Snapshot utilities (SQLite-only online snapshot with retention).
pub mod snapshot;

/// Re-export of [`error::MiniAppError`] for convenient `use mini_app_core::MiniAppError`.
pub use error::MiniAppError;
