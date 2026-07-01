//! Entrypoint for the qmp-mcp Rust variant (ADR-0011).
//!
//! Mirrors `../../src/index.ts`: load the config (failing closed with an
//! actionable message and exit code 1 on a [`config::ConfigError`]), set the log
//! level, construct the single-instance Orchestrator behind a shared async mutex,
//! then serve the MCP server — tearing down any running Instance on shutdown so
//! qemu is never orphaned (ADR-0004). This slice supports the stdio transport only;
//! selecting `http`/`both` is an actionable error (HTTP is slice #25).

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::Arc;

use qmp_mcp::config::{self, Config, TransportMode};
use qmp_mcp::instance::hardware_spec::probe_kvm;
use qmp_mcp::instance::orchestrator::{InstanceState, Orchestrator, OrchestratorOptions};
use qmp_mcp::logging;
use qmp_mcp::qemu::driver::UnavailableDriver;
use qmp_mcp::server::QmpMcpServer;
use rmcp::{transport::stdio, ServiceExt};
use tokio::sync::Mutex;

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

    // The single-instance Orchestrator, shared behind an async mutex so concurrent
    // tool calls serialise on the one Instance (ADR-0011). The real QEMU driver
    // arrives in slice #21; until then the fail-closed placeholder is wired in, so a
    // create_instance attempt reports an actionable "not yet implemented" instead of
    // pretending to have started a VM.
    let orchestrator = Arc::new(Mutex::new(Orchestrator::new(
        Box::new(UnavailableDriver),
        orchestrator_options(&config),
    )));

    let service = QmpMcpServer::new(Arc::clone(&orchestrator))
        .serve(stdio())
        .await
        .inspect_err(|err| tracing::error!("failed to start stdio transport: {err:?}"))?;

    // ADR-0004: the Instance's lifetime is the server's lifetime. Race the service
    // (which resolves when the peer disconnects / stdin closes) against SIGINT/
    // SIGTERM, then tear down any running Instance before exiting so qemu is never
    // orphaned. Mirrors the shutdown hook in ../../src/index.ts.
    tokio::select! {
        result = service.waiting() => {
            result?;
            tracing::info!("transport closed; shutting down");
        }
        signal = shutdown_signal() => {
            tracing::info!("received {signal}; shutting down");
        }
    }
    teardown(&orchestrator).await;
    Ok(())
}

/// Assemble the Orchestrator's options from the validated config. The QMP socket is
/// a per-server file under the OS temp dir (the server owns it; ADR-0004).
fn orchestrator_options(config: &Config) -> OrchestratorOptions {
    OrchestratorOptions {
        binary: "qemu-system-x86_64".to_string(),
        qmp_socket_path: default_qmp_socket_path(),
        image_dir: Some(config.image_dir.clone()),
        iso_dir: Some(config.iso_dir.clone()),
        hostfwd_port_range: Some(config.hostfwd_port_range),
        allow_host_net: config.allow_host_net,
        max_memory_mb: Some(config.max_memory_mb),
        max_vcpus: Some(config.max_vcpus),
        allow_raw_args: config.allow_raw_args,
        // `/dev/kvm` probe — the single source of truth from the hardware-spec module.
        kvm_available: Box::new(probe_kvm),
    }
}

/// Default QMP socket path: a per-server file under the OS temp dir (mirrors the TS
/// `defaultQmpSocketPath`).
fn default_qmp_socket_path() -> String {
    std::env::temp_dir()
        .join("qmp-mcp")
        .join("qmp.sock")
        .to_string_lossy()
        .into_owned()
}

/// Destroy a running Instance before the process exits (ADR-0004). A no-op when
/// nothing is running; a failure is logged, never fatal.
async fn teardown(orchestrator: &Arc<Mutex<Orchestrator>>) {
    let mut orchestrator = orchestrator.lock().await;
    if orchestrator.state() == InstanceState::None {
        return;
    }
    tracing::info!("shutting down: destroying the running Instance");
    if let Err(err) = orchestrator.destroy_instance().await {
        tracing::error!("failed to destroy the Instance during shutdown: {err}");
    }
}

/// Resolve when a termination signal arrives, returning its name for the log.
#[cfg(unix)]
async fn shutdown_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => "SIGINT",
        _ = sigterm.recv() => "SIGTERM",
    }
}

/// Non-unix fallback: resolve on Ctrl-C only.
#[cfg(not(unix))]
async fn shutdown_signal() -> &'static str {
    let _ = tokio::signal::ctrl_c().await;
    "SIGINT"
}
