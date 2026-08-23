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

/// Server-side ORDER BY primitive for the `list` tool.
///
/// Provides [`order_by::OrderByItem`] (field + direction pair) and
/// [`order_by::Direction`] (`Asc` / `Desc`), along with
/// [`order_by::validate_order_by`] (schema-whitelist check) and
/// [`order_by::build_order_by_sql`] (infallible SQL clause builder).
pub mod order_by;

/// Multi-table aggregation primitives (`query_aggregate` tool backend).
///
/// Provides `AliasAggregator` (Count / Sum / Avg / Min / Max / GroupBy),
/// `SourceSpec` (Single / Multi), `AliasRunResult` (Rows / Count / Value /
/// Groups), and `execute_aggregate` for SQLite ATTACH DATABASE + UNION ALL
/// composition across per-table `.db` files.
pub mod aggregator;

/// Multi-table registry + atomic reload (Arc-Swap based, K-110-compliant).
pub mod registry;

/// Global alias storage (Phase 2). Persists named queries that span
/// Single / Multi / Pattern table sources in a dedicated
/// `<project_dir or user_dir>/_global.db` SQLite file, separate from
/// per-table `<table>.db`. Provides lookup with Project → User
/// precedence and a lossless migration helper from legacy per-table
/// `_aliases` storage.
pub mod alias_storage;

/// `row_materialize` operation — row selection, field projection, and
/// multi-format filesystem output with SHA-256 integrity digest.
pub mod materialize;

/// Framework-level dump hook utilities (write-only file materialization).
pub mod dump;

/// Backup utilities (YAML + SQLite online backup with retention).
pub mod backup;

/// Snapshot utilities (SQLite-only online snapshot with retention).
pub mod snapshot;

/// S3-compatible snapshot upload (`data_snapshot` `upload=true` backend).
///
/// Configuration resolution (`MINI_APP_S3_*` env) is always compiled; the
/// network put path requires the `s3-upload` cargo feature.
pub mod snapshot_upload;

/// Top-level orchestration for `alias_run` — single entry point exposed for
/// both the MCP tool handler and direct SDK consumers.
pub mod alias_run;

/// Row-level change history (`_row_history` table).
///
/// Records every `create`, `update`, and `delete` atomically (same SQLite
/// transaction as the data DML).  Exposes [`row_history::ensure_history_table`],
/// [`row_history::record_in_tx`], [`row_history::fetch_at`],
/// [`row_history::list_versions`], and [`row_history::purge_old_history`].
pub mod row_history;

/// Re-export of [`error::MiniAppError`] for convenient `use mini_app_core::MiniAppError`.
pub use error::MiniAppError;
