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
use std::time::Duration;

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ErrorData as McpError, Json, ServerHandler,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::instance::event_buffer::{ReadResult, WaitForEventResult};
use crate::instance::hardware_spec::{AccelMode, DisplayMode, HardwareSpec, HardwareSpecParams};
use crate::instance::image_store::{
    CreateImageRequest, CreateImageResult, ImageFormat, ImageListing, ImageStore,
};
use crate::instance::iso_store::{IsoListing, IsoStore};
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

/// A bare lifecycle-state report, returned by the curated power/pause tools
/// (`pause_instance`, `resume_instance`, `reset_instance`, `powerdown_instance`).
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StateReport {
    /// The lifecycle state after the command: `NONE`, `STARTING`, `RUNNING`,
    /// `PAUSED`, or `STOPPED`.
    pub state: String,
}

/// The result reported by `list_block_devices`: the raw QMP `query-block` result
/// (an array of the Guest's block devices and their backing media), wrapped to give
/// the tool the object-typed output schema the MCP spec requires.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BlockDevicesReport {
    /// The raw QMP `query-block` result for the running Instance.
    pub block_devices: serde_json::Value,
}

/// The result reported by `query_cpus`: the raw QMP `query-cpus-fast` result
/// (per-vCPU information), wrapped for the object-typed output schema.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CpusReport {
    /// The raw QMP `query-cpus-fast` result for the running Instance.
    pub cpus: serde_json::Value,
}

/// The result reported by `qmp_execute`: the raw QMP `return` value of the executed
/// (allowlisted) command, wrapped for the object-typed output schema.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct QmpExecuteReport {
    /// The raw QMP `return` value the allowlisted command produced.
    pub result: serde_json::Value,
}

/// Validated input for `qmp_execute`: a QMP command name and its optional
/// `arguments` object. Mirrors the TS `qmp_execute` zod schema.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct QmpExecuteParams {
    /// The QMP command name to run (e.g. `query-pci`, `query-fdsets`). Subject to the
    /// Command Policy: a default-safe allowlist with an immutable hard denylist.
    /// Dangerous commands (human-monitor-command, migrate, dump-guest-memory,
    /// device_add, …) are permanently denied.
    pub command: String,
    /// The QMP command's `arguments` object, if it takes any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<serde_json::Value>,
}

/// Upper bound on a single `wait_for_event` long-poll (ms), so a wait can never hang
/// indefinitely. Mirrors the TS `MAX_TIMEOUT_MS`.
const MAX_WAIT_TIMEOUT_MS: u64 = 600_000;

/// Default `wait_for_event` timeout (ms) when the caller supplies none. Mirrors the TS
/// schema default.
fn default_wait_timeout_ms() -> u64 {
    30_000
}

/// Validated input for `get_events`: an optional cursor to page from. Mirrors the TS
/// `get_events` zod schema (`since >= 0`, integer; the `u64` type enforces both).
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct GetEventsParams {
    /// Cursor to page from: return only events whose `seq` is greater than this. Pass
    /// the `cursor` returned by a previous get_events (or wait_for_event) call to fetch
    /// only what is new. Omit to get all currently buffered events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
}

/// Validated input for `wait_for_event`: an optional event-name filter, a timeout, and
/// an optional race-safe cursor. Mirrors the TS `wait_for_event` zod schema; the bounds
/// (`eventName` non-empty, `timeoutMs` in `0..=600000`) are enforced explicitly in the
/// tool handler, matching the hand-rolled-validation ethos.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WaitForEventParams {
    /// QMP event name to wait for (e.g. "SHUTDOWN", "POWERDOWN", "RESET", "STOP").
    /// Omit to resolve on the next event of any kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_name: Option<String>,
    /// How long to wait before returning a timed-out result (default 30000, max
    /// 600000). A timeout is a normal outcome, not an error. 0 checks without blocking.
    #[serde(default = "default_wait_timeout_ms")]
    pub timeout_ms: u64,
    /// Make the wait race-safe: also resolve on an already-buffered event whose `seq`
    /// is greater than this cursor (from a prior get_events/wait_for_event), so an
    /// event that arrived between calls is not missed. Omit for future-only (events
    /// arriving after this call).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_cursor: Option<u64>,
}

/// Validated input for `create_image`: the bare name, virtual size in GiB, and
/// image format for a new blank disk provisioned into the Image Store. Mirrors the TS
/// `create_image` zod schema (the format defaults to `qcow2`; the size cap and name
/// containment are enforced by the Image Store, not the schema).
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateImageParams {
    /// Bare name for the new image inside the Image Store (no path separators).
    pub name: String,
    /// Virtual disk size in GiB. Rejected when it exceeds `QMP_MCP_MAX_DISK_GB`.
    /// Signed so a zero/negative request yields the actionable "positive integer"
    /// message rather than a generic deserialisation error.
    pub size_gb: i64,
    /// Image format: `qcow2` (default) or `raw`.
    #[serde(default)]
    pub format: ImageFormat,
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

/// The one MCP server struct. Holds the shared Orchestrator, the two allowlisted
/// stores, and its generated [`ToolRouter`]. `Clone` is cheap: every field is a
/// shared handle or a small config value.
#[derive(Clone)]
pub struct QmpMcpServer {
    orchestrator: Arc<Mutex<Orchestrator>>,
    image_store: ImageStore,
    iso_store: IsoStore,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl QmpMcpServer {
    /// Construct the server over the shared `Arc<Mutex<Orchestrator>>` (the same
    /// Arc the shutdown hook holds) plus the read-write Image Store and read-only ISO
    /// Store (ADR-0006), wiring up the generated tool router.
    pub fn new(
        orchestrator: Arc<Mutex<Orchestrator>>,
        image_store: ImageStore,
        iso_store: IsoStore,
    ) -> Self {
        Self {
            orchestrator,
            image_store,
            iso_store,
            tool_router: Self::tool_router(),
        }
    }

    /// Build, launch, and bring up a single QEMU Instance from a Hardware Spec,
    /// returning `RUNNING` on success. Rejected (with an actionable message) when an
    /// Instance already exists — only one runs at a time (ADR-0001/0004).
    #[tool(
        description = "Build and launch the single managed QEMU Instance from a Hardware Spec and \
                       negotiate its QMP session. Reports the chosen accelerator (KVM or TCG). \
                       Rejected if an Instance already exists — destroy it first. The Guest loads \
                       PAUSED (CPUs frozen at the -S startup pause for inspection) — call \
                       resume_instance to start it, unless the server runs with \
                       QMP_MCP_AUTO_START=true (then it auto-starts)."
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

    /// Read-only. Return the running Instance's recently buffered QMP async events
    /// WITHOUT blocking — the pull half of the Event Buffer contract (issue #12).
    /// Cursor-based: the response carries a `cursor` (the latest event sequence
    /// number); pass it back as `since` to page forward. The buffer is bounded, so a
    /// slow poller may miss evicted events. Rejected when no Instance is running.
    #[tool(
        description = "Return the running QEMU Instance's recently buffered QMP async events (e.g. \
                       SHUTDOWN, STOP, RESET, POWERDOWN) without blocking. Each event has \
                       { seq, event, data?, timestamp? }. The response includes a `cursor`; pass it \
                       back as `since` to fetch only newer events. The buffer is bounded (oldest \
                       events are evicted when full). For a blocking wait, use wait_for_event. Fails \
                       if no Instance is running."
    )]
    async fn get_events(
        &self,
        Parameters(params): Parameters<GetEventsParams>,
    ) -> Result<Json<ReadResult>, McpError> {
        let result = {
            let orchestrator = self.orchestrator.lock().await;
            orchestrator.get_events(params.since)
        }
        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        Ok(Json(result))
    }

    /// Read-only. Long-poll for a matching QMP async event — the blocking half of the
    /// Event Buffer contract (issue #12). Resolves with the first matching event, or
    /// with `{ timedOut: true }` once `timeoutMs` elapses (a timeout is a NORMAL
    /// outcome, never an error). Pass `sinceCursor` to make it race-safe against events
    /// that landed between calls. Rejected only when no Instance is running.
    #[tool(
        description = "Block until the running QEMU Instance emits a matching QMP async event, then \
                       return it; or return { timedOut: true } if none arrives within timeoutMs (a \
                       timeout is a normal result, not an error). Provide eventName to filter (e.g. \
                       \"SHUTDOWN\"), or omit it to wait for any event. Pass sinceCursor (a prior \
                       cursor) to also catch events already buffered since then, so nothing is missed \
                       between calls. Useful for \"has the Guest booted/shut down yet?\". Fails if no \
                       Instance is running."
    )]
    async fn wait_for_event(
        &self,
        Parameters(params): Parameters<WaitForEventParams>,
    ) -> Result<Json<WaitForEventResult>, McpError> {
        // Explicit bounds (mirroring the TS zod schema): a filter, when given, must be
        // a non-empty event name, and the timeout is capped so a wait cannot hang.
        if params.event_name.as_deref() == Some("") {
            return Err(McpError::invalid_params(
                "eventName, when provided, must be a non-empty QMP event name (e.g. \"SHUTDOWN\"); \
                 omit it to wait for the next event of any kind."
                    .to_string(),
                None,
            ));
        }
        if params.timeout_ms > MAX_WAIT_TIMEOUT_MS {
            return Err(McpError::invalid_params(
                format!(
                    "timeoutMs must be between 0 and {MAX_WAIT_TIMEOUT_MS} (got {}). Use a smaller \
                     timeout, or poll repeatedly with get_events.",
                    params.timeout_ms
                ),
                None,
            ));
        }
        // Register the waiter under the orchestrator lock, then release the lock and
        // await the long-poll — so a pending wait never holds the single Orchestrator
        // mutex (which would serialise out every other tool for the whole timeout).
        let future = {
            let orchestrator = self.orchestrator.lock().await;
            orchestrator.wait_for_event(
                params.event_name,
                Some(Duration::from_millis(params.timeout_ms)),
                params.since_cursor,
            )
        }
        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        Ok(Json(future.await))
    }

    /// Pause the running Instance's Guest CPUs (QMP `stop`), moving the lifecycle to
    /// PAUSED. Reversible with `resume_instance`. Rejected when no Instance is running.
    #[tool(
        description = "Pause the running QEMU Instance's Guest CPUs (QMP stop), moving its lifecycle \
                       state to PAUSED. Reversible with resume_instance. Fails if no Instance is running."
    )]
    async fn pause_instance(&self) -> Result<Json<StateReport>, McpError> {
        let state = {
            let mut orchestrator = self.orchestrator.lock().await;
            orchestrator.pause_instance().await
        }
        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        Ok(Json(StateReport {
            state: state.as_str().to_string(),
        }))
    }

    /// Resume the paused Instance's Guest CPUs (QMP `cont`), moving the lifecycle back
    /// to RUNNING. Rejected when no Instance is running.
    #[tool(
        description = "Resume the paused QEMU Instance's Guest CPUs (QMP cont), moving its lifecycle \
                       state back to RUNNING. Fails if no Instance is running."
    )]
    async fn resume_instance(&self) -> Result<Json<StateReport>, McpError> {
        let state = {
            let mut orchestrator = self.orchestrator.lock().await;
            orchestrator.resume_instance().await
        }
        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        Ok(Json(StateReport {
            state: state.as_str().to_string(),
        }))
    }

    /// Hard-reset the running Instance (QMP `system_reset`), rebooting the Guest in
    /// place. Unsaved Guest state is lost; the lifecycle state is unchanged. Rejected
    /// when no Instance is running.
    #[tool(
        description = "Hard-reset the running QEMU Instance (QMP system_reset), rebooting the Guest in \
                       place. Unsaved Guest state is lost. Fails if no Instance is running."
    )]
    async fn reset_instance(&self) -> Result<Json<StateReport>, McpError> {
        let state = {
            let orchestrator = self.orchestrator.lock().await;
            orchestrator.reset_instance().await
        }
        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        Ok(Json(StateReport {
            state: state.as_str().to_string(),
        }))
    }

    /// Request a graceful Guest shutdown (QMP `system_powerdown`, an ACPI power-button
    /// event). The Guest decides when to power off, so the lifecycle state is
    /// unchanged. Rejected when no Instance is running.
    #[tool(
        description = "Request a graceful Guest shutdown of the running QEMU Instance via an ACPI \
                       power-button event (QMP system_powerdown). The Guest decides when to power \
                       off. Fails if no Instance is running."
    )]
    async fn powerdown_instance(&self) -> Result<Json<StateReport>, McpError> {
        let state = {
            let orchestrator = self.orchestrator.lock().await;
            orchestrator.powerdown_instance().await
        }
        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        Ok(Json(StateReport {
            state: state.as_str().to_string(),
        }))
    }

    /// Read-only. Return the running Instance's block (storage) devices and their
    /// backing media (QMP `query-block`). Rejected when no Instance is running.
    #[tool(
        description = "Return the running QEMU Instance's block (storage) devices and their backing \
                       media (QMP query-block). Read-only. Fails if no Instance is running."
    )]
    async fn list_block_devices(&self) -> Result<Json<BlockDevicesReport>, McpError> {
        let block_devices = {
            let orchestrator = self.orchestrator.lock().await;
            orchestrator.query_block().await
        }
        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        Ok(Json(BlockDevicesReport { block_devices }))
    }

    /// Read-only. Return per-vCPU information for the running Instance's Guest (QMP
    /// `query-cpus-fast`). Rejected when no Instance is running.
    #[tool(
        description = "Return per-vCPU information for the running QEMU Instance's Guest (QMP \
                       query-cpus-fast). Read-only. Fails if no Instance is running."
    )]
    async fn query_cpus(&self) -> Result<Json<CpusReport>, McpError> {
        let cpus = {
            let orchestrator = self.orchestrator.lock().await;
            orchestrator.query_cpus().await
        }
        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        Ok(Json(CpusReport { cpus }))
    }

    /// Capture a screenshot of the running Instance's display and return it as a PNG
    /// image. The destination file is ALWAYS server-chosen (a single-use temp file),
    /// read back, and deleted — QMP `screendump` writes an arbitrary host file, so the
    /// path is never agent-supplied (ADR-0003). Rejected when no Instance is running.
    #[tool(
        description = "Capture a screenshot of the running QEMU Instance's display and return it as a \
                       PNG image (QMP screendump to a server-chosen path). Fails if no Instance is \
                       running."
    )]
    async fn screendump(&self) -> Result<CallToolResult, McpError> {
        let shot = {
            let orchestrator = self.orchestrator.lock().await;
            orchestrator.screendump().await
        }
        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::image(
            shot.data,
            shot.mime_type,
        )]))
    }

    /// Run an arbitrary QMP command against the running Instance, gated by the Command
    /// Policy (ADR-0003). The command name is checked BEFORE it can reach the QMP
    /// Session: a denied command returns an actionable error and never touches QEMU;
    /// hard-denied commands can never be enabled. Rejected when no Instance is running
    /// or the command is not permitted.
    #[tool(
        description = "Run an arbitrary QMP command against the running QEMU Instance, subject to the \
                       Command Policy (a default-safe allowlist plus an immutable hard denylist). \
                       Provide the QMP command name and optional arguments object. Dangerous commands \
                       (e.g. human-monitor-command, migrate, dump-guest-memory, device_add) are \
                       permanently denied and cannot be enabled. Fails if no Instance is running or \
                       the command is not permitted."
    )]
    async fn qmp_execute(
        &self,
        Parameters(params): Parameters<QmpExecuteParams>,
    ) -> Result<Json<QmpExecuteReport>, McpError> {
        let result = {
            let orchestrator = self.orchestrator.lock().await;
            orchestrator
                .execute_command(&params.command, params.arguments)
                .await
        }
        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        Ok(Json(QmpExecuteReport { result }))
    }

    /// Create a blank disk image of the given name, size (GiB), and format inside the
    /// read-write Image Store via `qemu-img create` (ADR-0006). Enforces the
    /// `QMP_MCP_MAX_DISK_GB` size cap and rejects any name that escapes the Store
    /// (absolute paths, `..`/separator traversal, injection characters, symlink
    /// escape) or collides with an existing image.
    #[tool(
        description = "Create a blank disk image of the given name, size (GiB), and format (qcow2 \
                       or raw) inside the Image Store using qemu-img. Enforces the \
                       QMP_MCP_MAX_DISK_GB size cap and rejects names that escape the Store."
    )]
    async fn create_image(
        &self,
        Parameters(params): Parameters<CreateImageParams>,
    ) -> Result<Json<CreateImageResult>, McpError> {
        let result = self
            .image_store
            .create(CreateImageRequest {
                name: params.name,
                size_gb: params.size_gb,
                format: params.format,
            })
            .await
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        Ok(Json(result))
    }

    /// Read-only. List the guest disk images available in the Image Store, by name —
    /// the names a Hardware Spec disk references (ADR-0006). Fails closed with an
    /// actionable message naming `QMP_MCP_IMAGE_DIR` when the Store is missing.
    #[tool(
        description = "List the guest disk images available in the Image Store, by name. These \
                       names are what a disk in the Hardware Spec references. Read-only."
    )]
    async fn list_images(&self) -> Result<Json<ImageListing>, McpError> {
        let listing = self
            .image_store
            .list()
            .await
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        Ok(Json(listing))
    }

    /// Read-only. List the installation/boot ISO media available in the strictly
    /// read-only ISO Store, by name — the names a Hardware Spec cdrom references
    /// (ADR-0006). Fails closed with an actionable message naming `QMP_MCP_ISO_DIR`
    /// when the Store is missing.
    #[tool(
        description = "List the installation/boot ISO media available in the read-only ISO Store, \
                       by name. These names are what a Hardware Spec cdrom references. Read-only."
    )]
    async fn list_isos(&self) -> Result<Json<IsoListing>, McpError> {
        let listing = self
            .iso_store
            .list()
            .await
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        Ok(Json(listing))
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
    use crate::instance::image_store::ImageStoreOptions;
    use crate::instance::orchestrator::OrchestratorOptions;
    use crate::qemu::driver::FakeQemuDriver;
    use crate::qemu::qmp_client::QmpEvent;
    use tokio::sync::broadcast;

    /// A handle onto the current fake Instance's synthetic-event sender, so a wiring
    /// test can emit QMP events into the running Instance's stream.
    type EventSlot = Arc<std::sync::Mutex<Option<broadcast::Sender<QmpEvent>>>>;

    /// A server wired to a fake-driver Orchestrator, so the wiring tests never touch
    /// a real QEMU. The stores point at non-existent directories: the wiring tests
    /// only check routing, and the store methods fail closed (which is asserted
    /// exhaustively in the store modules' own tests). Returns the fake driver's event
    /// slot too, so an event-surface test can emit synthetic QMP events.
    fn test_server_with_events() -> (QmpMcpServer, EventSlot) {
        let driver = FakeQemuDriver::new();
        let events = driver.events_slot();
        let options = OrchestratorOptions {
            qemu_binary_override: Some("qemu-system-x86_64".to_string()),
            host_arch: "x86_64".to_string(),
            qmp_socket_path: "/run/qmp-mcp/qmp.sock".to_string(),
            image_dir: None,
            iso_dir: None,
            hostfwd_port_range: None,
            allow_host_net: false,
            auto_start: false,
            max_memory_mb: None,
            max_vcpus: None,
            allow_raw_args: false,
            command_policy: None,
            event_buffer_size: None,
            viewer_password: None,
            viewer_host: "127.0.0.1".to_string(),
            viewer_port: 6080,
            start_viewer: None,
            kvm_available: Box::new(|| false),
        };
        let orchestrator = Orchestrator::new_shared(Box::new(driver), options);
        let image_store = ImageStore::new(ImageStoreOptions {
            dir: "/nonexistent/qmp-mcp-image-store".to_string(),
            max_disk_gb: 64,
            qemu_img_binary: None,
            run: None,
        });
        let iso_store = IsoStore::new("/nonexistent/qmp-mcp-iso-store".to_string());
        (
            QmpMcpServer::new(orchestrator, image_store, iso_store),
            events,
        )
    }

    /// The common case: just the server, discarding the event slot.
    fn test_server() -> QmpMcpServer {
        test_server_with_events().0
    }

    #[tokio::test]
    async fn advertises_the_lifecycle_and_qmp_tools() {
        let server = test_server();
        for name in [
            // Lifecycle (earlier slices).
            "create_instance",
            "destroy_instance",
            "get_instance",
            "get_status",
            // Event Buffer surface (this slice).
            "get_events",
            "wait_for_event",
            // Command Policy + curated QMP tools (this slice).
            "pause_instance",
            "resume_instance",
            "reset_instance",
            "powerdown_instance",
            "list_block_devices",
            "query_cpus",
            "screendump",
            "qmp_execute",
            // Image/ISO stores (this slice).
            "create_image",
            "list_images",
            "list_isos",
        ] {
            assert!(
                server.tool_router.has_route(name),
                "{name} missing from the tool router"
            );
        }
    }

    /// The store tools round-trip through the server surface: `list_images` and
    /// `list_isos` fail closed (their configured dirs do not exist), and
    /// `create_image` rejects a traversing name before it can touch the filesystem.
    #[tokio::test]
    async fn store_tools_surface_failures_actionably() {
        let server = test_server();

        // `Json<T>` success is not `Debug`, so match rather than `unwrap_err`.
        let Err(err) = server.list_images().await else {
            panic!("list_images must fail closed on a missing store");
        };
        assert!(err.to_string().contains("QMP_MCP_IMAGE_DIR"));

        let Err(err) = server.list_isos().await else {
            panic!("list_isos must fail closed on a missing store");
        };
        assert!(err.to_string().contains("QMP_MCP_ISO_DIR"));

        let Err(err) = server
            .create_image(Parameters(CreateImageParams {
                name: "../escape.qcow2".to_string(),
                size_gb: 1,
                format: ImageFormat::Qcow2,
            }))
            .await
        else {
            panic!("create_image must reject a traversing name");
        };
        assert!(err.to_string().contains("path separator"));
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

        // Create → PAUSED (loaded, frozen at -S; issue #10) — auto-start off.
        let created = server
            .create_instance(Parameters(
                serde_json::from_value(serde_json::json!({})).unwrap(),
            ))
            .await
            .unwrap()
            .0;
        assert_eq!(created.state, "PAUSED");
        assert_eq!(created.accel, "tcg");

        let view = server.get_instance().await.unwrap().0;
        assert_eq!(view.state, "PAUSED");
        assert!(view.spec.is_some());
        // get_status (live query-status) agrees: the Guest is not executing.
        let status = server.get_status().await.unwrap().0;
        assert_eq!(status.run_state["status"], "paused");

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

    /// The curated QMP tools round-trip through the server surface with the fake
    /// driver: pause/resume flip the state, reset/powerdown leave it, and the
    /// read-only queries and screendump return their payloads.
    #[tokio::test]
    async fn curated_qmp_tools_round_trip_through_the_tool_surface() {
        let server = test_server();
        // Before an Instance exists every curated tool is refused.
        assert!(server.pause_instance().await.is_err());
        assert!(server.list_block_devices().await.is_err());
        assert!(server.screendump().await.is_err());

        server
            .create_instance(Parameters(
                serde_json::from_value(serde_json::json!({})).unwrap(),
            ))
            .await
            .unwrap();

        assert_eq!(server.pause_instance().await.unwrap().0.state, "PAUSED");
        assert_eq!(server.resume_instance().await.unwrap().0.state, "RUNNING");
        assert_eq!(server.reset_instance().await.unwrap().0.state, "RUNNING");
        assert_eq!(
            server.powerdown_instance().await.unwrap().0.state,
            "RUNNING"
        );

        let block = server.list_block_devices().await.unwrap().0;
        assert_eq!(block.block_devices[0]["device"], "virtio0");
        let cpus = server.query_cpus().await.unwrap().0;
        assert_eq!(cpus.cpus[0]["cpu-index"], 0);

        // screendump returns MCP image content, not a host path.
        let shot = server.screendump().await.unwrap();
        assert_eq!(shot.is_error, Some(false));
        assert_eq!(shot.content.len(), 1);
    }

    /// `qmp_execute` forwards an allowlisted command and refuses a hard-denied one with
    /// an actionable reason — the generic escape hatch, gated by the Command Policy.
    #[tokio::test]
    async fn qmp_execute_allows_allowlisted_and_denies_hard_denied_through_the_surface() {
        let server = test_server();
        server
            .create_instance(Parameters(
                serde_json::from_value(serde_json::json!({})).unwrap(),
            ))
            .await
            .unwrap();

        // An allowlisted read-only command executes and returns its QMP result.
        let ok = server
            .qmp_execute(Parameters(QmpExecuteParams {
                command: "query-status".to_string(),
                arguments: None,
            }))
            .await
            .unwrap()
            .0;
        // Auto-start off, so the Guest is paused; query-status forwards that faithfully.
        assert_eq!(ok.result["status"], "paused");

        // A hard-denied command is refused with a reason naming the denylist. (A
        // `let else` avoids requiring `Debug` on the `Json` success type.)
        let Err(err) = server
            .qmp_execute(Parameters(QmpExecuteParams {
                command: "migrate".to_string(),
                arguments: None,
            }))
            .await
        else {
            panic!("migrate must be refused");
        };
        assert!(err.to_string().contains("hard denylist"), "got: {err}");

        // A default-denied command is refused as not-allowlisted (not a hard denial).
        // The dangerous filename argument is ignored — the policy gates the NAME.
        let Err(err) = server
            .qmp_execute(Parameters(QmpExecuteParams {
                command: "screendump".to_string(),
                arguments: Some(serde_json::json!({ "filename": "/etc/shadow" })),
            }))
            .await
        else {
            panic!("screendump must be refused generically");
        };
        assert!(
            err.to_string()
                .contains("not in the Command Policy allowlist"),
            "got: {err}"
        );
    }

    /// The event tools round-trip through the server surface with the fake driver:
    /// they reject when no Instance runs, validate their inputs, and — once an Instance
    /// is up — surface a synthetic QMP event through both `wait_for_event` (blocking
    /// match) and `get_events` (cursor paging).
    #[tokio::test]
    async fn event_tools_round_trip_through_the_tool_surface() {
        let (server, slot) = test_server_with_events();

        // Before an Instance exists both event tools are refused.
        let Err(err) = server
            .get_events(Parameters(GetEventsParams { since: None }))
            .await
        else {
            panic!("get_events must fail before an Instance exists");
        };
        assert!(err.to_string().contains("read its events"), "got: {err}");
        assert!(server
            .wait_for_event(Parameters(WaitForEventParams {
                event_name: None,
                timeout_ms: 0,
                since_cursor: None,
            }))
            .await
            .is_err());

        // Input validation is enforced before any Instance is consulted.
        assert!(server
            .wait_for_event(Parameters(WaitForEventParams {
                event_name: Some(String::new()),
                timeout_ms: 0,
                since_cursor: None,
            }))
            .await
            .is_err());
        assert!(server
            .wait_for_event(Parameters(WaitForEventParams {
                event_name: None,
                timeout_ms: MAX_WAIT_TIMEOUT_MS + 1,
                since_cursor: None,
            }))
            .await
            .is_err());

        server
            .create_instance(Parameters(
                serde_json::from_value(serde_json::json!({})).unwrap(),
            ))
            .await
            .unwrap();

        let events = slot
            .lock()
            .unwrap()
            .clone()
            .expect("create_instance installs an event sender");
        events
            .send(QmpEvent {
                event: "SHUTDOWN".to_string(),
                data: Some(serde_json::json!({ "guest": true })),
                timestamp: None,
            })
            .unwrap();

        // wait_for_event with sinceCursor=0 is race-safe against the feeder: it resolves
        // whether the event was already buffered or arrives after registration.
        let waited = server
            .wait_for_event(Parameters(WaitForEventParams {
                event_name: Some("SHUTDOWN".to_string()),
                timeout_ms: 1_000,
                since_cursor: Some(0),
            }))
            .await
            .unwrap()
            .0;
        assert!(!waited.timed_out);
        let event = waited.event.expect("a matching event");
        assert_eq!(event.event, "SHUTDOWN");
        assert_eq!(event.data, Some(serde_json::json!({ "guest": true })));

        // get_events now returns the buffered event and a cursor; paging past it is empty.
        let read = server
            .get_events(Parameters(GetEventsParams { since: None }))
            .await
            .unwrap()
            .0;
        assert_eq!(read.events.len(), 1);
        assert_eq!(read.events[0].event, "SHUTDOWN");
        let after = server
            .get_events(Parameters(GetEventsParams {
                since: Some(read.cursor),
            }))
            .await
            .unwrap()
            .0;
        assert!(after.events.is_empty());
    }
}
