//! The single-instance lifecycle Orchestrator (ADR-0001/0004). It holds the one
//! managed Instance and drives it through its lifecycle:
//!
//!   NONE → STARTING → RUNNING ⇄ PAUSED → STOPPED → NONE
//!
//! A second implementation of the shared bounded context, mirroring
//! `../../src/instance/orchestrator.ts`: same state names, same reject-while-running
//! wording, same instance-lifetime = server-lifetime teardown.
//!
//! The Orchestrator depends on the [`QemuDriver`] port by injection (it holds a
//! `Box<dyn QemuDriver>`), so its whole lifecycle is testable against the fake
//! driver with no real QEMU. Crucially (ADR-0011), the Orchestrator lives behind an
//! `Arc<Mutex<Orchestrator>>`: because a caller holds the async mutex for the whole
//! duration of `create_instance` — including the `await` on `driver.launch` — two
//! concurrent create attempts cannot interleave. The second one only runs once the
//! first has fully committed to RUNNING, so it observes the occupied slot and is
//! rejected. This makes the create-time TOCTOU *structurally impossible*, so the
//! launch-token bookkeeping the single-threaded-async TypeScript port needs is not
//! required here.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::config::PortRange;

use super::event_buffer::{
    EventBuffer, ReadResult, WaitForEventOptions, WaitFuture, DEFAULT_EVENT_BUFFER_SIZE,
};
use super::hardware_spec::{
    build_argv, parse_hardware_spec, resolve_accel, Accel, AccelResolution, ArgvOptions,
    HardwareSpec,
};
use crate::policy::{
    build_policy, decide_command, CommandPolicyError, PolicyOverrides, ResolvedPolicy,
};
use crate::qemu::driver::{InstanceHandle, LaunchRequest, QemuDriver};

/// The lifecycle states an Instance moves through. `PAUSED` is entered by
/// [`Orchestrator::pause_instance`] (QMP `stop`) and left by
/// [`Orchestrator::resume_instance`] (QMP `cont`). The names match the TS
/// `InstanceState` union exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceState {
    /// No Instance exists.
    None,
    /// A create is in flight (the single slot is reserved).
    Starting,
    /// An Instance is running with its Guest CPUs live.
    Running,
    /// An Instance is running with its Guest CPUs paused (QMP `stop`).
    Paused,
    /// An Instance is being torn down (its slot is being released).
    Stopped,
}

impl InstanceState {
    /// The canonical UPPERCASE spelling, matching the TS state union and used in
    /// tool responses and actionable messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "NONE",
            Self::Starting => "STARTING",
            Self::Running => "RUNNING",
            Self::Paused => "PAUSED",
            Self::Stopped => "STOPPED",
        }
    }
}

impl std::fmt::Display for InstanceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A read-only view of the current Instance for the tools to return.
#[derive(Debug, Clone)]
pub struct InstanceView {
    /// Current lifecycle state.
    pub state: InstanceState,
    /// The validated Hardware Spec, when an Instance exists.
    pub spec: Option<HardwareSpec>,
    /// The accelerator the running Instance was launched with.
    pub accel: Option<Accel>,
}

/// The result of a successful [`Orchestrator::create_instance`].
#[derive(Debug, Clone)]
pub struct CreateInstanceResult {
    /// Always [`InstanceState::Running`] on success.
    pub state: InstanceState,
    /// The validated Hardware Spec the Instance was built from.
    pub spec: HardwareSpec,
    /// The accelerator actually chosen (KVM or TCG).
    pub accel: Accel,
    /// Why that accelerator was chosen — reported to the agent (ADR-0008).
    pub accel_reason: String,
}

/// A captured Instance screenshot. The image bytes are returned inline (base64)
/// rather than as a host path: the agent never learns or controls where the file
/// lived, and the server deletes it after reading (see [`Orchestrator::screendump`]).
/// Mirrors the TS `ScreendumpResult`.
#[derive(Debug, Clone)]
pub struct ScreendumpResult {
    /// MIME type of the captured image (always `image/png`).
    pub mime_type: String,
    /// Base64-encoded image bytes, ready to hand back as MCP image content.
    pub data: String,
    /// Size of the decoded image in bytes.
    pub bytes: usize,
}

/// Knobs the Orchestrator needs that are not part of the Hardware Spec. The
/// singleton injects the env-resolved values (mirrors the TS `OrchestratorOptions`,
/// trimmed to what this slice's `build_argv` + accel resolution consume).
pub struct OrchestratorOptions {
    /// The `qemu-system-*` binary to launch.
    pub binary: String,
    /// Server-managed path of the QMP UNIX socket.
    pub qmp_socket_path: String,
    /// Absolute path of the Image Store directory (ADR-0006); required by a spec
    /// with disks.
    pub image_dir: Option<String>,
    /// Absolute path of the read-only ISO Store directory (ADR-0006); required by a
    /// spec with a cdrom.
    pub iso_dir: Option<String>,
    /// Inclusive host-port range a user-mode forward's `hostPort` must fall within
    /// (ADR-0009); `None` uses the argv builder's default.
    pub hostfwd_port_range: Option<PortRange>,
    /// Whether host-level (`tap`/`bridge`) networking is permitted (ADR-0009).
    pub allow_host_net: bool,
    /// Hard cap, in MiB, on the spec's `memoryMb` (issue #9); `None` skips it.
    pub max_memory_mb: Option<u32>,
    /// Hard cap on the spec's `vcpus` (issue #9); `None` skips it.
    pub max_vcpus: Option<u32>,
    /// Whether the raw-args escape hatch is enabled (`QMP_MCP_ALLOW_RAW_ARGS`,
    /// ADR-0002).
    pub allow_raw_args: bool,
    /// The resolved Command Policy governing which QMP commands the generic
    /// [`Orchestrator::execute_command`] path may run (ADR-0003). `None` uses the
    /// built-in default-safe allowlist (the singleton injects the env/file-resolved
    /// policy).
    pub command_policy: Option<ResolvedPolicy>,
    /// Capacity of the Event Buffer capturing the Instance's QMP async events
    /// (`QMP_MCP_EVENT_BUFFER_SIZE`, issue #12). `None` uses
    /// [`DEFAULT_EVENT_BUFFER_SIZE`] (the singleton injects the env-resolved value).
    pub event_buffer_size: Option<u32>,
    /// Probe for KVM availability (injected for testability; production passes the
    /// `/dev/kvm` probe, tests force a deterministic value).
    pub kvm_available: Box<dyn Fn() -> bool + Send + Sync>,
}

/// Raised for lifecycle violations (creating while an Instance exists, destroying
/// when none does, driving a non-existent Instance) and for any create-time
/// validation/launch failure. The message is always actionable — it names the cause
/// and the remediation — mirroring the TS `LifecycleError`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct LifecycleError(pub String);

/// The failure modes of the generic [`Orchestrator::execute_command`] path: either the
/// Command Policy refused the command (fail-closed, before it ever reached QEMU) or a
/// lifecycle/driver failure occurred while forwarding an allowed command. Both carry an
/// actionable message; the [`CommandPolicyError`] variant additionally preserves the
/// `hard_denied` flag. Mirrors the TS split between `CommandPolicyError` and
/// `LifecycleError` on the `executeCommand` path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExecuteCommandError {
    /// The command was refused by the Command Policy (never reached QEMU).
    #[error(transparent)]
    Policy(#[from] CommandPolicyError),
    /// No Instance is running, or the QMP round-trip for an allowed command failed.
    #[error(transparent)]
    Lifecycle(#[from] LifecycleError),
}

/// Default `wait_for_event` timeout when a caller supplies none (issue #12). A
/// long-poll horizon: long enough to catch a boot/shutdown, short enough that the
/// agent regains control to poll again. Mirrors the TS `DEFAULT_WAIT_TIMEOUT_MS`.
const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_millis(30_000);

/// Holds the single managed Instance: exactly one exists at a time. Requesting a
/// new Instance while one exists is rejected rather than auto-replaced (ADR-0004).
/// Not `Clone` and not thread-safe on its own — it is shared as an
/// `Arc<Mutex<Orchestrator>>`, which is what serialises concurrent tool calls.
pub struct Orchestrator {
    driver: Box<dyn QemuDriver>,
    options: OrchestratorOptions,
    /// The Command Policy gating [`execute_command`](Self::execute_command); defaults
    /// to the built-in allowlist when the options omit one.
    command_policy: ResolvedPolicy,
    state: InstanceState,
    handle: Option<Box<dyn InstanceHandle>>,
    spec: Option<HardwareSpec>,
    accel: Option<Accel>,
    /// The Event Buffer capturing the current Instance's QMP async events. One buffer
    /// lives for the server's lifetime; it is [`EventBuffer::reset`] on every
    /// create/destroy so events never bleed across Instances (issue #12). Shared with
    /// the feeder task, hence `Arc`.
    event_buffer: Arc<EventBuffer>,
    /// The background task draining the current Instance's async QMP events into the
    /// Event Buffer. Aborted (and cleared) on destroy, so the buffer stops advancing
    /// when the Instance is gone.
    event_feeder: Option<JoinHandle<()>>,
}

impl Orchestrator {
    /// Construct an Orchestrator over an injected [`QemuDriver`]. Starts in
    /// [`InstanceState::None`] with no Instance. Resolves the Command Policy once: an
    /// omitted policy means the built-in default-safe allowlist.
    pub fn new(driver: Box<dyn QemuDriver>, mut options: OrchestratorOptions) -> Self {
        let command_policy = options
            .command_policy
            .take()
            .unwrap_or_else(|| build_policy(&PolicyOverrides::default()));
        // One Event Buffer for the server's lifetime, sized from the env-resolved
        // option (or the default). It is reset — never re-created — per Instance.
        let capacity = options
            .event_buffer_size
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_EVENT_BUFFER_SIZE);
        Self {
            driver,
            options,
            command_policy,
            state: InstanceState::None,
            handle: None,
            spec: None,
            accel: None,
            event_buffer: Arc::new(EventBuffer::new(capacity)),
            event_feeder: None,
        }
    }

    /// The current lifecycle state (a cheap accessor for the shutdown hook and
    /// tests, avoiding an [`InstanceView`] clone).
    pub fn state(&self) -> InstanceState {
        self.state
    }

    /// Return the current Instance view. Reports `NONE` when nothing is running.
    pub fn get_instance(&self) -> InstanceView {
        InstanceView {
            state: self.state,
            spec: self.spec.clone(),
            accel: self.accel,
        }
    }

    /// Build and launch a new Instance from an untrusted candidate Hardware Spec,
    /// negotiate its QMP Session (owned by the returned handle), and bring it to
    /// `RUNNING`. Rejects with an actionable message when an Instance already
    /// exists. A validation or launch failure releases the reserved slot back to
    /// `NONE`, so a later create can proceed.
    pub async fn create_instance(
        &mut self,
        candidate: serde_json::Value,
    ) -> Result<CreateInstanceResult, LifecycleError> {
        if self.state != InstanceState::None {
            return Err(LifecycleError(format!(
                "An Instance already exists (state {}). Only one Instance may run at a time — \
                 destroy it with destroy_instance before creating a new one.",
                self.state
            )));
        }

        // Reserve the single slot. Because the whole Orchestrator lives behind an
        // `Arc<Mutex>`, this method runs to completion under the caller's lock, so
        // no concurrent create can observe STARTING and race us — the create-time
        // TOCTOU is structurally impossible (ADR-0011).
        self.state = InstanceState::Starting;
        match self.launch_instance(candidate).await {
            Ok((spec, resolution, handle)) => {
                // Start a fresh Event Buffer for this Instance and capture its QMP
                // async events for its whole life — no events carry over from a
                // previous Instance (the buffer's cursor stays monotonic, though).
                self.event_buffer.reset();
                self.spawn_event_feeder(handle.subscribe_events());
                self.handle = Some(handle);
                self.spec = Some(spec.clone());
                self.accel = Some(resolution.accel);
                self.state = InstanceState::Running;
                tracing::info!("Instance RUNNING ({})", resolution.reason);
                Ok(CreateInstanceResult {
                    state: InstanceState::Running,
                    spec,
                    accel: resolution.accel,
                    accel_reason: resolution.reason,
                })
            }
            Err(err) => {
                // Validation or launch failed: free the reserved slot (nothing was
                // published, so there is no handle to tear down).
                self.state = InstanceState::None;
                Err(err)
            }
        }
    }

    /// The fallible half of [`create_instance`](Self::create_instance): validate the
    /// candidate spec (slice-2 parser), resolve the accelerator, generate the argv,
    /// and launch via the driver. Borrows `&self` only, so it commits nothing — the
    /// caller publishes the Instance (or releases the slot) based on the outcome.
    async fn launch_instance(
        &self,
        candidate: serde_json::Value,
    ) -> Result<(HardwareSpec, AccelResolution, Box<dyn InstanceHandle>), LifecycleError> {
        let spec = parse_hardware_spec(candidate).map_err(|e| LifecycleError(e.0))?;
        let resolution = resolve_accel(spec.accel, || (self.options.kvm_available)())
            .map_err(|e| LifecycleError(e.0))?;
        let argv = build_argv(&spec, &self.argv_options(resolution.accel))
            .map_err(|e| LifecycleError(e.0))?;
        tracing::info!(
            "creating Instance (machine={}, accel={})",
            spec.machine.as_str(),
            resolution.accel.as_str()
        );
        let handle = self
            .driver
            .launch(LaunchRequest {
                binary: self.options.binary.clone(),
                argv,
                qmp_socket_path: self.options.qmp_socket_path.clone(),
            })
            .await
            .map_err(|e| LifecycleError(format!("Failed to create the Instance: {}", e.0)))?;
        Ok((spec, resolution, handle))
    }

    /// Assemble the [`ArgvOptions`] the pure argv generator needs from this slice's
    /// options plus the resolved accelerator.
    fn argv_options(&self, accel: Accel) -> ArgvOptions {
        ArgvOptions {
            accel,
            qmp_socket_path: self.options.qmp_socket_path.clone(),
            image_dir: self.options.image_dir.clone(),
            iso_dir: self.options.iso_dir.clone(),
            hostfwd_port_range: self.options.hostfwd_port_range,
            allow_host_net: self.options.allow_host_net,
            max_memory_mb: self.options.max_memory_mb,
            max_vcpus: self.options.max_vcpus,
            allow_raw_args: self.options.allow_raw_args,
        }
    }

    /// Terminate the running Instance's process, close its QMP Session, and return
    /// to `NONE`. Rejects when no Instance exists. State returns to `NONE` even if
    /// the driver's `close` reports an error (mirrors the TS `finally`).
    pub async fn destroy_instance(&mut self) -> Result<(), LifecycleError> {
        if self.state == InstanceState::None || self.handle.is_none() {
            return Err(LifecycleError(
                "No Instance is running, so there is nothing to destroy. Create one with \
                 create_instance first."
                    .to_string(),
            ));
        }
        // Claim the teardown: take the handle and clear the Instance fields before
        // awaiting close. (The outer mutex already serialises callers, so this
        // cannot interleave with another destroy.)
        let handle = self
            .handle
            .take()
            .expect("handle present in a non-NONE state");
        self.spec = None;
        self.accel = None;
        self.state = InstanceState::Stopped;
        // Detach from the Instance's event stream and clear the buffer (settling any
        // pending wait_for_event as a clean timeout); events do not outlive the
        // Instance. Mirrors the TS destroy path.
        self.stop_event_feeder();
        self.event_buffer.reset();
        tracing::info!("destroying Instance");
        let closed = handle.close().await;
        self.state = InstanceState::None;
        closed.map_err(|e| LifecycleError(format!("Failed to destroy the Instance: {}", e.0)))?;
        tracing::info!("Instance destroyed (state NONE)");
        Ok(())
    }

    /// Return the live QMP `query-status` result for the running Instance (the run
    /// state of the Guest CPUs). With the fake driver this is the tracked run-state;
    /// with the real driver (slice #21) it is an actual QMP round-trip. Rejects when
    /// no Instance is running.
    pub async fn get_status(&self) -> Result<serde_json::Value, LifecycleError> {
        self.require_handle("query its status")?
            .execute("query-status", None)
            .await
            .map_err(|e| LifecycleError(e.0))
    }

    /// Return the Instance's recently buffered QMP async events WITHOUT blocking (the
    /// `get_events` tool). Cursor-based: with no `since`, returns every buffered event
    /// plus a `cursor`; passing that `cursor` back as `since` next time pages forward
    /// without missing or repeating events. The buffer is bounded, so a slow poller may
    /// miss evicted events — a gap the monotonic cursor makes visible. Rejects when no
    /// Instance is running. Mirrors the TS `Orchestrator.getEvents`.
    pub fn get_events(&self, since: Option<u64>) -> Result<ReadResult, LifecycleError> {
        self.require_handle("read its events")?;
        Ok(self.event_buffer.read(since))
    }

    /// Long-poll for a matching QMP async event (the `wait_for_event` tool). Rejects
    /// only when no Instance is running; otherwise returns a [`WaitFuture`] that
    /// resolves — never rejects — with the first matching event, or with
    /// `{ timed_out: true }` once the timeout elapses (a timeout is a NORMAL outcome).
    /// With no `event_name` any event matches. Pass `since_cursor` (a prior `cursor`)
    /// to also consider already-buffered events, so an event that arrived between calls
    /// is not lost; without it the wait is future-only.
    ///
    /// The waiter is registered synchronously here (under the orchestrator lock), then
    /// the returned future is awaited by the caller AFTER the lock is released, so a
    /// long-poll never holds the single Orchestrator mutex. Mirrors the TS
    /// `Orchestrator.waitForEvent` (which defaults an omitted timeout to
    /// [`DEFAULT_WAIT_TIMEOUT`]).
    pub fn wait_for_event(
        &self,
        event_name: Option<String>,
        timeout: Option<Duration>,
        since_cursor: Option<u64>,
    ) -> Result<WaitFuture, LifecycleError> {
        self.require_handle("wait for its events")?;
        Ok(self.event_buffer.wait_for(WaitForEventOptions {
            event_name,
            since_cursor,
            timeout: timeout.unwrap_or(DEFAULT_WAIT_TIMEOUT),
        }))
    }

    /// Spawn the background task that drains this Instance's async QMP events into the
    /// Event Buffer for the Instance's whole life. A lagged broadcast (a burst beyond
    /// the channel capacity) drops the missed events — a gap the buffer's monotonic
    /// cursor already makes visible — rather than blocking the reader; a closed channel
    /// ends the task.
    fn spawn_event_feeder(
        &mut self,
        mut events: broadcast::Receiver<crate::qemu::qmp_client::QmpEvent>,
    ) {
        // A previous feeder should already be stopped, but be defensive.
        self.stop_event_feeder();
        let buffer = Arc::clone(&self.event_buffer);
        self.event_feeder = Some(tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        buffer.append(event);
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }));
    }

    /// Abort and drop the current event feeder, if any, so it stops advancing the
    /// buffer once the Instance is gone.
    fn stop_event_feeder(&mut self) {
        if let Some(feeder) = self.event_feeder.take() {
            feeder.abort();
        }
    }

    /// Pause the running Instance's Guest CPUs via QMP `stop`, moving the lifecycle
    /// RUNNING → PAUSED (reflected by `get_status`, which then reports `paused`).
    /// Rejects when no Instance is running.
    pub async fn pause_instance(&mut self) -> Result<InstanceState, LifecycleError> {
        self.require_handle("pause it")?
            .execute("stop", None)
            .await
            .map_err(|e| LifecycleError(e.0))?;
        self.state = InstanceState::Paused;
        tracing::info!("Instance PAUSED (QMP stop)");
        Ok(self.state)
    }

    /// Resume the Instance's Guest CPUs via QMP `cont`, moving the lifecycle
    /// PAUSED → RUNNING. Rejects when no Instance is running.
    pub async fn resume_instance(&mut self) -> Result<InstanceState, LifecycleError> {
        self.require_handle("resume it")?
            .execute("cont", None)
            .await
            .map_err(|e| LifecycleError(e.0))?;
        self.state = InstanceState::Running;
        tracing::info!("Instance RUNNING (QMP cont)");
        Ok(self.state)
    }

    /// Hard-reset the Instance via QMP `system_reset` (equivalent to the reset button).
    /// This reboots the Guest in place; it does not change the lifecycle state. Rejects
    /// when no Instance is running.
    pub async fn reset_instance(&self) -> Result<InstanceState, LifecycleError> {
        self.require_handle("reset it")?
            .execute("system_reset", None)
            .await
            .map_err(|e| LifecycleError(e.0))?;
        tracing::info!("Instance reset (QMP system_reset)");
        Ok(self.state)
    }

    /// Request a graceful Guest shutdown via QMP `system_powerdown` (an ACPI
    /// power-button event). This only *asks* the Guest to power off; the Instance keeps
    /// running until the Guest acts, so the lifecycle state is unchanged. Rejects when
    /// no Instance is running.
    pub async fn powerdown_instance(&self) -> Result<InstanceState, LifecycleError> {
        self.require_handle("power it down")?
            .execute("system_powerdown", None)
            .await
            .map_err(|e| LifecycleError(e.0))?;
        tracing::info!("Instance ACPI powerdown requested (QMP system_powerdown)");
        Ok(self.state)
    }

    /// Return the live QMP `query-block` result (the Guest's block devices and their
    /// backing media). Rejects when no Instance is running.
    pub async fn query_block(&self) -> Result<serde_json::Value, LifecycleError> {
        self.require_handle("list its block devices")?
            .execute("query-block", None)
            .await
            .map_err(|e| LifecycleError(e.0))
    }

    /// Return the live QMP `query-cpus-fast` result (per-vCPU information). Rejects when
    /// no Instance is running.
    pub async fn query_cpus(&self) -> Result<serde_json::Value, LifecycleError> {
        self.require_handle("query its CPUs")?
            .execute("query-cpus-fast", None)
            .await
            .map_err(|e| LifecycleError(e.0))
    }

    /// Capture a screenshot of the Instance's display via QMP `screendump` and return
    /// the image inline.
    ///
    /// SECURITY (ADR-0003, the name-vs-argument gate): QMP `screendump` writes an
    /// arbitrary host file at the path it is given, so the `filename` is ALWAYS
    /// server-chosen — a fresh, unique file under a server-controlled directory — and
    /// never agent-supplied (this method takes no path input). The generic Command
    /// Policy name-gate is NOT sufficient for `screendump` (it would gate the name but
    /// not the dangerous path argument), which is exactly why `screendump` is absent
    /// from the default allowlist and served only here. The bytes are read back,
    /// returned as base64, and the temp file is deleted, so the agent never learns or
    /// controls a host path. Rejects when no Instance is running. Mirrors the TS
    /// `Orchestrator.screendump`.
    pub async fn screendump(&self) -> Result<ScreendumpResult, LifecycleError> {
        let handle = self.require_handle("capture a screendump")?;
        let dir = std::env::temp_dir().join("qmp-mcp").join("screendumps");
        tokio::fs::create_dir_all(&dir).await.map_err(|e| {
            LifecycleError(format!(
                "Failed to create the screendump directory {}: {e}",
                dir.display()
            ))
        })?;
        // Server-chosen, single-use path — NOT influenced by the agent.
        let filename = screendump_path(&dir);
        let filename_str = filename.to_string_lossy().into_owned();

        // Best-effort cleanup on every exit path, so the captured frame never lingers.
        let result = async {
            handle
                .execute(
                    "screendump",
                    Some(serde_json::json!({ "filename": filename_str, "format": "png" })),
                )
                .await
                .map_err(|e| LifecycleError(e.0))?;
            let bytes = tokio::fs::read(&filename).await.map_err(|e| {
                LifecycleError(format!("Failed to read the captured screendump: {e}"))
            })?;
            Ok(ScreendumpResult {
                mime_type: "image/png".to_string(),
                data: base64_encode(&bytes),
                bytes: bytes.len(),
            })
        }
        .await;
        let _ = tokio::fs::remove_file(&filename).await;
        result
    }

    /// Run a generic QMP command against the running Instance, gated by the Command
    /// Policy (ADR-0003). The command name is checked FIRST: a denied command returns a
    /// [`CommandPolicyError`] and never reaches the QMP Session — fail-closed, so a
    /// hard-denied command is refused even with no Instance running. Only an allowed
    /// command requires (and is forwarded to) the live Session, returning its QMP
    /// `return` value. The forwarded name is the normalised one, so trailing whitespace
    /// never reaches QEMU. Mirrors the TS `Orchestrator.executeCommand`.
    pub async fn execute_command(
        &self,
        command: &str,
        args: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, ExecuteCommandError> {
        let verdict = decide_command(&self.command_policy, command);
        let name = match &verdict {
            crate::policy::CommandVerdict::Allowed { command } => command.clone(),
            crate::policy::CommandVerdict::Denied { .. } => {
                return Err(ExecuteCommandError::Policy(
                    CommandPolicyError::from_verdict(&verdict)
                        .expect("a denied verdict yields a CommandPolicyError"),
                ));
            }
        };
        let handle = self.require_handle(&format!("execute the QMP command \"{name}\""))?;
        handle
            .execute(&name, args)
            .await
            .map_err(|e| ExecuteCommandError::Lifecycle(LifecycleError(e.0)))
    }

    /// Borrow the live [`InstanceHandle`] for an action that requires a running
    /// Instance, or return an actionable [`LifecycleError`] naming the action. The
    /// handle is only present in RUNNING/PAUSED, so this also fail-closes the
    /// STARTING/STOPPED/NONE cases.
    fn require_handle(&self, action: &str) -> Result<&dyn InstanceHandle, LifecycleError> {
        match &self.handle {
            Some(handle) => Ok(handle.as_ref()),
            None => Err(LifecycleError(format!(
                "No Instance is running, so there is nothing to {action}. Create one with \
                 create_instance first."
            ))),
        }
    }
}

/// A monotonically increasing counter making each server-chosen screendump filename
/// unique within this process, combined with the PID and a high-resolution timestamp.
static SCREENDUMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// A fresh, single-use, server-controlled screendump path under `dir`. It is derived
/// entirely from server-side state (PID + high-resolution clock + a process-unique
/// counter) and NEVER from agent input — the containment guarantee for the arbitrary
/// host-file write QMP `screendump` performs.
fn screendump_path(dir: &std::path::Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SCREENDUMP_SEQ.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(
        "screendump-{}-{nanos}-{seq}.png",
        std::process::id()
    ))
}

/// Standard (RFC 4648) base64-encode `bytes`, with `=` padding. Hand-rolled to avoid a
/// dependency, matching the repo's hand-rolled ethos; the output feeds MCP image
/// content verbatim.
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use tokio::sync::Mutex as AsyncMutex;

    use super::*;
    use crate::qemu::driver::FakeQemuDriver;
    use crate::qemu::qmp_client::QmpEvent;

    /// Deterministic options for the lifecycle tests: force TCG (no `/dev/kvm`
    /// probe), a fixed socket, no stores or caps. A diskless empty spec launches
    /// cleanly against these.
    fn test_options() -> OrchestratorOptions {
        OrchestratorOptions {
            binary: "qemu-system-x86_64".to_string(),
            qmp_socket_path: "/run/qmp-mcp/qmp.sock".to_string(),
            image_dir: None,
            iso_dir: None,
            hostfwd_port_range: None,
            allow_host_net: false,
            max_memory_mb: None,
            max_vcpus: None,
            allow_raw_args: false,
            command_policy: None,
            event_buffer_size: None,
            kvm_available: Box::new(|| false),
        }
    }

    fn orchestrator_with(driver: FakeQemuDriver) -> Orchestrator {
        Orchestrator::new(Box::new(driver), test_options())
    }

    /// Emit a synthetic QMP event onto the fake driver's stream and wait until the
    /// Orchestrator's feeder has drained at least `expected` events into the buffer.
    /// The feeder runs on a background task, so accumulation is observed with a bounded
    /// poll rather than assumed synchronous (never flaky: bounded to ~1s).
    async fn await_event_count(orch: &Orchestrator, expected: usize) {
        for _ in 0..1_000 {
            if orch.get_events(None).map(|r| r.events.len()).unwrap_or(0) >= expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("feeder never accumulated {expected} events");
    }

    fn synth(event: &str) -> QmpEvent {
        QmpEvent {
            event: event.to_string(),
            data: None,
            timestamp: None,
        }
    }

    fn synth_data(event: &str, data: serde_json::Value) -> QmpEvent {
        QmpEvent {
            event: event.to_string(),
            data: Some(data),
            timestamp: None,
        }
    }

    /// A RUNNING Orchestrator plus a sender onto its Instance's synthetic event stream
    /// (captured before the driver moves into the Orchestrator).
    async fn running_with_events(
        buffer_size: Option<u32>,
    ) -> (Orchestrator, broadcast::Sender<QmpEvent>) {
        let driver = FakeQemuDriver::new();
        let slot = driver.events_slot();
        let mut options = test_options();
        options.event_buffer_size = buffer_size;
        let mut orch = Orchestrator::new(Box::new(driver), options);
        orch.create_instance(json!({})).await.unwrap();
        let sender = slot
            .lock()
            .unwrap()
            .clone()
            .expect("create_instance installs an event sender");
        (orch, sender)
    }

    #[tokio::test]
    async fn create_brings_instance_to_running_then_destroy_returns_to_none() {
        let mut orch = orchestrator_with(FakeQemuDriver::new());
        assert_eq!(orch.state(), InstanceState::None);
        assert_eq!(orch.get_instance().state, InstanceState::None);

        let result = orch.create_instance(json!({})).await.expect("create ok");
        assert_eq!(result.state, InstanceState::Running);
        assert_eq!(result.accel, Accel::Tcg); // kvm_available == false → TCG

        let view = orch.get_instance();
        assert_eq!(view.state, InstanceState::Running);
        assert!(view.spec.is_some());
        assert_eq!(view.accel, Some(Accel::Tcg));

        orch.destroy_instance().await.expect("destroy ok");
        assert_eq!(orch.state(), InstanceState::None);
        assert!(orch.get_instance().spec.is_none());
        assert!(orch.get_instance().accel.is_none());
    }

    #[tokio::test]
    async fn build_argv_output_reaches_the_driver() {
        let driver = FakeQemuDriver::new();
        let launches = driver.launches();
        let mut orch = orchestrator_with(driver);
        orch.create_instance(json!({})).await.unwrap();

        let launches = launches.lock().unwrap();
        assert_eq!(launches.len(), 1);
        let argv = &launches[0].argv;
        // The generated argv is headless/frozen and wires the managed QMP socket.
        assert!(argv.iter().any(|a| a == "-qmp"));
        assert!(argv.iter().any(|a| a == "-S"));
        assert_eq!(launches[0].binary, "qemu-system-x86_64");
    }

    #[tokio::test]
    async fn create_while_running_is_rejected_with_actionable_message() {
        let mut orch = orchestrator_with(FakeQemuDriver::new());
        orch.create_instance(json!({})).await.unwrap();

        let err = orch.create_instance(json!({})).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("An Instance already exists"), "got: {msg}");
        assert!(msg.contains("state RUNNING"), "got: {msg}");
        assert!(msg.contains("destroy_instance"), "got: {msg}");
        // Still exactly one Instance, still RUNNING.
        assert_eq!(orch.state(), InstanceState::Running);
    }

    #[tokio::test]
    async fn destroy_without_instance_is_rejected_actionably() {
        let mut orch = orchestrator_with(FakeQemuDriver::new());
        let err = orch.destroy_instance().await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("No Instance is running"), "got: {msg}");
        assert!(msg.contains("create_instance"), "got: {msg}");
    }

    #[tokio::test]
    async fn get_status_reflects_tracked_run_state_across_pause_resume() {
        let mut orch = orchestrator_with(FakeQemuDriver::new());
        // No Instance → get_status is refused (get_instance is the NONE-safe view).
        assert!(orch.get_status().await.is_err());

        orch.create_instance(json!({})).await.unwrap();
        let status = orch.get_status().await.unwrap();
        assert_eq!(status["status"], "running");
        assert_eq!(status["running"], true);

        // RUNNING → PAUSED (QMP stop), reflected by the live status.
        assert_eq!(orch.pause_instance().await.unwrap(), InstanceState::Paused);
        assert_eq!(orch.state(), InstanceState::Paused);
        let status = orch.get_status().await.unwrap();
        assert_eq!(status["status"], "paused");
        assert_eq!(status["running"], false);

        // PAUSED → RUNNING (QMP cont).
        assert_eq!(
            orch.resume_instance().await.unwrap(),
            InstanceState::Running
        );
        assert_eq!(orch.state(), InstanceState::Running);
        assert_eq!(orch.get_status().await.unwrap()["status"], "running");
    }

    #[tokio::test]
    async fn invalid_spec_is_rejected_before_any_launch() {
        let driver = FakeQemuDriver::new();
        let launches = driver.launches();
        let mut orch = orchestrator_with(driver);

        let err = orch
            .create_instance(json!({ "vcpus": 0 }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("vcpus"), "got: {err}");
        assert_eq!(orch.state(), InstanceState::None); // slot released
        assert_eq!(
            launches.lock().unwrap().len(),
            0,
            "no launch on invalid spec"
        );
    }

    #[tokio::test]
    async fn launch_failure_releases_the_slot_and_reports_actionably() {
        let mut orch =
            orchestrator_with(FakeQemuDriver::with_launch_error("qemu binary not found"));
        let err = orch.create_instance(json!({})).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Failed to create the Instance"), "got: {msg}");
        assert!(msg.contains("qemu binary not found"), "got: {msg}");
        // The reserved slot is freed, so the Orchestrator is back to NONE.
        assert_eq!(orch.state(), InstanceState::None);
    }

    #[tokio::test]
    async fn reset_and_powerdown_leave_the_lifecycle_state_unchanged() {
        let mut orch = orchestrator_with(FakeQemuDriver::new());
        // No Instance → both are refused with an actionable message.
        assert!(orch
            .reset_instance()
            .await
            .unwrap_err()
            .0
            .contains("reset it"));
        assert!(orch
            .powerdown_instance()
            .await
            .unwrap_err()
            .0
            .contains("power it down"));

        orch.create_instance(json!({})).await.unwrap();
        assert_eq!(orch.reset_instance().await.unwrap(), InstanceState::Running);
        assert_eq!(orch.state(), InstanceState::Running);
        assert_eq!(
            orch.powerdown_instance().await.unwrap(),
            InstanceState::Running
        );
        assert_eq!(orch.state(), InstanceState::Running);
    }

    #[tokio::test]
    async fn query_block_and_query_cpus_forward_their_qmp_commands() {
        let mut orch = orchestrator_with(FakeQemuDriver::new());
        assert!(orch.query_block().await.is_err());
        assert!(orch.query_cpus().await.is_err());

        orch.create_instance(json!({})).await.unwrap();
        let block = orch.query_block().await.unwrap();
        assert_eq!(block[0]["device"], "virtio0");
        let cpus = orch.query_cpus().await.unwrap();
        assert_eq!(cpus[0]["cpu-index"], 0);
    }

    #[tokio::test]
    async fn screendump_writes_a_server_chosen_path_reads_it_back_and_deletes_it() {
        let mut orch = orchestrator_with(FakeQemuDriver::new());
        // No Instance → refused (never touches the filesystem).
        assert!(orch
            .screendump()
            .await
            .unwrap_err()
            .0
            .contains("capture a screendump"));

        orch.create_instance(json!({})).await.unwrap();
        let shot = orch.screendump().await.unwrap();
        assert_eq!(shot.mime_type, "image/png");
        assert!(shot.bytes > 0);
        // The fake wrote a PNG signature; base64 of it starts with the same prefix as
        // the real bytes would, proving the captured file was read back inline.
        assert_eq!(shot.data, base64_encode(b"\x89PNG\r\n\x1a\nFAKE"));

        // The temp file is deleted after reading — the host is left clean.
        let dir = std::env::temp_dir().join("qmp-mcp").join("screendumps");
        if let Ok(mut entries) = std::fs::read_dir(&dir) {
            assert!(
                entries.all(|e| {
                    e.map(|e| !e.file_name().to_string_lossy().ends_with(".png"))
                        .unwrap_or(true)
                }),
                "screendump left a .png behind in {}",
                dir.display()
            );
        }
    }

    /// The screendump path is derived only from server-side state (PID + clock +
    /// counter), never from agent input, and is unique per call — the containment
    /// guarantee for the arbitrary host-file write `screendump` performs.
    #[test]
    fn screendump_path_is_server_chosen_contained_and_unique() {
        let dir = std::env::temp_dir().join("qmp-mcp").join("screendumps");
        let a = screendump_path(&dir);
        let b = screendump_path(&dir);
        assert_ne!(a, b, "each screendump path must be unique");
        for p in [&a, &b] {
            assert!(p.starts_with(&dir), "path must be contained under {dir:?}");
            let name = p.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with("screendump-") && name.ends_with(".png"));
        }
    }

    #[tokio::test]
    async fn execute_command_allows_an_allowlisted_command_and_forwards_the_normalised_name() {
        let mut orch = orchestrator_with(FakeQemuDriver::new());
        orch.create_instance(json!({})).await.unwrap();
        // Stray case/space normalises to `query-status` and reaches the fake handle.
        let result = orch
            .execute_command("  Query-Status  ", None)
            .await
            .unwrap();
        assert_eq!(result["status"], "running");
    }

    #[tokio::test]
    async fn execute_command_denies_a_default_denied_command_with_an_actionable_reason() {
        let mut orch = orchestrator_with(FakeQemuDriver::new());
        orch.create_instance(json!({})).await.unwrap();
        match orch.execute_command("totally-made-up-command", None).await {
            Err(ExecuteCommandError::Policy(err)) => {
                assert!(!err.hard_denied);
                assert!(err.message.contains("not in the Command Policy allowlist"));
            }
            other => panic!("expected a policy denial, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_command_hard_denies_even_with_no_instance_running_fail_closed() {
        // No Instance: a hard-denied command is still refused by the policy FIRST,
        // before the running-Instance check, so it never reaches (a non-existent) QEMU.
        let orch = orchestrator_with(FakeQemuDriver::new());
        match orch.execute_command("human-monitor-command", None).await {
            Err(ExecuteCommandError::Policy(err)) => {
                assert!(err.hard_denied);
                assert!(err.message.contains("hard denylist"));
            }
            other => panic!("expected a hard policy denial, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_command_honours_an_injected_policy_override() {
        // A policy that additionally allows `query-version` lets it through, while a
        // default Orchestrator would default-deny it.
        let mut options = test_options();
        options.command_policy = Some(build_policy(&PolicyOverrides {
            allow: vec!["query-version".to_string()],
            deny: vec![],
        }));
        let mut orch = Orchestrator::new(Box::new(FakeQemuDriver::new()), options);
        orch.create_instance(json!({})).await.unwrap();
        let version = orch.execute_command("query-version", None).await.unwrap();
        assert_eq!(version["qemu"]["major"], 9);
    }

    #[tokio::test]
    async fn captures_events_and_get_events_pages_by_cursor() {
        let (orch, events) = running_with_events(None).await;
        events
            .send(synth_data("STOP", json!({ "reason": "pause" })))
            .unwrap();
        events.send(synth("RESET")).unwrap();
        await_event_count(&orch, 2).await;

        let ReadResult {
            events: buffered,
            cursor,
        } = orch.get_events(None).unwrap();
        assert_eq!(
            buffered
                .iter()
                .map(|e| e.event.as_str())
                .collect::<Vec<_>>(),
            ["STOP", "RESET"]
        );
        assert_eq!(buffered[0].data, Some(json!({ "reason": "pause" })));
        assert_eq!(cursor, buffered.last().unwrap().seq);

        // Cursor paging: only newer events come back.
        events.send(synth("SHUTDOWN")).unwrap();
        await_event_count(&orch, 3).await;
        let next = orch.get_events(Some(cursor)).unwrap();
        assert_eq!(
            next.events
                .iter()
                .map(|e| e.event.as_str())
                .collect::<Vec<_>>(),
            ["SHUTDOWN"]
        );
    }

    #[tokio::test]
    async fn bounds_the_buffer_evicting_the_oldest_past_capacity() {
        let (orch, events) = running_with_events(Some(3)).await;
        for name in ["e1", "e2", "e3", "e4", "e5"] {
            events.send(synth(name)).unwrap();
        }
        // Wait for all five to be processed (the buffer retains only the last three).
        for _ in 0..1_000 {
            if orch.get_events(None).unwrap().cursor >= 5 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let buffered = orch.get_events(None).unwrap().events;
        assert_eq!(
            buffered
                .iter()
                .map(|e| e.event.as_str())
                .collect::<Vec<_>>(),
            ["e3", "e4", "e5"]
        );
    }

    #[tokio::test]
    async fn wait_for_event_resolves_on_a_matching_filtered_event() {
        let (orch, events) = running_with_events(None).await;
        let pending = orch
            .wait_for_event(
                Some("SHUTDOWN".to_string()),
                Some(Duration::from_secs(1)),
                None,
            )
            .unwrap();
        events.send(synth("STOP")).unwrap(); // non-matching
        events
            .send(synth_data("SHUTDOWN", json!({ "guest": true })))
            .unwrap();

        let result = pending.await;
        assert!(!result.timed_out);
        let event = result.event.expect("a matching event");
        assert_eq!(event.event, "SHUTDOWN");
        assert_eq!(event.data, Some(json!({ "guest": true })));
    }

    #[tokio::test]
    async fn wait_for_event_with_no_filter_resolves_on_any_event() {
        let (orch, events) = running_with_events(None).await;
        let pending = orch
            .wait_for_event(None, Some(Duration::from_secs(1)), None)
            .unwrap();
        events.send(synth("POWERDOWN")).unwrap();
        let result = pending.await;
        assert!(!result.timed_out);
        assert_eq!(result.event.unwrap().event, "POWERDOWN");
    }

    #[tokio::test]
    async fn wait_for_event_times_out_cleanly_when_no_match_arrives() {
        let (orch, events) = running_with_events(None).await;
        let pending = orch
            .wait_for_event(
                Some("SHUTDOWN".to_string()),
                Some(Duration::from_millis(20)),
                None,
            )
            .unwrap();
        events.send(synth("STOP")).unwrap(); // never matches
        let result = pending.await;
        assert!(result.timed_out);
        assert!(result.event.is_none());
    }

    #[tokio::test]
    async fn wait_for_event_is_race_safe_with_since_cursor() {
        let (orch, events) = running_with_events(None).await;
        // The event lands before the wait is issued; a future-only wait would miss it.
        events.send(synth("SHUTDOWN")).unwrap();
        await_event_count(&orch, 1).await;
        let result = orch
            .wait_for_event(Some("SHUTDOWN".to_string()), Some(Duration::ZERO), Some(0))
            .unwrap()
            .await;
        assert!(!result.timed_out);
        assert_eq!(result.event.unwrap().event, "SHUTDOWN");
    }

    #[tokio::test]
    async fn rejects_events_tools_actionably_when_no_instance_is_running() {
        let orch = orchestrator_with(FakeQemuDriver::new());
        let err = orch.get_events(None).unwrap_err();
        assert!(err.0.contains("read its events"), "got: {}", err.0);
        assert!(err.0.contains("create_instance"), "got: {}", err.0);
        // The Ok arm is a `WaitFuture` (not `Debug`), so match rather than `unwrap_err`.
        let Err(err) = orch.wait_for_event(None, Some(Duration::ZERO), None) else {
            panic!("wait_for_event must reject when no Instance is running");
        };
        assert!(err.0.contains("wait for its events"), "got: {}", err.0);
        assert!(err.0.contains("create_instance"), "got: {}", err.0);
    }

    #[tokio::test]
    async fn does_not_leak_events_across_instances() {
        let (mut orch, events) = running_with_events(None).await;
        events.send(synth("STOP")).unwrap();
        await_event_count(&orch, 1).await;
        assert_eq!(orch.get_events(None).unwrap().events.len(), 1);

        // Destroy + recreate: the buffer starts empty for the new Instance, and the
        // old sender's events no longer reach it (the feeder was aborted).
        orch.destroy_instance().await.unwrap();
        orch.create_instance(json!({})).await.unwrap();
        assert!(orch.get_events(None).unwrap().events.is_empty());
        // The old sender feeds the previous Instance's (now-aborted) channel; the
        // send may find no receiver at all, which is exactly the point — it must not
        // reach the new Instance's buffer. Tolerate a no-receiver send.
        let _ = events.send(synth("RESET"));
        // Give any stray delivery a chance, then confirm nothing leaked in.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(orch.get_events(None).unwrap().events.is_empty());
    }

    #[tokio::test]
    async fn settles_a_pending_wait_when_the_instance_is_destroyed() {
        let (mut orch, _events) = running_with_events(None).await;
        let pending = orch
            .wait_for_event(
                Some("SHUTDOWN".to_string()),
                Some(Duration::from_secs(5)),
                None,
            )
            .unwrap();
        orch.destroy_instance().await.unwrap();
        // The wait resolves as a clean timeout rather than hanging on the dead Instance.
        let result = pending.await;
        assert!(result.timed_out);
    }

    #[test]
    fn base64_encode_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    /// ADR-0011: concurrent `create_instance` calls serialise on the mutex and
    /// exactly one wins — the create-time TOCTOU is structurally impossible. Uses a
    /// multi-threaded runtime so the tasks genuinely contend for the lock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_create_calls_serialize_and_exactly_one_wins() {
        let driver = FakeQemuDriver::new();
        let launches = driver.launches();
        let orch = Arc::new(AsyncMutex::new(orchestrator_with(driver)));

        let attempts = 16;
        let mut tasks = Vec::with_capacity(attempts);
        for _ in 0..attempts {
            let orch = Arc::clone(&orch);
            tasks.push(tokio::spawn(async move {
                // The caller holds the async mutex for the whole create — including
                // the launch await — so the calls fully serialise.
                let mut guard = orch.lock().await;
                guard.create_instance(json!({})).await
            }));
        }

        let mut wins = 0usize;
        let mut rejects = 0usize;
        for task in tasks {
            match task.await.expect("task joined") {
                Ok(result) => {
                    assert_eq!(result.state, InstanceState::Running);
                    wins += 1;
                }
                Err(err) => {
                    assert!(
                        err.to_string().contains("An Instance already exists"),
                        "unexpected error: {err}"
                    );
                    rejects += 1;
                }
            }
        }

        assert_eq!(wins, 1, "exactly one create must win");
        assert_eq!(rejects, attempts - 1);
        // The structural guarantee: the driver launched exactly once — no second
        // qemu was ever spawned and orphaned.
        assert_eq!(
            launches.lock().unwrap().len(),
            1,
            "the driver must have launched exactly once"
        );
        assert_eq!(orch.lock().await.state(), InstanceState::Running);
    }
}
