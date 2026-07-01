//! Entrypoint for the qmp-mcp Rust variant (ADR-0011).
//!
//! Mirrors `../../src/index.ts`: load the config (failing closed with an
//! actionable message and exit code 1 on a [`config::ConfigError`]), set the log
//! level, then serve the MCP server. This slice supports the stdio transport only;
//! selecting `http`/`both` is an actionable error (HTTP is slice #25).

mod config;
mod logging;
mod server;

use std::collections::HashMap;
use std::process::ExitCode;

use config::{Config, TransportMode};
use rmcp::{transport::stdio, ServiceExt};
use server::QmpMcpServer;

#[tokio::main]
async fn main() -> ExitCode {
    // Snapshot the process environment into a map; config parsing is a pure
    // function of it (never reads the process env itself), so it stays testable.
    let env: HashMap<String, String> = std::env::vars().collect();

    let config = match config::load_config(&env) {
        Ok(config) => config,
        Err(err) => {
            // The logger is not up yet; write the actionable message straight to
            // stderr (never stdout — it carries the stdio JSON-RPC stream), then
            // exit 1, mirroring index.ts.
            eprintln!("[qmp-mcp] error: {err}");
            return ExitCode::FAILURE;
        }
    };

    logging::init(config.log_level);

    match run(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("fatal: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Serve the configured transport. Returns an error (which `main` logs and turns
/// into exit 1) for the not-yet-implemented HTTP transports and for any serve-time
/// failure.
async fn run(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    if config.transport.exposes_http() {
        return Err(format!(
            "QMP_MCP_TRANSPORT={} selected, but the HTTP transport arrives in a later slice \
             (#25). Set QMP_MCP_TRANSPORT=stdio to run this build.",
            config.transport
        )
        .into());
    }

    debug_assert_eq!(config.transport, TransportMode::Stdio);
    tracing::info!("starting qmp-mcp (transport=stdio)");

    let service = QmpMcpServer::new()
        .serve(stdio())
        .await
        .inspect_err(|err| tracing::error!("failed to start stdio transport: {err:?}"))?;
    // Resolves when the peer disconnects (stdin closes) or the service stops.
    service.waiting().await?;
    Ok(())
}
