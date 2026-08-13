use clap::Parser;

/// Command-line interface for mini-app-mcp.
///
/// Two transport modes are supported, both MCP-only (Crux "MCP-only entry
/// point" constraint — no REST/CLI-CRUD surface):
///
/// - `--mcp` — stdio transport (single local client).
/// - `--mcp-http` — streamable HTTP transport (multi-device: one central
///   daemon, remote MCP clients connect to `http://<host>/mcp`).
#[derive(Parser)]
#[command(
    name = "mini-app-mcp",
    version,
    about = "Agent-First CRUD store MCP server"
)]
struct Cli {
    /// Start as MCP server (stdio transport).
    #[arg(long, conflicts_with = "mcp_http")]
    mcp: bool,

    /// Start as MCP server (streamable HTTP transport) on `--bind`.
    #[arg(long)]
    mcp_http: bool,

    /// Bind address for --mcp-http. Non-loopback addresses require
    /// MINI_APP_HTTP_TOKEN to be set.
    #[arg(long, default_value = "127.0.0.1:8484")]
    bind: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if cli.mcp {
        mini_app_mcp::mcp::run().await
    } else if cli.mcp_http {
        mini_app_mcp::mcp::run_http(&cli.bind).await
    } else {
        eprintln!("mini-app-mcp: use --mcp (stdio) or --mcp-http (streamable HTTP) to start");
        std::process::exit(1);
    }
}
