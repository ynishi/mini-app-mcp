/// MCP server module for mini-app-mcp.
///
/// Re-exports the public entry point and server type for use in `main.rs`.
pub use server::{MiniAppMcpServer, ReloadResult, run, run_http};

// registry now lives in mini-app-core; re-export here for backward-compat
// path callers that used `mini_app_mcp::mcp::registry::*`.
pub use mini_app_core::registry;

pub(crate) mod resources;
pub mod schema_tools;
pub mod server;
