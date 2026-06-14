//! # mini-app-mcp
//!
//! MCP server (stdio transport) for the mini-app DB core. Depends on
//! `mini-app-core` for all DB / schema / store / materialize logic.
//!
//! ## Re-exports (backward-compat surface)
//!
//! Pre-split callers used `mini_app_mcp::{config, error, schema, store,
//! filter, materialize, dump, backup, snapshot}` paths. After the workspace
//! split these symbols live in `mini-app-core`; we re-export them here so
//! existing integration tests continue to compile without path rewrites.

pub use mini_app_core::aggregator;
pub use mini_app_core::backup;
pub use mini_app_core::config;
pub use mini_app_core::dump;
pub use mini_app_core::error;
pub use mini_app_core::filter;
pub use mini_app_core::materialize;
pub use mini_app_core::registry;
pub use mini_app_core::schema;
pub use mini_app_core::snapshot;
pub use mini_app_core::store;

pub use mini_app_core::UpdateMode;

/// ACL adapter — convert [`mini_app_core::MiniAppError`] to [`rmcp::ErrorData`].
mod error_conv;

/// MCP server module (transport + handlers + resources).
pub mod mcp;

pub(crate) use error_conv::miniapp_error_to_mcp_error;
