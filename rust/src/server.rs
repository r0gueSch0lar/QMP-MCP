//! The MCP server: a single [`QmpMcpServer`] struct whose `#[tool_router]` impl
//! carries the `#[tool]` methods, and whose `#[tool_handler] impl ServerHandler`
//! advertises them (ADR-0011).
//!
//! The server holds the shared `Arc<Mutex<Orchestrator>>`. Every tool locks that one
//! async mutex, so concurrent tool calls serialise on the single managed Instance —
//! which is exactly what makes the create-time TOCTOU structurally impossible
//! (ADR-0011). This slice wires the lifecycle surface: `create_instance`,
//! `destroy_instance`, `get_instance`, and `get_status`. The remaining tools (QMP
//! control, events, images) land in later slices.

use std::sync::Arc;

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ErrorData as McpError, Json, ServerHandler,
};
use schemars::JsonSchema;
use serde::Serialize;
use tokio::sync::Mutex;

use crate::instance::hardware_spec::{AccelMode, DisplayMode, HardwareSpec, HardwareSpecParams};
use crate::instance::orchestrator::{InstanceState, Orchestrator};

/// A compact, JSON-serialisable summary of a validated Hardware Spec, returned by
/// the lifecycle tools. Deliberately a projection (not the full spec): enough for an
/// agent to confirm what it built, without re-exposing the validated newtypes.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SpecSummary {
    /// QEMU machine type (e.g. `q35`).
    pub machine: String,
    /// CPU model passed to `-cpu` (e.g. `max`).
    pub cpu: String,
    /// Number of virtual CPUs.
    pub vcpus: u32,
    /// Guest RAM in MiB.
    pub memory_mb: u32,
    /// The requested accelerator mode (`auto`/`kvm`/`tcg`).
    pub accel: AccelMode,
    /// Guest Display mode (`none`/`vnc`).
    pub display: DisplayMode,
    /// Number of guest disks.
    pub disks: usize,
    /// Whether a CD-ROM is attached.
    pub cdrom: bool,
}

impl SpecSummary {
    fn of(spec: &HardwareSpec) -> Self {
        Self {
            machine: spec.machine.as_str().to_string(),
            cpu: spec.cpu.as_str().to_string(),
            vcpus: spec.vcpus,
            memory_mb: spec.memory_mb,
            accel: spec.accel,
            display: spec.display,
            disks: spec.disks.len(),
            cdrom: spec.cdrom.is_some(),
        }
    }
}

/// The result reported by `create_instance`: the Instance reached `RUNNING`, the
/// accelerator actually chosen and why (ADR-0008), and a summary of the spec.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateInstanceReport {
    /// Lifecycle state — always `RUNNING` on success.
    pub state: String,
    /// The accelerator actually chosen (`kvm` or `tcg`).
    pub accel: String,
    /// Why that accelerator was chosen — surfaced to the agent.
    pub accel_reason: String,
    /// A summary of the validated Hardware Spec the Instance was built from.
    pub spec: SpecSummary,
}

/// The result reported by `get_status`: the running Instance's live QMP
/// `query-status` result (run state of the Guest CPUs). `runState` is the raw,
/// dynamic QMP payload (e.g. `{ "status": "running", "running": true }`); wrapping
/// it in this struct gives the tool the object-typed output schema the MCP spec
/// requires while preserving the QMP shape verbatim.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StatusReport {
    /// The raw QMP `query-status` result for the running Instance.
    pub run_state: serde_json::Value,
}

/// The result reported by `get_instance` and `destroy_instance`: the lifecycle
/// state, plus the spec/accel when an Instance exists.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceReport {
    /// Lifecycle state: `NONE`, `STARTING`, `RUNNING`, `PAUSED`, or `STOPPED`.
    pub state: String,
    /// A summary of the running Instance's Hardware Spec, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<SpecSummary>,
    /// The accelerator the running Instance was launched with, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accel: Option<String>,
}

/// The one MCP server struct. Holds the shared Orchestrator and its generated
/// [`ToolRouter`]. `Clone` is cheap: both fields are shared handles.
#[derive(Clone)]
pub struct QmpMcpServer {
    orchestrator: Arc<Mutex<Orchestrator>>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl QmpMcpServer {
    /// Construct the server over the shared `Arc<Mutex<Orchestrator>>` (the same
    /// Arc the shutdown hook holds), wiring up the generated tool router.
    pub fn new(orchestrator: Arc<Mutex<Orchestrator>>) -> Self {
        Self {
            orchestrator,
            tool_router: Self::tool_router(),
        }
    }

    /// Build, launch, and bring up a single QEMU Instance from a Hardware Spec,
    /// returning `RUNNING` on success. Rejected (with an actionable message) when an
    /// Instance already exists — only one runs at a time (ADR-0001/0004).
    #[tool(
        description = "Build and launch the single managed QEMU Instance from a Hardware Spec, \
                       bringing it to RUNNING. Rejected if an Instance already exists — destroy \
                       it first. Only one Instance runs at a time."
    )]
    async fn create_instance(
        &self,
        Parameters(spec): Parameters<HardwareSpecParams>,
    ) -> Result<Json<CreateInstanceReport>, McpError> {
        // Re-encode the typed, schema-validated params to the untrusted JSON value
        // the Orchestrator validates via the slice-2 `parse_hardware_spec` (the one
        // validation entry point), so the security rules run exactly once, centrally.
        let candidate = serde_json::to_value(&spec).map_err(|e| {
            McpError::internal_error(format!("failed to encode Hardware Spec: {e}"), None)
        })?;
        let result = {
            let mut orchestrator = self.orchestrator.lock().await;
            orchestrator.create_instance(candidate).await
        }
        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        Ok(Json(CreateInstanceReport {
            state: result.state.as_str().to_string(),
            accel: result.accel.as_str().to_string(),
            accel_reason: result.accel_reason,
            spec: SpecSummary::of(&result.spec),
        }))
    }

    /// Terminate the running Instance and return to `NONE`. Rejected when no
    /// Instance exists.
    #[tool(
        description = "Terminate the running QEMU Instance and its QMP Session, returning to NONE. \
                       Rejected when no Instance is running."
    )]
    async fn destroy_instance(&self) -> Result<Json<InstanceReport>, McpError> {
        {
            let mut orchestrator = self.orchestrator.lock().await;
            orchestrator.destroy_instance().await
        }
        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        Ok(Json(InstanceReport {
            state: InstanceState::None.as_str().to_string(),
            spec: None,
            accel: None,
        }))
    }

    /// Read-only. Report the current lifecycle state — `NONE` when nothing runs,
    /// otherwise the running Instance's state, spec summary, and accelerator.
    #[tool(
        description = "Report the managed Instance's lifecycle state (NONE, STARTING, RUNNING, \
                       PAUSED, or STOPPED). When an Instance is running, also reports its Hardware \
                       Spec summary and chosen accelerator. Never errors."
    )]
    async fn get_instance(&self) -> Result<Json<InstanceReport>, McpError> {
        let view = self.orchestrator.lock().await.get_instance();
        Ok(Json(InstanceReport {
            state: view.state.as_str().to_string(),
            spec: view.spec.as_ref().map(SpecSummary::of),
            accel: view.accel.map(|a| a.as_str().to_string()),
        }))
    }

    /// Read-only. Return the running Instance's live QMP `query-status` result (the
    /// run state of the Guest CPUs). Rejected when no Instance is running — use
    /// `get_instance` for the state when nothing is running.
    #[tool(
        description = "Return the running QEMU Instance's live QMP query-status result (run state \
                       of the Guest CPUs, e.g. running or paused). Rejected when no Instance is \
                       running — use get_instance for the lifecycle state instead."
    )]
    async fn get_status(&self) -> Result<Json<StatusReport>, McpError> {
        let run_state = {
            let orchestrator = self.orchestrator.lock().await;
            orchestrator.get_status().await
        }
        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        Ok(Json(StatusReport { run_state }))
    }
}

#[tool_handler]
impl ServerHandler for QmpMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "QEMU MCP server (Rust variant). Build, launch, drive, and tear down a single \
                 QEMU Instance over QMP. Lifecycle tools: create_instance, destroy_instance, \
                 get_instance, get_status."
                    .to_string(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            // Advertise our own identity; `Implementation::default()` would report
            // rmcp's crate name/version, so set name + version explicitly.
            server_info: Implementation {
                name: "qmp-mcp".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                ..Default::default()
            },
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::orchestrator::OrchestratorOptions;
    use crate::qemu::driver::FakeQemuDriver;

    /// A server wired to a fake-driver Orchestrator, so the wiring tests never touch
    /// a real QEMU.
    fn test_server() -> QmpMcpServer {
        let options = OrchestratorOptions {
            binary: "qemu-system-x86_64".to_string(),
            qmp_socket_path: "/run/qmp-mcp/qmp.sock".to_string(),
            image_dir: None,
            iso_dir: None,
            hostfwd_port_range: None,
            allow_host_net: false,
            max_memory_mb: None,
            max_vcpus: None,
            allow_raw_args: false,
            kvm_available: Box::new(|| false),
        };
        let orchestrator = Arc::new(Mutex::new(Orchestrator::new(
            Box::new(FakeQemuDriver::new()),
            options,
        )));
        QmpMcpServer::new(orchestrator)
    }

    #[tokio::test]
    async fn advertises_the_four_lifecycle_tools() {
        let server = test_server();
        for name in [
            "create_instance",
            "destroy_instance",
            "get_instance",
            "get_status",
        ] {
            assert!(
                server.tool_router.has_route(name),
                "{name} missing from the tool router"
            );
        }
    }

    #[tokio::test]
    async fn get_info_advertises_identity_and_tools() {
        let info = test_server().get_info();
        assert_eq!(info.server_info.name, "qmp-mcp");
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
        assert!(
            info.capabilities.tools.is_some(),
            "tools capability must be advertised"
        );
    }

    /// The tools share the one Orchestrator: create → get_instance/get_status →
    /// destroy round-trips through the server surface with the fake driver.
    #[tokio::test]
    async fn lifecycle_round_trips_through_the_tool_surface() {
        let server = test_server();

        // Nothing running yet.
        let view = server.get_instance().await.unwrap().0;
        assert_eq!(view.state, "NONE");
        assert!(server.get_status().await.is_err());

        // Create → RUNNING.
        let created = server
            .create_instance(Parameters(
                serde_json::from_value(serde_json::json!({})).unwrap(),
            ))
            .await
            .unwrap()
            .0;
        assert_eq!(created.state, "RUNNING");
        assert_eq!(created.accel, "tcg");

        let view = server.get_instance().await.unwrap().0;
        assert_eq!(view.state, "RUNNING");
        assert!(view.spec.is_some());
        let status = server.get_status().await.unwrap().0;
        assert_eq!(status.run_state["status"], "running");

        // Create-while-running is rejected through the tool too.
        assert!(server
            .create_instance(Parameters(
                serde_json::from_value(serde_json::json!({})).unwrap()
            ))
            .await
            .is_err());

        // Destroy → NONE.
        let destroyed = server.destroy_instance().await.unwrap().0;
        assert_eq!(destroyed.state, "NONE");
        assert_eq!(server.get_instance().await.unwrap().0.state, "NONE");
    }
}
