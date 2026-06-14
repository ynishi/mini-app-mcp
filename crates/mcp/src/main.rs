use clap::Parser;

/// Command-line interface for mini-app-mcp.
///
/// The only supported flag is `--mcp`, which starts the server in MCP stdio
/// transport mode.  No other entry points (HTTP, REST, CLI-CRUD) are provided;
/// this is by design (Crux "MCP-only entry point" constraint).
#[derive(Parser)]
#[command(
    name = "mini-app-mcp",
    version,
    about = "Agent-First CRUD store MCP server"
)]
struct Cli {
    /// Start as MCP server (stdio transport).
    #[arg(long)]
    mcp: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if cli.mcp {
        mini_app_mcp::mcp::run().await
    } else {
        eprintln!("mini-app-mcp: use --mcp to start as MCP server");
        std::process::exit(1);
    }
}
