//! apple-mcp: an MCP server (stdio) exposing Apple's online services as tools.

mod config;
mod error;
mod http;
mod jwt;
mod server;
mod setup;
mod tools;
mod util;

use rmcp::ServiceExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version") | Some("-V") => {
            println!("apple-mcp {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("--check") | Some("check") => {
            let cfg = config::Config::load();
            println!("{}", cfg.status_report());
            return Ok(());
        }
        Some("--doctor") | Some("doctor") => {
            let server = server::AppleServer::new();
            let only: Vec<String> = args.iter().skip(1).cloned().collect();
            let results = server.doctor(if only.is_empty() { None } else { Some(&only) }).await;
            println!("config: {}", server.cfg().path.display());
            for r in &results {
                let tag = if !r.configured { "[skip]" } else if r.ok { "[ ok ]" } else { "[FAIL]" };
                println!("  {tag} {:<20} {}{}", r.section, r.detail, if r.millis > 0 { format!(" ({} ms)", r.millis) } else { String::new() });
            }
            let failed = results.iter().filter(|r| r.configured && !r.ok).count();
            if failed > 0 {
                std::process::exit(2);
            }
            return Ok(());
        }
        Some("setup") | Some("--setup") => {
            return setup::run(&args[1..]).await;
        }
        Some("--template") | Some("template") => {
            println!("{}", config::CONFIG_TEMPLATE);
            return Ok(());
        }
        Some("--help") | Some("-h") => {
            println!(
                "apple-mcp (MCP server over stdio)\n\n  apple-mcp            run the MCP server on stdin/stdout\n  apple-mcp --check    show which Apple services are configured\n  apple-mcp --doctor [section...]  verify configured credentials with read-only calls\n  apple-mcp setup [auto|set|discover|links|open|pubkey|wizard]  automated credential setup (setup --help)\n  apple-mcp --template print a config.json template\n  apple-mcp --version\n\nConfig file: $APPLE_MCP_CONFIG or ~/.config/apple-mcp/config.json\nEnv overrides: APPLE_MCP_<SECTION>_<KEY> (e.g. APPLE_MCP_APP_STORE_CONNECT_KEY_ID), APPLE_TEAM_ID"
            );
            return Ok(());
        }
        _ => {}
    }

    let server = server::AppleServer::new();
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
