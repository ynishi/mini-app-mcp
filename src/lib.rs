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
