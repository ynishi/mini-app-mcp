/// MCP server module for mini-app-mcp.
///
/// Re-exports the public entry point and server type for use in `main.rs`.
pub use server::{MiniAppMcpServer, run};

pub mod registry;
pub(crate) mod resources;
mod server;
