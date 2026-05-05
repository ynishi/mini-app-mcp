//! # mini-app-mcp
//!
//! Agent-First CRUD store MCP server. One daemon process manages one table
//! whose shape is declared entirely in `schema.yaml`.
//!
//! ## Modules
//!
//! - [`schema`] — YAML schema loading and JSON row validation.
//! - [`error`] — [`error::MiniAppError`] enum and its conversion to
//!   `rmcp::ErrorData` (structured JSON errors).
//!
//! Additional modules (`config`, `store`, `mcp`) are added in subsequent
//! subtasks and will be declared here when ready.

/// Schema definition, runtime loading, and JSON row validation.
///
/// The [`schema::SchemaConfig`] type is the runtime representation of
/// `schema.yaml` and is the sole authority for field definitions, type
/// coercions, and required-field checks.
pub mod schema;

/// Application-level error type and MCP error conversion.
///
/// [`error::MiniAppError`] is used throughout the crate. Every variant
/// converts to a structured JSON `rmcp::ErrorData` object via the
/// `From<MiniAppError> for rmcp::ErrorData` impl, satisfying the Crux
/// "structured JSON error" constraint.
pub mod error;

/// Runtime configuration loaded from environment variables.
pub mod config;

/// SQLite-backed row store (async CRUD via `tokio::task::spawn_blocking`).
pub mod store;

/// MCP server: 6 tools (info, create, get, list, update, delete) over stdio.
pub mod mcp;

/// Framework-level dump hook utilities (write-only file materialization).
///
/// Provides [`dump::on_change`] and [`dump::on_delete`] hooks that `Store`
/// calls after successful CRUD operations.  Defined here — not in `store.rs` —
/// so any future mini-app can reuse them directly (Crux #1 compliance).
pub mod dump;

/// Backup utilities for schema CRUD tools.
///
/// Provides [`backup::write_backup_pair`] (YAML + SQLite online backup) and
/// [`backup::purge_old_backups`] (retention-based cleanup). All I/O runs
/// inside `tokio::task::spawn_blocking` (K-110).
pub mod backup;
