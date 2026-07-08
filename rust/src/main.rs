//! Entrypoint for the qmp-mcp Rust variant (ADR-0011).
//!
//! Mirrors `../../typescript/src/index.ts`: load the config (failing closed with an
//! actionable message and exit code 1 on a [`config::ConfigError`]), set the log
//! level, construct the single-instance Orchestrator behind a shared async mutex,
//! then serve the MCP server — tearing down any running Instance on shutdown so
//! qemu is never orphaned (ADR-0004). The transport is selected by
//! `QMP_MCP_TRANSPORT`: `stdio` (auth-free), the streamable `http` transport behind
//! the fail-closed auth + origin guards (`crate::http`, ADR-0005), or `both`
//! concurrently — mirroring `index.ts`.

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::Arc;

use qmp_mcp::config::{self, Config, TransportMode};
use qmp_mcp::http;
use qmp_mcp::instance::hardware_spec::{host_qemu_arch, probe_kvm};
use qmp_mcp::instance::image_store::{ImageStore, ImageStoreOptions};
use qmp_mcp::instance::iso_store::IsoStore;
use qmp_mcp::instance::orchestrator::{InstanceState, Orchestrator, OrchestratorOptions};
use qmp_mcp::logging;
use qmp_mcp::policy::{self, ResolvedPolicy};
use qmp_mcp::qemu::real_driver::RealQemuDriver;
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

    // Resolve the Command Policy for the generic qmp_execute tool: the default-safe
    // allowlist plus QMP_MCP_ALLOW/DENY and the optional QMP_MCP_POLICY_FILE overrides,
    // with the immutable hard denylist always in force (ADR-0003). Fail closed — with
    // an actionable message naming QMP_MCP_POLICY_FILE — on a missing/malformed file,
    // rather than starting with a half-understood policy. Resolved after the logger is
    // up so the "ignoring hard-denied allow override" warnings are visible.
    let command_policy = match policy::resolve_command_policy(&env) {
        Ok(policy) => policy,
        Err(err) => {
            tracing::error!("{err}");
            return ExitCode::FAILURE;
        }
    };

    match run(config, command_policy).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("fatal: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Serve the configured transport(s). Returns an error (which `main` logs and turns
/// into exit 1) for any serve-time failure.
///
/// Mirrors `../../typescript/src/index.ts`: `stdio` serves one auth-free stdio transport;
/// `http` serves the streamable HTTP transport behind the fail-closed auth +
/// origin guards (API-key or JWT, per `QMP_MCP_AUTH`); `both` runs stdio and HTTP
/// concurrently. In every case any running Instance is torn down before returning,
/// so qemu is never orphaned (ADR-0004).
async fn run(
    config: Config,
    command_policy: ResolvedPolicy,
) -> Result<(), Box<dyn std::error::Error>> {
    // The single-instance Orchestrator, shared behind an async mutex so concurrent
    // tool calls serialise on the one Instance (ADR-0011). It is wired to the real
    // QEMU driver, which spawns `qemu-system-*` on the server-managed QMP UNIX socket
    // and negotiates the live QMP Session (slice #21).
    // `new_shared` installs the Orchestrator's `Weak` self back-reference so
    // create_instance can spawn the exit-watch task that reconciles an unexpected qemu
    // exit (crash, guest poweroff, external kill) back to NONE (issue #28).
    let orchestrator = Orchestrator::new_shared(
        Box::new(RealQemuDriver),
        orchestrator_options(&config, command_policy),
    );

    // The two allowlisted stores (ADR-0006): a read-write Image Store that provisions
    // disks via `qemu-img create` under the configured size cap, and a strictly
    // read-only ISO Store. Built once from the validated config (env is snapshotted at
    // startup), so the tools resolve names against the configured directories.
    let image_store = ImageStore::new(ImageStoreOptions {
        dir: config.image_dir.clone(),
        max_disk_gb: config.max_disk_gb,
        qemu_img_binary: None,
        run: None,
    });
    let iso_store = IsoStore::new(config.iso_dir.clone());
    let server = QmpMcpServer::new(Arc::clone(&orchestrator), image_store, iso_store);

    // ADR-0005: the only way to serve HTTP without auth is the explicit insecure
    // override, which logs the same cleartext warning as the TS server so an operator
    // is never surprised by an open port. stdio-only never reaches this.
    if config.allow_insecure && config.transport.exposes_http() {
        tracing::warn!(
            "QMP_MCP_ALLOW_INSECURE=true: serving the HTTP transport WITHOUT authentication. \
             This is for local development only — never expose this port on an untrusted network."
        );
    }

    match config.transport {
        TransportMode::Stdio => serve_stdio(server, &orchestrator).await?,
        TransportMode::Http => serve_http(&config, server, &orchestrator).await?,
        TransportMode::Both => serve_both(&config, server, &orchestrator).await?,
    }
    Ok(())
}

/// Serve the auth-free stdio transport, racing it against a termination signal, then
/// tear down any running Instance (ADR-0004).
async fn serve_stdio(
    server: QmpMcpServer,
    orchestrator: &Arc<Mutex<Orchestrator>>,
) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("starting qmp-mcp (transport=stdio)");
    let service = server
        .serve(stdio())
        .await
        .inspect_err(|err| tracing::error!("failed to start stdio transport: {err:?}"))?;
    tokio::select! {
        result = service.waiting() => {
            result?;
            tracing::info!("transport closed; shutting down");
        }
        signal = shutdown_signal() => {
            tracing::info!("received {signal}; shutting down");
        }
    }
    teardown(orchestrator).await;
    Ok(())
}

/// Serve only the streamable HTTP transport (behind the fail-closed guards), racing
/// it against a termination signal, then tear down any running Instance (ADR-0004).
async fn serve_http(
    config: &Config,
    server: QmpMcpServer,
    orchestrator: &Arc<Mutex<Orchestrator>>,
) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!(
        "starting qmp-mcp (transport=http on http://{}:{}{})",
        config.http_host,
        config.http_port,
        config.http_endpoint
    );
    tokio::select! {
        result = http::serve(config, server, std::future::pending()) => {
            result?;
            tracing::info!("HTTP transport closed; shutting down");
        }
        signal = shutdown_signal() => {
            tracing::info!("received {signal}; shutting down");
        }
    }
    teardown(orchestrator).await;
    Ok(())
}

/// Serve stdio and the streamable HTTP transport concurrently (mirroring the TS
/// `both` mode). Whichever of {stdio closes, HTTP ends, a signal arrives} happens
/// first stops the server; any running Instance is then torn down (ADR-0004).
async fn serve_both(
    config: &Config,
    server: QmpMcpServer,
    orchestrator: &Arc<Mutex<Orchestrator>>,
) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!(
        "starting qmp-mcp (transport=both on http://{}:{}{})",
        config.http_host,
        config.http_port,
        config.http_endpoint
    );
    let stdio_service = server
        .clone()
        .serve(stdio())
        .await
        .inspect_err(|err| tracing::error!("failed to start stdio transport: {err:?}"))?;
    tokio::select! {
        result = stdio_service.waiting() => {
            result?;
            tracing::info!("stdio transport closed; shutting down");
        }
        result = http::serve(config, server, std::future::pending()) => {
            result?;
            tracing::info!("HTTP transport closed; shutting down");
        }
        signal = shutdown_signal() => {
            tracing::info!("received {signal}; shutting down");
        }
    }
    teardown(orchestrator).await;
    Ok(())
}

/// Assemble the Orchestrator's options from the validated config and the resolved
/// Command Policy. The QMP socket is a per-server file under the OS temp dir (the
/// server owns it; ADR-0004).
fn orchestrator_options(config: &Config, command_policy: ResolvedPolicy) -> OrchestratorOptions {
    OrchestratorOptions {
        // argv[0] for the launched guest. When QMP_MCP_QEMU_BINARY is unset the binary
        // is derived per-instance from the spec's machine (q35 -> x86_64, virt/raspi*
        // -> aarch64, issue #18); an explicit value overrides that for every Instance.
        qemu_binary_override: config.qemu_binary_override.clone(),
        // This host's arch for the accel=auto guest/host match (issue #18).
        host_arch: host_qemu_arch().to_string(),
        qmp_socket_path: default_qmp_socket_path(),
        image_dir: Some(config.image_dir.clone()),
        iso_dir: Some(config.iso_dir.clone()),
        hostfwd_port_range: Some(config.hostfwd_port_range),
        allow_host_net: config.allow_host_net,
        auto_start: config.auto_start,
        max_memory_mb: Some(config.max_memory_mb),
        max_vcpus: Some(config.max_vcpus),
        allow_raw_args: config.allow_raw_args,
        // The generic qmp_execute tool runs only what this policy admits (ADR-0003).
        command_policy: Some(command_policy),
        // Bound the Event Buffer of recent QMP async events (issue #12).
        event_buffer_size: Some(config.event_buffer_size),
        // noVNC Viewer for a vnc Display (ADR-0010): the human-facing gate plus the
        // Viewer's own bind address/port. A vnc spec is refused when the password is
        // unset. `start_viewer: None` wires in the real in-process Viewer.
        viewer_password: config.viewer_password.clone(),
        viewer_host: config.viewer_host.clone(),
        viewer_port: config.viewer_port,
        start_viewer: None,
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
