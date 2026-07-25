//! The single-instance lifecycle Orchestrator (ADR-0001/0004). It holds the one
//! managed Instance and drives it through its lifecycle:
//!
//!   NONE → STARTING → RUNNING ⇄ PAUSED → STOPPED → NONE
//!
//! A second implementation of the shared bounded context, mirroring
//! `../../typescript/src/instance/orchestrator.ts`: same state names, same reject-while-running
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

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::process::{Child, ChildStderr, ChildStdin, Command};
use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::config::{PortRange, SerialBackend};

use super::event_buffer::{
    EventBuffer, ReadResult, WaitForEventOptions, WaitFuture, DEFAULT_EVENT_BUFFER_SIZE,
};
use super::hardware_spec::{
    build_argv, parse_hardware_spec, qemu_arch_of_binary, qemu_binary_for_machine, resolve_accel,
    resolve_serial_spool_path, serial_socket_path, Accel, AccelResolution, ArgvOptions,
    DisplayDevice, DisplayMode, HardwareSpec, SERIAL_CHARDEV_ID, SHARE_MOUNT_TAG,
    VNC_LOOPBACK_HOST, VNC_LOOPBACK_PORT,
};
use crate::policy::{
    build_policy, decide_command, CommandPolicyError, PolicyOverrides, ResolvedPolicy,
};
use crate::qemu::driver::{DriverError, ExitedFuture, InstanceHandle, LaunchRequest, QemuDriver};
use crate::viewer::{RealViewerFactory, ViewerFactory, ViewerHandle, ViewerOptions};

/// The lifecycle states an Instance moves through. `PAUSED` is entered by
/// [`Orchestrator::pause_instance`] (QMP `stop`) — and by
/// [`Orchestrator::create_instance`] when the Guest loads frozen at the `-S` startup
/// pause (the default, unless `QMP_MCP_AUTO_START` resumes it) — and left by
/// [`Orchestrator::resume_instance`] (QMP `cont`). In `PAUSED` the Guest CPUs are not
/// executing (`get_status`/`query-status` reads `paused`/`prelaunch`). The names
/// match the TS `InstanceState` union exactly.
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

/// Read-only report of the host↔guest folder-sharing configuration (ADR-0014),
/// returned by the `get_share` tool. Never includes the host path. Mirrors the shape of
/// the TS `describeShare` result.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ShareReport {
    /// Whether a host folder is shared into guests (`QMP_MCP_HOST_SHARE_DIR` is set).
    pub available: bool,
    /// The fixed 9p mount tag the guest mounts.
    pub mount_tag: String,
    /// The intended guest mountpoint (`QMP_MCP_GUEST_SHARE_DIR`), if configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guest_mountpoint: Option<String>,
    /// Whether the share is read-only.
    pub read_only: bool,
    /// The exact `mount` command to run inside the guest (`None` when unavailable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mount_command: Option<String>,
    /// A human hint on how to use (or enable) the share.
    pub hint: String,
}

/// The read-only Serial Port report returned by the `get_serial` tool (ADR-0015). Advisory:
/// it names the capture backend, the ring-buffer size, how `read_serial` behaves, and a
/// best-effort guess of the guest console device — which the guest's own `console=` cmdline
/// ultimately decides. Mirrors the TS `SerialReport`.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SerialReport {
    /// The capture backend carrying the Serial Port's output (`ringbuf`).
    pub backend: String,
    /// Ring-buffer size in bytes (`QMP_MCP_SERIAL_BUFFER_BYTES`).
    pub buffer_bytes: u32,
    /// How `read_serial` behaves: `drain` (ringbuf returns and clears new output).
    pub read_semantics: String,
    /// Whether writing to the console is enabled (always false in the read-only slice).
    pub writable: bool,
    /// Whether the current Instance has a Serial Port attached (`serial: true`).
    pub enabled: bool,
    /// The spool subdirectory the log is written under, when the spool backend + a `serialSpool`
    /// are active (advisory — the host path is never revealed). `None` otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spool_subdir: Option<String>,
    /// Best-effort expected guest console device for the current machine, when known
    /// (`ttyS0` on q35/pc, `ttyAMA0` on virt). Advisory only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub console_device: Option<String>,
    /// A hint on reading the Serial Port and setting the guest console.
    pub hint: String,
}

/// The result of a `read_serial` drain (ADR-0015). Mirrors the TS `SerialReadResult`.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SerialReadResult {
    /// The serial output read and cleared (drained). Empty when nothing was buffered.
    pub output: String,
    /// Encoding of `output`: `utf8` (text) or `base64` (non-UTF8 early-boot bytes).
    pub format: String,
    /// Number of characters returned in `output`.
    pub bytes: usize,
}

/// The result of a `write_serial` (ADR-0015). Mirrors the TS `SerialWriteResult`.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SerialWriteResult {
    /// Number of characters written to the console.
    pub bytes_written: usize,
}

/// The encoding `read_serial` requests from QMP `ringbuf-read`. Defaults to `utf8`. Mirrors
/// the TS `read_serial` `format` union.
#[derive(
    Debug, Clone, Copy, Default, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum SerialFormat {
    /// UTF-8 text (default).
    #[default]
    Utf8,
    /// Base64 — for non-UTF8 bytes.
    Base64,
}

impl SerialFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Utf8 => "utf8",
            Self::Base64 => "base64",
        }
    }
}

/// Best-effort guess of the guest's serial console device for a machine (ADR-0015). Advisory
/// only — the guest's own `console=` cmdline is authoritative. `None` for boards we cannot
/// guess (incl. the `raspi*` two-UART case, which the hint covers). Mirrors the TS helper.
fn guess_console_device(machine: &str) -> Option<&'static str> {
    if machine.starts_with("q35")
        || machine.starts_with("pc")
        || machine.starts_with("microvm")
        || machine.starts_with("isapc")
    {
        Some("ttyS0")
    } else if machine.starts_with("virt") {
        Some("ttyAMA0")
    } else {
        None
    }
}

/// The result of a successful [`Orchestrator::create_instance`].
#[derive(Debug, Clone)]
pub struct CreateInstanceResult {
    /// `RUNNING` when the Guest was auto-started (`QMP_MCP_AUTO_START`, on by default —
    /// ADR-0016), else `PAUSED` (loaded, frozen at `-S` until `resume_instance`).
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

/// The read-only display-recording capability report returned by the `get_recording` tool
/// (ADR-0017). Advisory and infallible (like `describe_share`/`describe_serial`): it says whether
/// recording is available and why, the resolved codec, whether a recording is active now, the
/// output root, and the CRF/max-fps/pixfmt the encoder is run with. Mirrors the TS `RecordingReport`.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecordingReport {
    /// Whether display recording is available (ffmpeg resolved, output dir set, codec present).
    pub available: bool,
    /// Human-readable reason: why recording is unavailable, or that it is available. Actionable.
    pub reason: String,
    /// The resolved recording codec (`QMP_MCP_RECORDING_CODEC`).
    pub codec: String,
    /// The CRF / constant-quality target the encoder uses.
    pub crf: u32,
    /// The max frame rate the `screendump` loop is paced to (the capture-loop pacing cap).
    pub max_fps: u32,
    /// The pixel format the encoder writes.
    pub pixfmt: String,
    /// Whether a recording is running on the current Instance right now.
    pub active: bool,
    /// When active, the host path being written to; otherwise `null`.
    pub active_path: Option<String>,
    /// The operator output root recordings are written under (`QMP_MCP_RECORDING_DIR`), or `null`.
    pub output_dir: Option<String>,
}

/// The result of a successful [`Orchestrator::start_recording`] (ADR-0017). The video is large, so
/// — unlike `screendump`'s inline PNG — only the host path is returned; the operator retrieves the
/// file. Converged with the TS `RecordingStartResult` to exactly `{ path, name, codec }` (ADR-0012).
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StartRecordingResult {
    /// The host path the recording is being written to (under `QMP_MCP_RECORDING_DIR`).
    pub path: String,
    /// The output file's base name (the on-disk file name, e.g. `run1.mkv`).
    pub name: String,
    /// The codec the recording is encoded with.
    pub codec: String,
}

/// The result of a successful [`Orchestrator::stop_recording`] (ADR-0017): the finalized file's
/// path/name/codec, its size in bytes, and the wall-clock duration in MILLISECONDS. Converged with
/// the TS `RecordingStopResult` to exactly `{ path, name, codec, sizeBytes, durationMs }` (ADR-0012).
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StopRecordingResult {
    /// The host path of the finalized recording.
    pub path: String,
    /// The output file's base name.
    pub name: String,
    /// The codec the recording was encoded with.
    pub codec: String,
    /// Size of the finalized file in bytes (0 if it could not be stat'd).
    pub size_bytes: u64,
    /// Wall-clock recording duration in milliseconds.
    pub duration_ms: u64,
}

/// Knobs the Orchestrator needs that are not part of the Hardware Spec. The
/// singleton injects the env-resolved values (mirrors the TS `OrchestratorOptions`,
/// trimmed to what this slice's `build_argv` + accel resolution consume).
pub struct OrchestratorOptions {
    /// Explicit `qemu-system-*` binary override (`QMP_MCP_QEMU_BINARY`). When `None`,
    /// the binary is DERIVED per-instance from the spec's `machine` (issue #18):
    /// `qemu-system-x86_64` for q35/pc, `qemu-system-aarch64` for virt/raspi*. Set it
    /// only to force a specific emulator (e.g. a custom build) for every Instance.
    pub qemu_binary_override: Option<String>,
    /// This host's QEMU architecture (`x86_64`/`aarch64`) for the `accel: auto`
    /// guest/host match (issue #18). The singleton wires the real host via
    /// `host_qemu_arch()`; tests set it deterministically.
    pub host_arch: String,
    /// Server-managed path of the QMP UNIX socket.
    pub qmp_socket_path: String,
    /// Absolute path of the Image Store directory (ADR-0006); required by a spec
    /// with disks.
    pub image_dir: Option<String>,
    /// Absolute path of the read-only ISO Store directory (ADR-0006); required by a
    /// spec with a cdrom.
    pub iso_dir: Option<String>,
    /// Host directory shared into the guest via virtio-9p when a spec sets `share: true`
    /// (`QMP_MCP_HOST_SHARE_DIR`, ADR-0014). Operator-configured, never agent-supplied;
    /// `None` means sharing is disabled (a `share: true` spec is then refused).
    pub host_share_dir: Option<String>,
    /// Intended guest mountpoint for the shared folder (`QMP_MCP_GUEST_SHARE_DIR`).
    /// Advisory only — reported by `get_share`.
    pub guest_share_dir: Option<String>,
    /// Whether the shared folder is read-only (default true; `Some(false)` only when
    /// the operator set `QMP_MCP_ALLOW_SHARE_WRITE`). Threaded into the argv fail-closed.
    pub share_readonly: Option<bool>,
    /// Serial Port ring-buffer size in bytes when a spec sets `serial: true`
    /// (`QMP_MCP_SERIAL_BUFFER_BYTES`, ADR-0015; power-of-two). Passed to the argv generator.
    pub serial_buffer_bytes: u32,
    /// Whether writing to the Serial Port is enabled (`QMP_MCP_ALLOW_SERIAL_WRITE`, ADR-0015;
    /// default false). Gates the `write_serial` tool; the agent can never enable it.
    pub allow_serial_write: bool,
    /// The Serial Port backend (`QMP_MCP_SERIAL_BACKEND`, ADR-0015). Passed to the argv generator
    /// and drives how `read_serial` behaves (drain vs tail).
    pub serial_backend: SerialBackend,
    /// Operator root for spool-backend Serial Port logs (`QMP_MCP_SERIAL_SPOOL_DIR`), or `None`.
    pub serial_spool_dir: Option<String>,
    /// Resolved `ffmpeg` binary for display recording (`QMP_MCP_FFMPEG_BINARY` or PATH, ADR-0017),
    /// or `None` when absent — recording is then unavailable and `start_recording` fails closed.
    pub ffmpeg_binary: Option<String>,
    /// Operator output root display recordings are written under (`QMP_MCP_RECORDING_DIR`, ADR-0017),
    /// or `None`. The agent names only a file under it (the spool trust model).
    pub recording_dir: Option<String>,
    /// Display-recording codec (`QMP_MCP_RECORDING_CODEC`, ADR-0017; default `libx264`).
    pub recording_codec: String,
    /// Display-recording CRF / constant-quality target (`QMP_MCP_RECORDING_CRF`, ADR-0017).
    pub recording_crf: u32,
    /// Max frame rate the `screendump` loop is paced to (`QMP_MCP_RECORDING_MAX_FPS`, ADR-0017).
    pub recording_max_fps: u32,
    /// Display-recording pixel format (`QMP_MCP_RECORDING_PIXFMT`, ADR-0017; default `yuv420p`).
    pub recording_pixfmt: String,
    /// Whether the selected codec is compiled into the resolved ffmpeg build (ADR-0017). Injected
    /// for testability, exactly like `kvm_available`: production wires an INSTANT lookup into the
    /// encoder set probed ONCE at startup ([`probe_ffmpeg_encoders`], R1) — no per-request spawn,
    /// no blocking call under the lock; tests force a deterministic value so no ffmpeg is spawned.
    /// Only consulted when ffmpeg + output dir are set.
    pub recording_encoder_available: Box<dyn Fn(&str) -> bool + Send + Sync>,
    /// Inclusive host-port range a user-mode forward's `hostPort` must fall within
    /// (ADR-0009); `None` uses the argv builder's default.
    pub hostfwd_port_range: Option<PortRange>,
    /// Whether host-level (`tap`/`bridge`) networking is permitted (ADR-0009).
    pub allow_host_net: bool,
    /// Whether `create_instance` auto-starts the Guest by issuing QMP `cont` right
    /// after launch (`QMP_MCP_AUTO_START`, issue #8). Default false: the Guest stays
    /// frozen at the `-S` startup pause until `resume_instance`.
    pub auto_start: bool,
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
    /// The human-facing noVNC Viewer password (`QMP_MCP_VIEWER_PASSWORD`, ADR-0010).
    /// A `display: vnc` spec is REJECTED before qemu is spawned when this is `None` —
    /// the fail-closed coupling between the Display and its browser Viewer.
    pub viewer_password: Option<String>,
    /// Optional username the Viewer enforces on its HTTP Basic auth alongside the
    /// password (`QMP_MCP_VIEWER_USER`, ADR-0010). `None` means the username is ignored
    /// — password-only, the historical default.
    pub viewer_user: Option<String>,
    /// Address the Viewer's own HTTP server binds to (`QMP_MCP_VIEWER_HOST`, default
    /// `127.0.0.1`). Independent of the MCP transport, so the Viewer works under
    /// `TRANSPORT=stdio` (ADR-0010).
    pub viewer_host: String,
    /// TCP port the Viewer's HTTP server listens on (`QMP_MCP_VIEWER_PORT`, default 6080).
    pub viewer_port: u16,
    /// Factory that starts the noVNC Viewer for a `display: vnc` Instance. `None` wires
    /// in the real in-process Viewer ([`RealViewerFactory`]); tests inject a fake so the
    /// lifecycle is exercisable without binding a real port.
    pub start_viewer: Option<Box<dyn ViewerFactory>>,
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
    /// The live Instance handle. An `Arc` (not a `Box`) so a background task — the display
    /// recording's `screendump` loop (ADR-0017) — can hold a clone and issue QMP commands
    /// concurrently while the Orchestrator still owns and later closes it. `execute`/`close`
    /// both take `&self`, so shared ownership is sound.
    handle: Option<Arc<dyn InstanceHandle>>,
    /// The socket-backend Serial Port bridge (ADR-0015): a live UNIX-socket connection to
    /// QEMU's `serial` chardev plus the reader task draining it into a bounded ring. Present
    /// only while a `serial: true` Instance runs under `QMP_MCP_SERIAL_BACKEND=socket`; torn
    /// down with the Instance. `None` for the ringbuf/spool backends.
    serial_bridge: Option<SerialBridge>,
    /// The active display recording (ADR-0017): the ffmpeg child, its stdin, the frame-loop task,
    /// and the output path. Present only while a recording runs; torn down with the Instance and
    /// on an unexpected qemu exit, exactly like [`SerialBridge`]. `None` when nothing is recording.
    recording: Option<Recording>,
    spec: Option<HardwareSpec>,
    accel: Option<Accel>,
    /// The running noVNC Viewer for a `display: vnc` Instance, if any (ADR-0010). Its
    /// lifetime equals the Instance's: started during create, stopped on destroy.
    viewer: Option<Box<dyn ViewerHandle>>,
    /// Factory that starts the Viewer; the real in-process Viewer by default, a fake
    /// under test.
    start_viewer: Box<dyn ViewerFactory>,
    /// The Event Buffer capturing the current Instance's QMP async events. One buffer
    /// lives for the server's lifetime; it is [`EventBuffer::reset`] on every
    /// create/destroy so events never bleed across Instances (issue #12). Shared with
    /// the feeder task, hence `Arc`.
    event_buffer: Arc<EventBuffer>,
    /// The background task draining the current Instance's async QMP events into the
    /// Event Buffer. Aborted (and cleared) on destroy, so the buffer stops advancing
    /// when the Instance is gone.
    event_feeder: Option<JoinHandle<()>>,
    /// A `Weak` back-reference to the shared `Arc<Mutex<Self>>`, installed once by
    /// [`new_shared`](Self::new_shared) right after construction (the `Arc` cannot exist
    /// yet inside [`new`](Self::new)). [`create_instance`](Self::create_instance)
    /// upgrades it to spawn the exit-watch task. Storing a `Weak` — not an `Arc` — keeps
    /// the Orchestrator from owning itself, so there is no reference cycle. Empty for a
    /// bare (non-shared) Orchestrator, whose exit watcher is then inert (issue #28).
    self_ref: Weak<Mutex<Orchestrator>>,
    /// Monotonic Instance generation, bumped each time an Instance is published RUNNING.
    /// The exit-watch task captures the generation of the Instance it watches and only
    /// reconciles while it still matches — so a stale watcher for a since-destroyed (and
    /// perhaps recreated) Instance is a no-op and can never tear down a NEWER one.
    generation: u64,
    /// The background task awaiting the current Instance's exit signal to reconcile it to
    /// NONE on an unexpected qemu exit. Aborted on an explicit destroy; the generation
    /// guard also makes any stale wakeup inert.
    exit_watcher: Option<JoinHandle<()>>,
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
        // Default to the real in-process Viewer; tests inject a fake factory.
        let start_viewer = options
            .start_viewer
            .take()
            .unwrap_or_else(|| Box::new(RealViewerFactory));
        Self {
            driver,
            options,
            command_policy,
            state: InstanceState::None,
            handle: None,
            serial_bridge: None,
            recording: None,
            spec: None,
            accel: None,
            viewer: None,
            start_viewer,
            event_buffer: Arc::new(EventBuffer::new(capacity)),
            event_feeder: None,
            self_ref: Weak::new(),
            generation: 0,
            exit_watcher: None,
        }
    }

    /// Construct the Orchestrator already behind its shared `Arc<Mutex<Self>>`, with the
    /// `Weak` self back-reference installed so [`create_instance`](Self::create_instance)
    /// can spawn the exit-watch task that reconciles an unexpected qemu exit to NONE
    /// (issue #28). The production wiring and the shared-lifecycle tests use this; the
    /// bare [`new`](Self::new) is fine for the pure-lifecycle unit tests (their exit
    /// watcher never fires because the self-reference is empty).
    pub fn new_shared(
        driver: Box<dyn QemuDriver>,
        options: OrchestratorOptions,
    ) -> Arc<Mutex<Self>> {
        let orch = Arc::new(Mutex::new(Self::new(driver, options)));
        // We hold the only reference right after construction, so `try_lock` cannot fail.
        orch.try_lock()
            .expect("a freshly constructed Orchestrator is unlocked")
            .self_ref = Arc::downgrade(&orch);
        orch
    }

    /// The current lifecycle state (a cheap accessor for the shutdown hook and
    /// tests, avoiding an [`InstanceView`] clone).
    pub fn state(&self) -> InstanceState {
        self.state
    }

    /// Report the host↔guest folder-sharing configuration (ADR-0014) for `get_share`.
    /// Never returns the host path; it gives the agent what it needs to USE the share
    /// from inside the guest (mount tag, guest mountpoint, read-only, and the exact
    /// `mount` command). The share itself is attached at create time via `share: true`.
    pub fn describe_share(&self) -> ShareReport {
        let available = self
            .options
            .host_share_dir
            .as_deref()
            .is_some_and(|d| !d.trim().is_empty());
        let read_only = self.options.share_readonly != Some(false);
        let guest_mountpoint = self.options.guest_share_dir.clone();
        let mountpoint = guest_mountpoint
            .clone()
            .unwrap_or_else(|| "<your-mountpoint>".to_string());
        let ro = if read_only { ",ro" } else { "" };
        ShareReport {
            available,
            mount_tag: SHARE_MOUNT_TAG.to_string(),
            guest_mountpoint,
            read_only,
            mount_command: available.then(|| {
                format!(
                    "mount -t 9p -o trans=virtio,version=9p2000.L{ro} {SHARE_MOUNT_TAG} {mountpoint}"
                )
            }),
            hint: if available {
                "Set share: true on create_instance to attach the host folder, then run mountCommand \
                 inside the guest (needs the 9p + 9pnet_virtio kernel modules)."
                    .to_string()
            } else {
                "Folder sharing is not configured on this server (QMP_MCP_HOST_SHARE_DIR is unset)."
                    .to_string()
            },
        }
    }

    /// The current Instance generation — the value a fresh exit watcher captures and
    /// [`reconcile_unexpected_exit`](Self::reconcile_unexpected_exit) guards on. Test-only,
    /// so the generation guard can be asserted deterministically.
    #[cfg(test)]
    fn current_generation(&self) -> u64 {
        self.generation
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
            Ok((spec, resolution, handle, serial_bridge)) => {
                // For a vnc Display, arm the Display password over QMP and start the
                // Viewer BEFORE publishing the Instance as RUNNING, so any failure tears
                // the launched qemu down and leaves state NONE (fail-closed, ADR-0010).
                let viewer = if spec.display == DisplayMode::Vnc {
                    match self.arm_display(handle.as_ref()).await {
                        Ok(viewer) => Some(viewer),
                        Err(err) => {
                            // The serial bridge was attached during launch; tear it down too
                            // so a Viewer failure leaves nothing running (fail-closed).
                            if let Some(bridge) = serial_bridge {
                                teardown_serial_bridge(bridge).await;
                            }
                            let _ = handle.close().await;
                            self.state = InstanceState::None;
                            return Err(err);
                        }
                    }
                } else {
                    None
                };
                // Start a fresh Event Buffer for this Instance and capture its QMP
                // async events for its whole life — no events carry over from a
                // previous Instance (the buffer's cursor stays monotonic, though).
                self.event_buffer.reset();
                self.spawn_event_feeder(handle.subscribe_events());
                // Capture the exit signal BEFORE the handle moves into the field; the
                // exit-watch task awaits it to reconcile an unexpected qemu exit to NONE.
                let exited = handle.exited();
                // Store the handle behind an `Arc` so the recording loop can share it (ADR-0017).
                self.handle = Some(Arc::from(handle));
                self.serial_bridge = serial_bridge;
                self.viewer = viewer;
                self.spec = Some(spec.clone());
                self.accel = Some(resolution.accel);
                // A new live Instance: bump the generation the exit-watch task guards on,
                // so a stale watcher for a prior Instance can never clobber this one.
                self.generation = self.generation.wrapping_add(1);
                let generation = self.generation;
                // Auto-start (issues #8, #10; default flipped in ADR-0016) decides the
                // state create lands in. The Guest always launches frozen at the `-S`
                // startup pause; by default we `cont` it below (sub-second) and land
                // RUNNING. With QMP_MCP_AUTO_START=false we skip the `cont` and stay
                // PAUSED — the honest state, agreeing with get_status/query-status
                // (paused/prelaunch until resume_instance).
                let started = self.options.auto_start;
                self.state = if started {
                    InstanceState::Running
                } else {
                    InstanceState::Paused
                };
                self.spawn_exit_watcher(generation, exited);
                // When auto-start is on (the default), resume the Guest now (QMP `cont`)
                // so it begins executing; done AFTER event capture is wired so the boot's
                // events are recorded. With auto-start off it stays PAUSED, frozen at
                // `-S`, until resume_instance.
                if started {
                    self.handle
                        .as_ref()
                        .expect("handle present after publish")
                        .execute("cont", None)
                        .await
                        .map_err(|e| LifecycleError(e.0))?;
                    tracing::info!("Instance RUNNING (auto-started; {})", resolution.reason);
                } else {
                    tracing::info!(
                        "Instance PAUSED — loaded, frozen at the -S startup pause; call \
                         resume_instance to start it ({})",
                        resolution.reason
                    );
                }
                Ok(CreateInstanceResult {
                    state: self.state,
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
    ) -> Result<
        (
            HardwareSpec,
            AccelResolution,
            Box<dyn InstanceHandle>,
            Option<SerialBridge>,
        ),
        LifecycleError,
    > {
        let spec = parse_hardware_spec(candidate).map_err(|e| LifecycleError(e.0))?;
        // Fail-closed coupling (ADR-0010): a vnc Display starts the browser Viewer,
        // which cannot serve without QMP_MCP_VIEWER_PASSWORD. Refuse BEFORE spawning
        // qemu, so nothing is launched when the Viewer could never front it.
        if spec.display == DisplayMode::Vnc {
            self.assert_viewer_configured()?;
        }
        // The machine picks the emulator, unless QMP_MCP_QEMU_BINARY forces one (#18).
        let binary = self
            .options
            .qemu_binary_override
            .clone()
            .unwrap_or_else(|| qemu_binary_for_machine(spec.machine.as_str()));
        // accel=auto's KVM eligibility keys off the LAUNCHED binary's arch (an override
        // may differ from the machine's arch) and the machine (raspi can't use KVM).
        let resolution = resolve_accel(
            spec.accel,
            qemu_arch_of_binary(&binary),
            &self.options.host_arch,
            spec.machine.as_str(),
            || (self.options.kvm_available)(),
        )
        .map_err(|e| LifecycleError(e.0))?;
        let argv = build_argv(&spec, &self.argv_options(resolution.accel))
            .map_err(|e| LifecycleError(e.0))?;
        // Serial Port spool backend (ADR-0015): create the log's parent directory before qemu
        // opens the file; otherwise warn if a `serialSpool` was set where it cannot apply.
        if spec.serial && self.options.serial_backend.is_spool() {
            let path = resolve_serial_spool_path(
                self.options.serial_spool_dir.as_deref(),
                spec.serial_spool.as_deref(),
            )
            .map_err(|e| LifecycleError(e.0))?;
            if let Some(parent) = std::path::Path::new(&path).parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    LifecycleError(format!(
                        "Failed to create the Serial Port spool directory {}: {e}",
                        parent.display()
                    ))
                })?;
            }
        } else if spec.serial_spool.is_some() {
            tracing::warn!(
                "serialSpool={:?} ignored: it applies only with QMP_MCP_SERIAL_BACKEND=spool and \
                 serial: true (backend={}, serial={})",
                spec.serial_spool,
                self.options.serial_backend.as_str(),
                spec.serial
            );
        }
        // Socket-backend Serial Port (ADR-0015): refuse-on-occupied, symmetric to the QMP
        // socket's own check (real_driver). A leftover serial.sock means a stale or foreign
        // owner in the server's runtime dir; refuse with an actionable message rather than let
        // QEMU's server `bind()` fail obscurely with EADDRINUSE (surfaced only as a spawn error).
        if spec.serial && self.options.serial_backend.is_socket() {
            let serial_path = serial_socket_path(&self.options.qmp_socket_path);
            if tokio::fs::symlink_metadata(&serial_path).await.is_ok() {
                return Err(LifecycleError(format!(
                    "The Serial Port socket path {serial_path} is already occupied — refusing to \
                     start rather than clobber or adopt a process this server did not launch. \
                     Remove the stale socket (or stop the other process), then retry."
                )));
            }
        }
        tracing::info!(
            "creating Instance (machine={}, accel={})",
            spec.machine.as_str(),
            resolution.accel.as_str()
        );
        let handle = self
            .driver
            .launch(LaunchRequest {
                binary,
                argv,
                qmp_socket_path: self.options.qmp_socket_path.clone(),
            })
            .await
            .map_err(|e| LifecycleError(format!("Failed to create the Instance: {}", e.0)))?;
        // Socket-backend Serial Port (ADR-0015): dial QEMU's listening serial socket and
        // start the reader NOW — before create_instance issues `cont` — so the guest's boot
        // output is captured from the first byte. Fail-closed: a bridge that will not connect
        // tears the just-launched qemu down rather than leaving a half-wired Instance.
        let serial_bridge = if spec.serial && self.options.serial_backend.is_socket() {
            match attach_serial_bridge(
                &self.options.qmp_socket_path,
                self.options.serial_buffer_bytes,
            )
            .await
            {
                Ok(bridge) => Some(bridge),
                Err(err) => {
                    let _ = handle.close().await;
                    // Belt-and-suspenders: a SIGKILLed qemu would leave its serial.sock behind
                    // (a clean SIGTERM exit removes it), which the refuse-guard above would then
                    // trip on next create — so unlink it on this failure path too.
                    let _ =
                        tokio::fs::remove_file(serial_socket_path(&self.options.qmp_socket_path))
                            .await;
                    return Err(err);
                }
            }
        } else {
            None
        };
        Ok((spec, resolution, handle, serial_bridge))
    }

    /// Assemble the [`ArgvOptions`] the pure argv generator needs from this slice's
    /// options plus the resolved accelerator.
    fn argv_options(&self, accel: Accel) -> ArgvOptions {
        ArgvOptions {
            accel,
            qmp_socket_path: self.options.qmp_socket_path.clone(),
            image_dir: self.options.image_dir.clone(),
            iso_dir: self.options.iso_dir.clone(),
            host_share_dir: self.options.host_share_dir.clone(),
            share_readonly: self.options.share_readonly,
            serial_buffer_bytes: self.options.serial_buffer_bytes,
            serial_backend: self.options.serial_backend,
            serial_spool_dir: self.options.serial_spool_dir.clone(),
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
        // A normal destroy: silence the exit-watch task first, so it cannot double-
        // reconcile when `close` fires the exit signal. The generation/NONE guard also
        // makes it inert, but aborting avoids leaving the task parked. Safe to abort
        // here — destroy runs on a tool task, never the watcher's own task. (The outer
        // mutex already serialises callers, so this cannot interleave with another
        // destroy.)
        self.stop_exit_watcher();
        tracing::info!("destroying Instance");
        self.teardown_current_instance()
            .await
            .map_err(|e| LifecycleError(format!("Failed to destroy the Instance: {}", e.0)))?;
        tracing::info!("Instance destroyed (state NONE)");
        Ok(())
    }

    /// Reconcile the Orchestrator to NONE after the current Instance's qemu exited
    /// WITHOUT an explicit [`destroy_instance`](Self::destroy_instance) — a crash, a
    /// guest-initiated poweroff, or an external kill — running the SAME teardown as
    /// destroy (stop the Viewer, abort the event feeder, close the handle so the managed
    /// QMP socket is removed). Called by the exit-watch task under the shared lock once
    /// the [`InstanceHandle`]'s exit signal fires.
    ///
    /// Guarded twice so it is race-safe: a no-op when `generation` no longer matches (a
    /// stale watcher for a since-destroyed / recreated Instance — it must NEVER clobber a
    /// NEWER Instance), and a no-op when the slot is already empty (a normal destroy beat
    /// it here). Mirrors the TS `Orchestrator.#onProcessExit`.
    async fn reconcile_unexpected_exit(&mut self, generation: u64) {
        if self.generation != generation
            || self.state == InstanceState::None
            || self.handle.is_none()
        {
            return;
        }
        tracing::warn!("Instance process exited unexpectedly; resetting state to NONE");
        // This runs ON the watcher's own task: drop (never abort) its JoinHandle, so we
        // do not cancel this very teardown at its first await point.
        let _ = self.exit_watcher.take();
        if let Err(err) = self.teardown_current_instance().await {
            tracing::warn!("cleanup after the unexpected qemu exit reported: {}", err.0);
        }
        tracing::info!("Instance reconciled to NONE after an unexpected qemu exit");
    }

    /// The teardown BOTH [`destroy_instance`](Self::destroy_instance) and
    /// [`reconcile_unexpected_exit`](Self::reconcile_unexpected_exit) run: take the
    /// handle and Viewer, clear the Instance fields, detach the event feeder and reset
    /// the buffer (settling any pending wait_for_event as a clean timeout), stop the
    /// Viewer, then close the handle — which terminates qemu and removes the managed QMP
    /// socket, so a crashed/SIGKILLed Instance's leftover socket cannot block the next
    /// create. The caller has already guarded that an Instance exists and dealt with the
    /// exit-watch task. Returns the driver `close` result so the explicit-destroy path
    /// can surface it. State ends at `NONE` even if `close` errors (mirrors the TS
    /// `finally`).
    async fn teardown_current_instance(&mut self) -> Result<(), DriverError> {
        let handle = self
            .handle
            .take()
            .expect("handle present in a non-NONE state");
        let viewer = self.viewer.take();
        // Display recording (ADR-0017): stop the frame loop and finalize/kill ffmpeg BEFORE the
        // handle closes qemu, so the loop stops issuing `screendump` and no ffmpeg child leaks.
        // Runs on both destroy and unexpected-exit reconcile (this is their shared teardown).
        if let Some(recording) = self.recording.take() {
            let _ = finalize_recording(recording).await;
        }
        // Socket-backend Serial Port: stop the reader and unlink the socket before qemu goes
        // away, so a SIGKILLed qemu's leftover socket cannot block the next create.
        if let Some(bridge) = self.serial_bridge.take() {
            teardown_serial_bridge(bridge).await;
        }
        self.spec = None;
        self.accel = None;
        self.state = InstanceState::Stopped;
        // Detach from the Instance's event stream and clear the buffer; events do not
        // outlive the Instance. Mirrors the TS destroy/exit path.
        self.stop_event_feeder();
        self.event_buffer.reset();
        // Stop the Viewer alongside qemu — its lifetime equals the Instance's (ADR-0010).
        if let Some(viewer) = viewer {
            viewer.stop().await;
        }
        let closed = handle.close().await;
        self.state = InstanceState::None;
        closed
    }

    /// Spawn the background task that awaits the current Instance's exit signal and, when
    /// qemu vanishes WITHOUT an explicit destroy (crash, guest poweroff, external kill),
    /// reconciles the Orchestrator to NONE. It upgrades the `Weak` self back-reference
    /// and re-checks the captured `generation` under the lock, so a stale watcher for a
    /// since-destroyed/recreated Instance is a no-op. A bare (non-shared) Orchestrator
    /// has an empty self-reference, so its watcher upgrades to nothing and is inert.
    fn spawn_exit_watcher(&mut self, generation: u64, exited: ExitedFuture) {
        // A previous watcher should already be stopped, but be defensive.
        self.stop_exit_watcher();
        let self_ref = self.self_ref.clone();
        self.exit_watcher = Some(tokio::spawn(async move {
            exited.await;
            if let Some(orch) = self_ref.upgrade() {
                orch.lock()
                    .await
                    .reconcile_unexpected_exit(generation)
                    .await;
            }
        }));
    }

    /// Abort and drop the current exit-watch task, if any, so it cannot reconcile after
    /// an explicit destroy (or before a fresh watcher is spawned).
    fn stop_exit_watcher(&mut self) {
        if let Some(watcher) = self.exit_watcher.take() {
            watcher.abort();
        }
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

    /// Drain the Serial Port's ring buffer via QMP `ringbuf-read`: returns the guest serial
    /// output produced since the last read (the ring is destructive), up to `max_bytes`
    /// (defaulting to the full buffer). Rejects when no Instance is running. Mirrors the TS
    /// `Orchestrator.readSerial`.
    pub async fn read_serial(
        &self,
        max_bytes: Option<u32>,
        format: SerialFormat,
    ) -> Result<SerialReadResult, LifecycleError> {
        let size = max_bytes.unwrap_or(self.options.serial_buffer_bytes);
        let output = match self.options.serial_backend {
            SerialBackend::Ringbuf => {
                // Ringbuf: drain the in-QEMU buffer over QMP (destructive).
                let value = self
                    .require_handle("read its Serial Port")?
                    .execute(
                        "ringbuf-read",
                        Some(serde_json::json!({
                            "device": SERIAL_CHARDEV_ID,
                            "size": size,
                            "format": format.as_str(),
                        })),
                    )
                    .await
                    .map_err(|e| LifecycleError(e.0))?;
                // QMP `ringbuf-read` returns the read data as a bare string.
                value.as_str().unwrap_or_default().to_string()
            }
            SerialBackend::Spool => {
                // Spool: tail the persistent log file, non-destructively.
                self.require_handle("read its Serial Port")?;
                let path = resolve_serial_spool_path(
                    self.options.serial_spool_dir.as_deref(),
                    self.spec.as_ref().and_then(|s| s.serial_spool.as_deref()),
                )
                .map_err(|e| LifecycleError(e.0))?;
                let bytes = tokio::fs::read(&path).await.unwrap_or_default();
                let start = bytes.len().saturating_sub(size as usize);
                let slice = &bytes[start..];
                match format {
                    SerialFormat::Utf8 => String::from_utf8_lossy(slice).into_owned(),
                    SerialFormat::Base64 => base64_encode(slice),
                }
            }
            SerialBackend::Socket => {
                // Socket: drain (destructively, like ringbuf) the in-server ring the reader
                // task fills from QEMU's serial socket — up to `size` bytes from the oldest.
                self.require_handle("read its Serial Port")?;
                let bridge = self
                    .serial_bridge
                    .as_ref()
                    .ok_or_else(serial_bridge_missing)?;
                let drained: Vec<u8> = {
                    let mut ring = bridge.ring.lock().expect("serial ring mutex poisoned");
                    let take = (size as usize).min(ring.len());
                    ring.drain(..take).collect()
                };
                match format {
                    SerialFormat::Utf8 => String::from_utf8_lossy(&drained).into_owned(),
                    SerialFormat::Base64 => base64_encode(&drained),
                }
            }
        };
        Ok(SerialReadResult {
            bytes: output.chars().count(),
            format: format.as_str().to_string(),
            output,
        })
    }

    /// Write raw bytes to the Guest Serial Port via QMP `ringbuf-write` — types `data` into the
    /// guest console (keyboard-equivalent input). Refused unless the operator enabled writing
    /// (`QMP_MCP_ALLOW_SERIAL_WRITE`); no auto-newline (the caller submits its own). Rejects when
    /// no Instance is running. Mirrors the TS `Orchestrator.writeSerial`.
    pub async fn write_serial(
        &self,
        data: String,
        format: SerialFormat,
    ) -> Result<SerialWriteResult, LifecycleError> {
        if self.options.serial_backend.is_spool() {
            return Err(LifecycleError(
                "The spool Serial Port backend is output-only; writing needs the ringbuf backend \
                 (unset QMP_MCP_SERIAL_BACKEND or set it to ringbuf)."
                    .to_string(),
            ));
        }
        if !self.options.allow_serial_write {
            return Err(LifecycleError(
                "Writing to the Serial Port is disabled. Set QMP_MCP_ALLOW_SERIAL_WRITE=true to \
                 enable write_serial (it types raw input into the guest console)."
                    .to_string(),
            ));
        }
        // Socket backend: write raw bytes straight into QEMU's serial socket — a real,
        // connected serial line, so this input actually reaches the guest console (unlike
        // ringbuf-write). Decode base64 ourselves; utf8 goes as its raw bytes.
        if self.options.serial_backend.is_socket() {
            self.require_handle("write to its Serial Port")?;
            let bridge = self
                .serial_bridge
                .as_ref()
                .ok_or_else(serial_bridge_missing)?;
            let raw = match format {
                SerialFormat::Utf8 => data.into_bytes(),
                SerialFormat::Base64 => base64_decode(&data).map_err(LifecycleError)?,
            };
            let bytes_written = raw.len();
            let mut write = bridge.write.lock().await;
            // BOUND the write: this method runs holding the single global Orchestrator lock, and
            // QEMU applies real serial flow control — if the guest is not draining its UART RX
            // (paused, panicked, or no console reader), the socket buffer fills and an unbounded
            // write would block FOREVER, wedging every other tool (destroy/resume included). A
            // timeout releases the lock and returns an actionable error instead.
            let write_result = tokio::time::timeout(SERIAL_WRITE_TIMEOUT, async {
                write.write_all(&raw).await?;
                write.flush().await
            })
            .await;
            match write_result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    return Err(LifecycleError(format!(
                        "Failed to write to the Serial Port socket: {e}"
                    )))
                }
                Err(_elapsed) => {
                    return Err(LifecycleError(format!(
                        "Timed out after {}ms writing to the Serial Port. The guest is not draining \
                         its serial input — is it paused, has it panicked, or is nothing reading the \
                         console (no getty on the shown console device)?",
                        SERIAL_WRITE_TIMEOUT.as_millis()
                    )))
                }
            }
            return Ok(SerialWriteResult { bytes_written });
        }
        let bytes_written = data.chars().count();
        self.require_handle("write to its Serial Port")?
            .execute(
                "ringbuf-write",
                Some(serde_json::json!({
                    "device": SERIAL_CHARDEV_ID,
                    "data": data,
                    "format": format.as_str(),
                })),
            )
            .await
            .map_err(|e| LifecycleError(e.0))?;
        Ok(SerialWriteResult { bytes_written })
    }

    /// Report the Serial Port configuration and a best-effort guest console-device guess
    /// (`get_serial`, ADR-0015). Advisory and infallible — like `describe_share`, it reports
    /// config even with no Instance running. Mirrors the TS `Orchestrator.describeSerial`.
    pub fn describe_serial(&self) -> SerialReport {
        let console_device = self
            .spec
            .as_ref()
            .and_then(|s| guess_console_device(s.machine.as_str()))
            .map(str::to_string);
        let enabled = self.spec.as_ref().is_some_and(|s| s.serial);
        let backend = self.options.serial_backend;
        let spool_subdir = if backend.is_spool() {
            self.spec.as_ref().and_then(|s| s.serial_spool.clone())
        } else {
            None
        };
        SerialReport {
            backend: backend.as_str().to_string(),
            buffer_bytes: self.options.serial_buffer_bytes,
            // Ringbuf drains on read; spool tails the persistent file non-destructively.
            read_semantics: if backend.is_spool() { "tail" } else { "drain" }.to_string(),
            // Spool is output-only, so it is never writable regardless of the write gate.
            writable: self.options.allow_serial_write && !backend.is_spool(),
            enabled,
            spool_subdir,
            console_device,
            hint: "Set serial: true on create_instance to attach the Serial Port, then read its \
                   output with read_serial (ringbuf and socket drain on read; spool tails a \
                   persistent file). The socket backend also makes write_serial a real interactive \
                   console — typed input reaches the guest as a connected serial line. The guest \
                   must use the shown console device in its own console= kernel cmdline for output \
                   to appear; raspi* boards have two UARTs and may need adjusting."
                .to_string(),
        }
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

    /// Report the display-recording capability and current state (`get_recording`, ADR-0017).
    /// Advisory and infallible — like `describe_share`/`describe_serial`, it reports config even
    /// with no Instance running. Availability is: ffmpeg resolved AND an output dir set AND the
    /// selected codec compiled into the ffmpeg build (the last checked only when the first two
    /// hold, so no ffmpeg is spawned when recording is plainly unconfigured). Mirrors the TS
    /// `Orchestrator.describeRecording`.
    pub fn describe_recording(&self) -> RecordingReport {
        let codec = self.options.recording_codec.clone();
        let (available, reason) = recording_capability(
            self.options.ffmpeg_binary.as_deref(),
            self.options.recording_dir.as_deref(),
            &codec,
            || (self.options.recording_encoder_available)(&codec),
        );
        RecordingReport {
            available,
            reason,
            codec,
            crf: self.options.recording_crf,
            max_fps: self.options.recording_max_fps,
            pixfmt: self.options.recording_pixfmt.clone(),
            active: self.recording.is_some(),
            active_path: self.recording.as_ref().map(|r| r.output_path.clone()),
            output_dir: self.options.recording_dir.clone(),
        }
    }

    /// Start recording the running Instance's display to a video file (ADR-0017, the documented
    /// `screendump`-loop fallback). A co-located ffmpeg child encodes the stream; a background task
    /// loops QMP `screendump` at up to `recording_max_fps`, feeding each PNG frame into ffmpeg's
    /// stdin (`-f image2pipe`). Fails closed — with an actionable message — when there is no
    /// Instance, the Instance has no display adapter (no framebuffer to capture), the recording
    /// capability is unavailable (no ffmpeg / no output dir / codec absent), or a recording is
    /// already running. An agent-supplied `name` is validated with the same allowlist as
    /// `serialSpool` and resolved under `QMP_MCP_RECORDING_DIR`; otherwise a unique name is
    /// generated. Returns the output path and codec (the file is large, so it is NOT returned
    /// inline — the operator retrieves it). Mirrors the TS `Orchestrator.startRecording`.
    pub async fn start_recording(
        &mut self,
        name: Option<String>,
    ) -> Result<StartRecordingResult, LifecycleError> {
        // 1) A running Instance is required, and we need a shareable handle for the loop task.
        self.require_handle("start a display recording")?;
        let handle = Arc::clone(
            self.handle
                .as_ref()
                .expect("handle present after require_handle"),
        );
        // 2) A display adapter must be present — `screendump` captures a framebuffer, and a
        //    headless (displayDevice: none) Instance has none.
        let has_display = self
            .spec
            .as_ref()
            .is_some_and(|s| s.display_device != DisplayDevice::None);
        if !has_display {
            return Err(LifecycleError(
                "The running Instance has no display adapter (displayDevice is \"none\"), so there \
                 is no framebuffer to record. Recreate it with a displayDevice such as \
                 \"virtio-gpu\", \"vga\", or \"ramfb\"."
                    .to_string(),
            ));
        }
        // 3) The recording capability must be available (ffmpeg + output dir + selected codec).
        let codec = self.options.recording_codec.clone();
        let (available, reason) = recording_capability(
            self.options.ffmpeg_binary.as_deref(),
            self.options.recording_dir.as_deref(),
            &codec,
            || (self.options.recording_encoder_available)(&codec),
        );
        if !available {
            return Err(LifecycleError(reason));
        }
        // 4) One recording per Instance.
        if self.recording.is_some() {
            return Err(LifecycleError(
                "A display recording is already running. Stop it with stop_recording before \
                 starting another."
                    .to_string(),
            ));
        }

        // Resolve the output path under the operator root (agent names only the file).
        let output_path =
            resolve_recording_path(self.options.recording_dir.as_deref(), name.as_deref())?;
        if let Some(parent) = std::path::Path::new(&output_path).parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                LifecycleError(format!(
                    "Failed to create the recording output directory {}: {e}",
                    parent.display()
                ))
            })?;
        }
        // A server-chosen transient path the `screendump` loop writes each PNG frame to.
        let frame_dir = std::env::temp_dir().join("qmp-mcp").join("recordings");
        tokio::fs::create_dir_all(&frame_dir).await.map_err(|e| {
            LifecycleError(format!(
                "Failed to create the recording frame directory {}: {e}",
                frame_dir.display()
            ))
        })?;
        let frame_path = recording_frame_path(&frame_dir);

        // Spawn the ffmpeg encoder: stdin piped (fed the PNG frames), stdout silenced, stderr
        // PIPED so the post-spawn liveness check (R5) can report why an immediate exit happened,
        // and `kill_on_drop(true)` (R4) so a dropped `Recording` can never leak the process.
        let ffmpeg = self
            .options
            .ffmpeg_binary
            .as_deref()
            .expect("capability check guaranteed ffmpeg is resolved");
        let args = build_ffmpeg_args(
            &codec,
            self.options.recording_crf,
            self.options.recording_max_fps,
            &self.options.recording_pixfmt,
            &output_path,
        );
        let mut child = Command::new(ffmpeg)
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                LifecycleError(format!(
                    "Failed to start ffmpeg ({ffmpeg}) for display recording: {e}. Check \
                     QMP_MCP_FFMPEG_BINARY or that ffmpeg is installed."
                ))
            })?;
        let mut stderr = child.stderr.take();

        // Post-spawn liveness (R5, parity with the TS T5): ffmpeg that dies immediately (bad
        // codec/pixfmt/output) must surface as an actionable start error, not a silent empty file.
        tokio::time::sleep(POST_SPAWN_LIVENESS_WINDOW).await;
        if let Ok(Some(status)) = child.try_wait() {
            let captured = if let Some(mut e) = stderr.take() {
                let mut buf = Vec::new();
                let _ = e.read_to_end(&mut buf).await;
                String::from_utf8_lossy(&buf).trim().to_string()
            } else {
                String::new()
            };
            // Best-effort remove the empty/partial output so the host is left clean.
            let _ = tokio::fs::remove_file(&output_path).await;
            return Err(LifecycleError(format!(
                "ffmpeg exited immediately ({status}) after starting the display recording \
                 ({ffmpeg}, codec \"{codec}\", pixfmt \"{}\"). Check the codec/pixel-format/output \
                 path. ffmpeg stderr: {}",
                self.options.recording_pixfmt,
                if captured.is_empty() {
                    "(none)"
                } else {
                    captured.as_str()
                }
            )));
        }
        // Alive: drain stderr in the background so a full pipe never blocks ffmpeg (aborted on
        // teardown). If stderr somehow wasn't captured, an inert task keeps the field uniform.
        let stderr_task = match stderr.take() {
            Some(e) => tokio::spawn(drain_ffmpeg_stderr(e)),
            None => tokio::spawn(async {}),
        };

        let stdin = child
            .stdin
            .take()
            .expect("ffmpeg child was spawned with a piped stdin");
        let stdin = Arc::new(Mutex::new(stdin));

        // Pace the loop to the target fps; a background task pumps frames until stopped.
        let interval = Duration::from_millis(1_000 / self.options.recording_max_fps.max(1) as u64);
        let loop_task = tokio::spawn(run_recording_loop(
            handle,
            Arc::clone(&stdin),
            frame_path.clone(),
            interval,
        ));

        let file_name = std::path::Path::new(&output_path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| output_path.clone());
        self.recording = Some(Recording {
            child,
            stdin: Some(stdin),
            loop_task,
            stderr_task,
            output_path: output_path.clone(),
            name: file_name.clone(),
            codec: codec.clone(),
            frame_path,
            started_at: Instant::now(),
        });
        tracing::info!("display recording started -> {output_path} (codec {codec})");
        Ok(StartRecordingResult {
            path: output_path,
            name: file_name,
            codec,
        })
    }

    /// Stop the running display recording (ADR-0017): abort the frame loop, close ffmpeg's stdin so
    /// it flushes and finalizes the container, and await the child. Returns the finalized file's
    /// path, size, and approximate duration. Fails closed when no recording is running. Mirrors the
    /// TS `Orchestrator.stopRecording`.
    pub async fn stop_recording(&mut self) -> Result<StopRecordingResult, LifecycleError> {
        let recording = self.recording.take().ok_or_else(|| {
            LifecycleError(
                "No display recording is running. Start one with start_recording first."
                    .to_string(),
            )
        })?;
        let result = finalize_recording(recording).await;
        tracing::info!(
            "display recording stopped -> {} ({} bytes, {} ms)",
            result.path,
            result.size_bytes,
            result.duration_ms
        );
        Ok(result)
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

    /// Enforce the fail-closed Display↔Viewer coupling (ADR-0010): a `display: vnc`
    /// spec requires `QMP_MCP_VIEWER_PASSWORD`. Returns an actionable [`LifecycleError`]
    /// naming the variable when it is unset, so create_instance is refused before qemu
    /// is spawned. Mirrors the TS `#assertViewerConfigured`.
    fn assert_viewer_configured(&self) -> Result<(), LifecycleError> {
        match self.options.viewer_password.as_deref() {
            Some(password) if !password.is_empty() => Ok(()),
            _ => Err(LifecycleError(
                "The Hardware Spec requested display \"vnc\", which starts the noVNC Viewer, but \
                 QMP_MCP_VIEWER_PASSWORD is not set. Set QMP_MCP_VIEWER_PASSWORD to a strong \
                 password to enable the Viewer, or use display \"none\" for a headless Instance."
                    .to_string(),
            )),
        }
    }

    /// Arm the vnc Display and start its Viewer. A fresh VNC password is generated and
    /// set over QMP (`set_password`) — never placed in argv, so it stays out of `ps` —
    /// then handed to the Viewer to embed in the post-auth page for auto-authentication.
    /// On any failure a [`LifecycleError`] is returned (and the caller tears the launched
    /// qemu down), so the Instance never reaches RUNNING half-armed. Mirrors the TS
    /// `#armDisplay`.
    async fn arm_display(
        &self,
        handle: &dyn InstanceHandle,
    ) -> Result<Box<dyn ViewerHandle>, LifecycleError> {
        let vnc_password = generate_vnc_password();
        handle
            .execute(
                "set_password",
                Some(serde_json::json!({ "protocol": "vnc", "password": vnc_password })),
            )
            .await
            .map_err(|e| {
                LifecycleError(format!(
                    "Failed to start the noVNC Viewer for the vnc Display: {}",
                    e.0
                ))
            })?;
        self.start_viewer
            .start(ViewerOptions {
                host: self.options.viewer_host.clone(),
                port: self.options.viewer_port,
                // assert_viewer_configured() guaranteed a non-empty password before launch.
                password: self.options.viewer_password.clone().unwrap_or_default(),
                // Optional username enforcement (QMP_MCP_VIEWER_USER); None = password-only.
                user: self.options.viewer_user.clone(),
                vnc_host: VNC_LOOPBACK_HOST.to_string(),
                vnc_port: VNC_LOOPBACK_PORT,
                vnc_password,
            })
            .await
            .map_err(|e| {
                LifecycleError(format!(
                    "Failed to start the noVNC Viewer for the vnc Display: {}",
                    e.0
                ))
            })
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

/// Generate the internal VNC Display password (ADR-0010). The VNC auth scheme
/// truncates the password to 8 characters, so 8 alphanumerics is the effective
/// maximum; QEMU and noVNC truncate identically, so the armed and embedded passwords
/// always match. Ambiguous glyphs (0/O, 1/l/I) are omitted so it stays copy-safe if
/// ever surfaced. Draws from a CSPRNG (`getrandom`) with rejection sampling, so the
/// selection is UNBIASED — 256 is not a multiple of the 55-char alphabet, so a plain
/// `byte % 55` would skew toward the first 36 glyphs. Mirrors the TS
/// `generateVncPassword` (`crypto.randomInt`).
fn generate_vnc_password() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789";
    let n = ALPHABET.len();
    // Largest multiple of `n` that fits in a byte's range; bytes at or above it are
    // rejected so every accepted byte maps to a glyph with equal probability.
    let limit = 256 - (256 % n);
    let mut out = String::with_capacity(8);
    while out.len() < 8 {
        let mut byte = [0u8; 1];
        getrandom::fill(&mut byte).expect("the OS CSPRNG must be available");
        let value = byte[0] as usize;
        if value < limit {
            out.push(ALPHABET[value % n] as char);
        }
    }
    out
}

/// How long to wait for QEMU's serial `socket` chardev to accept our connection. QEMU
/// creates the listening socket during chardev init — before the QMP socket the launch
/// already dialed — so this normally connects on the first try; the retry only covers a
/// tiny startup race.
const SERIAL_DIAL_TIMEOUT: Duration = Duration::from_millis(2_000);
/// Delay between serial-socket connection attempts.
const SERIAL_DIAL_INTERVAL: Duration = Duration::from_millis(25);
/// Read chunk size for the serial reader task.
const SERIAL_READ_CHUNK: usize = 4_096;
/// Upper bound on a single `write_serial` to the serial socket. Bounds the time the global
/// Orchestrator lock is held when the guest is not draining its serial input, so a stuck
/// guest can never wedge the whole server.
const SERIAL_WRITE_TIMEOUT: Duration = Duration::from_millis(2_000);

/// The live bridge to a socket-backend Serial Port (ADR-0015). Owns the write half of the
/// UNIX-socket connection to QEMU's `serial` chardev (interactive `write_serial`), the
/// bounded ring the reader task drains guest output into (`read_serial`), the reader task
/// itself (aborted on teardown), and the server-owned socket path (removed on teardown).
#[derive(Debug)]
struct SerialBridge {
    /// Write half of the serial socket; `write_serial` types raw bytes into the guest here.
    write: Mutex<OwnedWriteHalf>,
    /// Bounded ring of captured guest serial output; `read_serial` drains it destructively,
    /// matching the ringbuf backend's drain-on-read semantics.
    ring: Arc<std::sync::Mutex<VecDeque<u8>>>,
    /// The reader task pumping the socket into `ring`; aborted on teardown.
    reader: JoinHandle<()>,
    /// The server-owned serial socket path, unlinked on teardown (belt-and-suspenders: QEMU
    /// removes its own listening socket on a clean exit, but a SIGKILLed qemu leaves it).
    socket_path: String,
}

/// Append `data` to a bounded serial ring, evicting the oldest bytes once it exceeds `cap`
/// — the same overwrite-oldest behaviour QEMU's `ringbuf` chardev has, so a slow reader
/// never blocks the guest's serial writes (it just loses the oldest unread output).
fn ring_push_bounded(ring: &mut VecDeque<u8>, data: &[u8], cap: usize) {
    ring.extend(data.iter().copied());
    let overflow = ring.len().saturating_sub(cap);
    if overflow > 0 {
        ring.drain(..overflow);
    }
}

/// The reader task body: pump the serial socket's read half into the bounded ring until the
/// socket closes (guest gone) or errors. Always draining is what keeps the socket buffers
/// empty, so the guest never stalls writing to its serial port (the ringbuf backend's flaw).
async fn run_serial_reader(
    mut read: OwnedReadHalf,
    ring: Arc<std::sync::Mutex<VecDeque<u8>>>,
    cap: usize,
) {
    let mut buf = [0u8; SERIAL_READ_CHUNK];
    loop {
        match read.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut ring = ring.lock().expect("serial ring mutex poisoned");
                ring_push_bounded(&mut ring, &buf[..n], cap);
            }
        }
    }
}

/// Dial QEMU's serial `socket` chardev (server-owned UNIX socket), retrying briefly until it
/// accepts, then start the reader task and return the live [`SerialBridge`]. Called during
/// launch — BEFORE the `cont` — so the connection is up before the guest emits a byte and no
/// boot output is lost. Fails (fail-closed) if the socket never accepts within the timeout.
async fn attach_serial_bridge(
    qmp_socket_path: &str,
    buffer_bytes: u32,
) -> Result<SerialBridge, LifecycleError> {
    let socket_path = serial_socket_path(qmp_socket_path);
    let deadline = Instant::now() + SERIAL_DIAL_TIMEOUT;
    let stream = loop {
        match UnixStream::connect(&socket_path).await {
            Ok(stream) => break stream,
            Err(err) => {
                if Instant::now() >= deadline {
                    return Err(LifecycleError(format!(
                        "Failed to connect to the Serial Port socket at {socket_path} within {}ms: \
                         {err}. The socket backend needs QEMU's server chardev to be listening.",
                        SERIAL_DIAL_TIMEOUT.as_millis()
                    )));
                }
                sleep(SERIAL_DIAL_INTERVAL).await;
            }
        }
    };
    let (read_half, write_half) = stream.into_split();
    let ring = Arc::new(std::sync::Mutex::new(VecDeque::new()));
    let reader = tokio::spawn(run_serial_reader(
        read_half,
        Arc::clone(&ring),
        buffer_bytes as usize,
    ));
    Ok(SerialBridge {
        write: Mutex::new(write_half),
        ring,
        reader,
        socket_path,
    })
}

/// Tear a [`SerialBridge`] down: stop the reader task and unlink the server-owned socket
/// file. Dropping the bridge also closes the write half. Run from every path that discards
/// a bridge — teardown, and the fail-closed create paths.
async fn teardown_serial_bridge(bridge: SerialBridge) {
    bridge.reader.abort();
    let _ = tokio::fs::remove_file(&bridge.socket_path).await;
}

/// The error when a socket-backend serial op finds no attached bridge — no running Instance,
/// or one created without `serial: true`. Shared by `read_serial`/`write_serial`.
fn serial_bridge_missing() -> LifecycleError {
    LifecycleError(
        "The Serial Port socket bridge is not attached — create an Instance with serial: true \
         (backend QMP_MCP_SERIAL_BACKEND=socket) first."
            .to_string(),
    )
}

// ---------------------------------------------------------------------------
// Display recording (ADR-0017) — the documented `screendump`-loop → ffmpeg fallback.
// ---------------------------------------------------------------------------

/// The file extension (and thus container) recordings are written with. Matroska holds every
/// encoder the operator may select (`libx264`/`libx265`/`libvpx-vp9`/`libsvtav1`), so it never
/// fails the way an `.mp4` muxer would for VP9/AV1 — a robust, codec-agnostic default.
const RECORDING_EXTENSION: &str = "mkv";

/// How long to wait for ffmpeg to flush and finalize the container after its stdin closes, before
/// we give up and kill it. Ample for a low-fps screen capture; bounds the teardown path.
const FFMPEG_FINALIZE_TIMEOUT: Duration = Duration::from_millis(5_000);

/// How long to wait for ffmpeg to die after a `start_kill()` on finalize-timeout, before giving up
/// (R3). Bounds the SECOND `wait()` so teardown never awaits unbounded even if the kill is slow;
/// `kill_on_drop(true)` (R4) is the final backstop when the `Recording` is dropped.
const FFMPEG_KILL_TIMEOUT: Duration = Duration::from_millis(2_000);

/// How long a freshly-spawned ffmpeg is watched for an immediate exit before it is declared live
/// (R5, post-spawn liveness). Long enough to catch an instant failure (bad codec/pixfmt), short
/// enough not to delay a healthy start.
const POST_SPAWN_LIVENESS_WINDOW: Duration = Duration::from_millis(150);

/// Bound on the one-shot startup encoder probe (`ffmpeg -encoders`, R1), so a hung ffmpeg at
/// startup can never wedge server boot.
const FFMPEG_PROBE_TIMEOUT: Duration = Duration::from_millis(5_000);

/// Process-unique counter making each server-chosen recording frame path unique (with the PID and
/// a high-resolution timestamp), so concurrent test runs never collide.
static RECORDING_SEQ: AtomicU64 = AtomicU64::new(0);

/// The live display recording (ADR-0017): the ffmpeg child encoding the stream, its stdin (fed the
/// PNG frames — shared with the loop task, closed on stop to finalize the file), the frame-loop
/// task (aborted on teardown), the output path, the transient frame path (removed on teardown), and
/// the start time (for the reported duration). Mirrors the [`SerialBridge`] lifecycle discipline.
struct Recording {
    /// The ffmpeg encoder child; awaited on stop so the container is finalized before we return.
    /// Spawned with `kill_on_drop(true)` (R4) so a dropped `Recording` can never leak the process.
    child: Child,
    /// ffmpeg's stdin, shared with the loop task. Behind an `Arc<Mutex>` so the loop can write
    /// frames while the Orchestrator retains ownership; dropping the last reference (after the loop
    /// task is aborted+joined) closes the pipe, giving ffmpeg EOF so it finalizes the file.
    stdin: Option<Arc<Mutex<ChildStdin>>>,
    /// The frame-loop task issuing `screendump` and pumping frames; aborted on teardown.
    loop_task: JoinHandle<()>,
    /// The background task draining ffmpeg's stderr (so a full pipe never blocks the encoder);
    /// aborted on teardown. Present once the post-spawn liveness check passed (R5).
    stderr_task: JoinHandle<()>,
    /// The host path the recording is written to (under `QMP_MCP_RECORDING_DIR`).
    output_path: String,
    /// The output file's base name (e.g. `run1.mkv`), reported by start/stop.
    name: String,
    /// The codec the recording is encoded with, reported by start/stop.
    codec: String,
    /// The transient per-frame PNG path the loop writes and re-reads; removed on teardown.
    frame_path: PathBuf,
    /// When the recording started, for the reported wall-clock duration.
    started_at: Instant,
}

/// Decide whether display recording is available and why (ADR-0017), as a pure function of the
/// resolved ffmpeg binary, the output directory, the selected codec, and a lazily-evaluated
/// encoder-presence probe. The probe (`ffmpeg -encoders`) runs ONLY when ffmpeg and the output dir
/// are both present, so an unconfigured server never spawns ffmpeg. Returns `(available, reason)`
/// with an actionable reason naming the offending variable. Mirrors the TS `recordingCapability`.
fn recording_capability(
    ffmpeg: Option<&str>,
    recording_dir: Option<&str>,
    codec: &str,
    encoder_present: impl FnOnce() -> bool,
) -> (bool, String) {
    if ffmpeg.is_none_or(str::is_empty) {
        return (
            false,
            "Display recording requires ffmpeg, which was not found. Install ffmpeg or set \
             QMP_MCP_FFMPEG_BINARY to its path."
                .to_string(),
        );
    }
    if recording_dir.is_none_or(|d| d.trim().is_empty()) {
        return (
            false,
            "Display recording requires an output directory. Set QMP_MCP_RECORDING_DIR to the host \
             directory recordings are written under."
                .to_string(),
        );
    }
    if !encoder_present() {
        return (
            false,
            format!(
                "Display recording codec \"{codec}\" is not compiled into this ffmpeg build \
                 (checked ffmpeg -encoders). Set QMP_MCP_RECORDING_CODEC to an available encoder \
                 (e.g. libx264), or rebuild ffmpeg with it."
            ),
        );
    }
    (
        true,
        format!("Display recording is available (ffmpeg present, codec \"{codec}\")."),
    )
}

/// Parse the output of `ffmpeg -encoders` into the set of encoder names (ADR-0017). The listing
/// has a flags column (six chars over `[A-Z.]`) then the encoder name and a description, after a
/// `------` separator line; this returns the second token of every post-separator entry. Pure so
/// it can be unit-tested against a captured corpus without spawning ffmpeg. Mirrors the TS
/// `parseFfmpegEncoders`.
pub fn parse_ffmpeg_encoders(output: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut past_header = false;
    for line in output.lines() {
        if !past_header {
            if line.contains("------") {
                past_header = true;
            }
            continue;
        }
        let mut tokens = line.split_whitespace();
        if let Some(flags) = tokens.next() {
            let flags_ok =
                flags.len() == 6 && flags.bytes().all(|b| b.is_ascii_uppercase() || b == b'.');
            if flags_ok {
                if let Some(name) = tokens.next() {
                    names.insert(name.to_string());
                }
            }
        }
    }
    names
}

/// Probe the ffmpeg build's compiled-in encoders ONCE, bounded (R1). Runs
/// `ffmpeg -hide_banner -encoders` via `tokio::process` under a [`FFMPEG_PROBE_TIMEOUT`] so a hung
/// or missing ffmpeg can never wedge startup, and parses the result with [`parse_ffmpeg_encoders`].
/// Returns an empty set when ffmpeg is unresolved, cannot be run, or times out — recording is then
/// reported unavailable. The server calls this ONCE at boot and caches the set, so
/// `recording_encoder_available` becomes an instant in-memory lookup (no per-request spawn, no
/// blocking call under the Orchestrator lock).
pub async fn probe_ffmpeg_encoders(binary: Option<&str>) -> HashSet<String> {
    let Some(binary) = binary.filter(|b| !b.is_empty()) else {
        return HashSet::new();
    };
    let fut = Command::new(binary)
        .args(["-hide_banner", "-encoders"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();
    match tokio::time::timeout(FFMPEG_PROBE_TIMEOUT, fut).await {
        Ok(Ok(output)) if output.status.success() => {
            parse_ffmpeg_encoders(&String::from_utf8_lossy(&output.stdout))
        }
        _ => HashSet::new(),
    }
}

/// Drain an ffmpeg child's stderr to the debug log so a full stderr pipe never blocks the encoder
/// (R5 keeps stderr piped for the liveness check, so it must be drained for the recording's life).
/// Ends when the pipe closes; aborted on teardown.
async fn drain_ffmpeg_stderr(mut stderr: ChildStderr) {
    let mut buf = [0u8; 4096];
    loop {
        match stderr.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => tracing::debug!("ffmpeg: {}", String::from_utf8_lossy(&buf[..n]).trim_end()),
        }
    }
}

/// Build the ffmpeg argv for the `screendump`-loop encoder (ADR-0017), a PURE, unit-testable
/// function pinned byte-for-byte by the shared golden corpus (`testdata/ffmpeg-argv/*.json`,
/// ADR-0012). Reads concatenated PNG frames from stdin (`-f image2pipe -i pipe:0`), stamps each
/// with a real wall-clock presentation timestamp (`-use_wallclock_as_timestamps 1`), encodes with
/// the selected codec at the quality-targeted CRF and pixel format, and muxes variable-frame-rate
/// (`-fps_mode vfr`) so the output's duration matches real elapsed time. `libx264` additionally
/// gets the screen-content `-tune stillimage`; `-y` overwrites the output.
///
/// `max_fps` is intentionally NOT part of the argv: with wallclock timestamps + VFR it is only the
/// capture-loop pacing cap, so an input `-framerate` would double-count time and skew duration. It
/// stays in the signature (and the shared fixture tuple) so the golden corpus pins that it never
/// leaks into the argv.
pub fn build_ffmpeg_args(
    codec: &str,
    crf: u32,
    max_fps: u32,
    pixfmt: &str,
    out_path: &str,
) -> Vec<String> {
    // Referenced only to document that max_fps is deliberately excluded from the argv (see above).
    let _ = max_fps;
    let mut args = vec![
        "-f".to_string(),
        "image2pipe".to_string(),
        "-use_wallclock_as_timestamps".to_string(),
        "1".to_string(),
        "-i".to_string(),
        "pipe:0".to_string(),
        "-c:v".to_string(),
        codec.to_string(),
    ];
    if codec == "libx264" {
        args.push("-tune".to_string());
        args.push("stillimage".to_string());
    }
    args.push("-crf".to_string());
    args.push(crf.to_string());
    args.push("-pix_fmt".to_string());
    args.push(pixfmt.to_string());
    args.push("-fps_mode".to_string());
    args.push("vfr".to_string());
    args.push("-y".to_string());
    args.push(out_path.to_string());
    args
}

/// A fresh, server-chosen transient frame path under `dir` (PID + high-resolution clock + a
/// process-unique counter) — never agent input. The `screendump` loop writes each PNG here and
/// re-reads it; a single reused path is safe because each frame is fully read before the next write.
fn recording_frame_path(dir: &std::path::Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = RECORDING_SEQ.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("frame-{}-{nanos}-{seq}.png", std::process::id()))
}

/// Validate an agent-supplied recording file name (ADR-0017): the SAME allowlist as `serialSpool`
/// and the Image/ISO Store names — a single component `^[A-Za-z0-9][A-Za-z0-9._-]*` — so it can
/// never carry a slash, `..`, or an absolute path and escape the operator's `QMP_MCP_RECORDING_DIR`.
fn validate_recording_name(name: &str) -> Result<(), LifecycleError> {
    let mut chars = name.chars();
    let ok = matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(LifecycleError(format!(
            "Recording name \"{name}\" must match ^[A-Za-z0-9][A-Za-z0-9._-]* — a single file name \
             under QMP_MCP_RECORDING_DIR with no slash, '..', or leading dot (never a host path)."
        )))
    }
}

/// Resolve the recording's host output path: `<recording_dir>/<name>.mkv` for a validated
/// agent-supplied name, else a generated `<recording_dir>/recording-<pid>-<nanos>-<seq>.mkv`
/// (ADR-0017). The name is validated like a Store name, so it can never escape the operator root.
fn resolve_recording_path(
    recording_dir: Option<&str>,
    name: Option<&str>,
) -> Result<String, LifecycleError> {
    let root =
        match recording_dir {
            Some(d) if !d.trim().is_empty() => d,
            _ => return Err(LifecycleError(
                "Display recording requires QMP_MCP_RECORDING_DIR — the host directory recordings \
                 are written under."
                    .to_string(),
            )),
        };
    let file = match name {
        Some(n) => {
            validate_recording_name(n)?;
            format!("{n}.{RECORDING_EXTENSION}")
        }
        None => {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let seq = RECORDING_SEQ.fetch_add(1, Ordering::Relaxed);
            format!(
                "recording-{}-{nanos}-{seq}.{RECORDING_EXTENSION}",
                std::process::id()
            )
        }
    };
    let mut path = PathBuf::from(root);
    path.push(file);
    Ok(path.to_string_lossy().into_owned())
}

/// The frame-loop task body (ADR-0017): at up to the target fps, capture the Guest display via QMP
/// `screendump` to the transient frame path, read the PNG back, and write it into ffmpeg's stdin
/// (`image2pipe` splits the concatenated PNGs). Breaks when the Instance's QMP round-trip fails
/// (qemu gone) or ffmpeg's stdin closes (encoder exited) — either way the recording is over.
async fn run_recording_loop(
    handle: Arc<dyn InstanceHandle>,
    stdin: Arc<Mutex<ChildStdin>>,
    frame_path: PathBuf,
    interval: Duration,
) {
    let filename = frame_path.to_string_lossy().into_owned();
    loop {
        let tick = Instant::now();
        // Capture one frame to the server-chosen path (mirrors `Orchestrator::screendump`).
        if handle
            .execute(
                "screendump",
                Some(serde_json::json!({ "filename": filename, "format": "png" })),
            )
            .await
            .is_err()
        {
            break;
        }
        // Read the PNG back and feed it to ffmpeg. A read error is a transient miss (skip the
        // frame); a write error means ffmpeg is gone (stop).
        if let Ok(bytes) = tokio::fs::read(&frame_path).await {
            let mut guard = stdin.lock().await;
            if guard.write_all(&bytes).await.is_err() || guard.flush().await.is_err() {
                break;
            }
        }
        // Pace to the target frame rate, accounting for the time this frame already took.
        let elapsed = tick.elapsed();
        if elapsed < interval {
            sleep(interval - elapsed).await;
        }
    }
}

/// Fully stop a [`Recording`] (ADR-0017) and report the result: abort the frame loop and join it
/// (releasing its stdin clone), close ffmpeg's stdin so it flushes and finalizes the container,
/// await the child (bounded — killed if it overruns), remove the transient frame file, then stat
/// the output. Shared by `stop_recording` (returns the result) and teardown (discards it), so a
/// destroy / unexpected exit can never leak the ffmpeg child or a half-written file.
async fn finalize_recording(recording: Recording) -> StopRecordingResult {
    let Recording {
        mut child,
        stdin,
        loop_task,
        stderr_task,
        output_path,
        name,
        codec,
        frame_path,
        started_at,
    } = recording;
    // Stop the loop first so it issues no more `screendump`s, then join it so its stdin clone is
    // dropped — leaving the Recording's clone as the sole owner.
    loop_task.abort();
    let _ = loop_task.await;
    // Stop draining stderr — we are tearing the child down next.
    stderr_task.abort();
    let _ = stderr_task.await;
    let duration_ms = started_at.elapsed().as_millis() as u64;
    // Close ffmpeg's stdin (drop the last reference) → EOF → ffmpeg flushes and finalizes the file.
    drop(stdin);
    // Give ffmpeg a bounded window to finalize; kill it if it overruns so nothing leaks.
    if tokio::time::timeout(FFMPEG_FINALIZE_TIMEOUT, child.wait())
        .await
        .is_err()
    {
        let _ = child.start_kill();
        // R3: bound the post-kill wait too, so teardown never awaits unbounded. `kill_on_drop`
        // (R4) is the final backstop when `child` is dropped after this returns.
        let _ = tokio::time::timeout(FFMPEG_KILL_TIMEOUT, child.wait()).await;
    }
    // Remove the transient frame file (best-effort — the host is left clean).
    let _ = tokio::fs::remove_file(&frame_path).await;
    let size_bytes = tokio::fs::metadata(&output_path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    StopRecordingResult {
        path: output_path,
        name,
        codec,
        size_bytes,
        duration_ms,
    }
}

/// Standard (RFC 4648) base64-decode; the inverse of [`base64_encode`]. Whitespace/newlines
/// are ignored; invalid characters or trailing junk after padding are rejected so a bad
/// `write_serial` payload surfaces a clear error instead of typing garbage into the guest.
fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    fn sextet(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s
        .bytes()
        .filter(|&b| !matches!(b, b'\n' | b'\r' | b' ' | b'\t'))
        .collect();
    let body_len = bytes.iter().take_while(|&&b| b != b'=').count();
    if bytes[body_len..].iter().any(|&b| b != b'=') {
        return Err(
            "Invalid base64 in write_serial data: unexpected characters after padding.".to_string(),
        );
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut nbits = 0u32;
    for &c in &bytes[..body_len] {
        let v = sextet(c).ok_or_else(|| {
            format!("Invalid base64 in write_serial data (byte 0x{c:02x}). Use format: utf8 for raw text.")
        })?;
        acc = (acc << 6) | v as u32;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }
    // A base64 body length of 4n+1 leaves a dangling 6-bit sextet that completes no byte —
    // an invalid encoding. Reject it rather than silently dropping the guest's last input.
    if nbits >= 6 {
        return Err(
            "Invalid base64 in write_serial data: a dangling sextet (the input is not a valid \
             base64 length)."
                .to_string(),
        );
    }
    Ok(out)
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
            qemu_binary_override: Some("qemu-system-x86_64".to_string()),
            // Pin the host arch so the accel=auto guest/host match is deterministic
            // across CI runners (issue #18); the default q35 machine is x86_64.
            host_arch: "x86_64".to_string(),
            qmp_socket_path: "/run/qmp-mcp/qmp.sock".to_string(),
            image_dir: None,
            iso_dir: None,
            host_share_dir: None,
            guest_share_dir: None,
            share_readonly: None,
            serial_buffer_bytes: 1 << 20,
            allow_serial_write: false,
            serial_backend: SerialBackend::Ringbuf,
            serial_spool_dir: None,
            // Display recording OFF by default in tests (no ffmpeg, no output dir); the encoder
            // probe is a deterministic stub so no ffmpeg is ever spawned (ADR-0017).
            ffmpeg_binary: None,
            recording_dir: None,
            recording_codec: "libx264".to_string(),
            recording_crf: 22,
            recording_max_fps: 15,
            recording_pixfmt: "yuv420p".to_string(),
            recording_encoder_available: Box::new(|_| true),
            hostfwd_port_range: None,
            allow_host_net: false,
            auto_start: false,
            max_memory_mb: None,
            max_vcpus: None,
            allow_raw_args: false,
            command_policy: None,
            event_buffer_size: None,
            viewer_password: None,
            viewer_user: None,
            viewer_host: "127.0.0.1".to_string(),
            viewer_port: 6080,
            start_viewer: None,
            kvm_available: Box::new(|| false),
        }
    }

    fn orchestrator_with(driver: FakeQemuDriver) -> Orchestrator {
        Orchestrator::new(Box::new(driver), test_options())
    }

    #[test]
    fn describe_share_reports_tag_and_mount_command() {
        let mut opts = test_options();
        opts.host_share_dir = Some("/srv/share".to_string());
        opts.guest_share_dir = Some("/mnt/share".to_string());
        opts.share_readonly = Some(true);
        let orch = Orchestrator::new(Box::new(FakeQemuDriver::new()), opts);
        let s = orch.describe_share();
        assert!(s.available);
        assert_eq!(s.mount_tag, "share");
        assert_eq!(s.guest_mountpoint.as_deref(), Some("/mnt/share"));
        assert!(s.read_only);
        assert_eq!(
            s.mount_command.as_deref(),
            Some("mount -t 9p -o trans=virtio,version=9p2000.L,ro share /mnt/share")
        );

        // Unavailable with no host share configured.
        let off = Orchestrator::new(Box::new(FakeQemuDriver::new()), test_options());
        let s = off.describe_share();
        assert!(!s.available);
        assert!(s.mount_command.is_none());
        // The None fields are OMITTED from the JSON (skip_serializing_if), matching the
        // TS get_share which drops undefined keys — no `guestMountpoint: null` divergence.
        let json = serde_json::to_value(&s).unwrap();
        assert!(json.get("guestMountpoint").is_none());
        assert!(json.get("mountCommand").is_none());

        // Writable share drops the ,ro option.
        let mut rw = test_options();
        rw.host_share_dir = Some("/srv/s".to_string());
        rw.share_readonly = Some(false);
        let orch = Orchestrator::new(Box::new(FakeQemuDriver::new()), rw);
        let s = orch.describe_share();
        assert!(!s.read_only);
        assert!(!s.mount_command.unwrap().contains(",ro"));
    }

    /// An Orchestrator with auto-start explicit-on, so `create_instance` lands in
    /// RUNNING — the starting point for tests that exercise pause/resume/reset on a
    /// running Guest. (The `test_options` scaffolding pins auto-start OFF for a
    /// deterministic frozen Guest; production defaults it ON — ADR-0016.)
    fn orchestrator_autostart(driver: FakeQemuDriver) -> Orchestrator {
        let mut options = test_options();
        options.auto_start = true;
        Orchestrator::new(Box::new(driver), options)
    }

    #[tokio::test]
    async fn create_does_not_cont_when_auto_start_off_guest_stays_paused() {
        // issue #8: with QMP_MCP_AUTO_START=false, create must NOT issue `cont` — the
        // Guest stays frozen at the `-S` startup pause until resume_instance (ADR-0016
        // opt-out). `test_options` pins auto-start off.
        let driver = FakeQemuDriver::new();
        let commands = driver.commands();
        let mut orch = orchestrator_with(driver);
        orch.create_instance(json!({})).await.expect("create");
        assert!(!commands.lock().unwrap().iter().any(|c| c == "cont"));
    }

    #[tokio::test]
    async fn create_auto_starts_with_cont_when_enabled() {
        // issue #8: with auto-start (the default — ADR-0016), create issues `cont` after launch.
        let driver = FakeQemuDriver::new();
        let commands = driver.commands();
        let mut options = test_options();
        options.auto_start = true;
        let mut orch = Orchestrator::new(Box::new(driver), options);
        let result = orch.create_instance(json!({})).await.expect("create");
        assert_eq!(result.state, InstanceState::Running);
        assert!(commands.lock().unwrap().iter().any(|c| c == "cont"));
    }

    #[tokio::test]
    async fn write_serial_gated_by_allow_serial_write_and_read_serial_drains() {
        // ADR-0015: write is refused by default (read-only), naming the env var, no ringbuf-write.
        let driver = FakeQemuDriver::new();
        let commands = driver.commands();
        let mut orch = orchestrator_with(driver);
        orch.create_instance(json!({ "serial": true }))
            .await
            .expect("create");
        assert!(!orch.describe_serial().writable);
        let e = orch
            .write_serial("id\n".to_string(), SerialFormat::Utf8)
            .await
            .unwrap_err();
        assert!(e.0.contains("QMP_MCP_ALLOW_SERIAL_WRITE"));
        assert!(!commands
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "ringbuf-write"));
        // read_serial drains the ring buffer (ringbuf-read).
        let r = orch
            .read_serial(None, SerialFormat::Utf8)
            .await
            .expect("read");
        assert_eq!(r.output, "fake-serial-output");

        // With QMP_MCP_ALLOW_SERIAL_WRITE the write is issued (ringbuf-write).
        let driver = FakeQemuDriver::new();
        let commands = driver.commands();
        let mut options = test_options();
        options.allow_serial_write = true;
        let mut orch = Orchestrator::new(Box::new(driver), options);
        orch.create_instance(json!({ "serial": true }))
            .await
            .expect("create");
        assert!(orch.describe_serial().writable);
        let w = orch
            .write_serial("id\n".to_string(), SerialFormat::Utf8)
            .await
            .expect("write");
        assert_eq!(w.bytes_written, 3);
        assert!(commands
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "ringbuf-write"));
    }

    #[test]
    fn describe_serial_reports_spool_backend_tail_and_not_writable() {
        // ADR-0015: the spool backend tails a persistent file and is never writable (output-only),
        // even with the write gate on.
        let mut options = test_options();
        options.serial_backend = SerialBackend::Spool;
        options.serial_spool_dir = Some("/var/log/qmp-serial".to_string());
        options.allow_serial_write = true;
        let orch = Orchestrator::new(Box::new(FakeQemuDriver::new()), options);
        let s = orch.describe_serial();
        assert_eq!(s.backend, "spool");
        assert_eq!(s.read_semantics, "tail");
        assert!(!s.writable);
    }

    use crate::viewer::ViewerError;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    /// A fake noVNC Viewer factory: records the start/stop counts and the options it
    /// was wired with, so the vnc lifecycle is exercisable without binding a real port.
    struct FakeViewerFactory {
        starts: Arc<AtomicUsize>,
        stops: Arc<AtomicUsize>,
        last_options: Arc<std::sync::Mutex<Option<ViewerOptions>>>,
        fail: bool,
    }

    /// The handle the fake factory hands back; `stop` bumps the shared stop counter.
    struct FakeViewer {
        stops: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ViewerHandle for FakeViewer {
        async fn stop(&self) {
            self.stops.fetch_add(1, SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl ViewerFactory for FakeViewerFactory {
        async fn start(
            &self,
            options: ViewerOptions,
        ) -> Result<Box<dyn ViewerHandle>, ViewerError> {
            if self.fail {
                return Err(ViewerError("simulated Viewer bind failure".to_string()));
            }
            *self.last_options.lock().unwrap() = Some(options);
            self.starts.fetch_add(1, SeqCst);
            Ok(Box::new(FakeViewer {
                stops: Arc::clone(&self.stops),
            }))
        }
    }

    /// Options wired with a fake Viewer factory plus a configured Viewer password, so a
    /// `display: vnc` create can complete without a real port.
    fn options_with_viewer(
        factory: FakeViewerFactory,
        viewer_password: Option<&str>,
    ) -> OrchestratorOptions {
        let mut options = test_options();
        options.viewer_password = viewer_password.map(str::to_string);
        options.start_viewer = Some(Box::new(factory));
        options
    }

    #[tokio::test]
    async fn vnc_display_without_viewer_password_is_rejected_before_any_launch() {
        // Fail-closed (ADR-0010): a vnc Display with QMP_MCP_VIEWER_PASSWORD unset is
        // refused BEFORE qemu is spawned, with an actionable message naming the variable.
        let driver = FakeQemuDriver::new();
        let launches = driver.launches();
        let mut orch = orchestrator_with(driver); // test_options → viewer_password None
        let err = orch
            .create_instance(json!({ "display": "vnc" }))
            .await
            .unwrap_err();
        assert!(err.0.contains("QMP_MCP_VIEWER_PASSWORD"), "got: {}", err.0);
        assert!(err.0.contains("display \"vnc\""), "got: {}", err.0);
        assert_eq!(orch.state(), InstanceState::None);
        assert_eq!(
            launches.lock().unwrap().len(),
            0,
            "no qemu is launched when the Viewer is unconfigured"
        );
    }

    #[tokio::test]
    async fn vnc_display_arms_the_password_starts_the_viewer_and_destroy_stops_it() {
        let starts = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        let last = Arc::new(std::sync::Mutex::new(None));
        let factory = FakeViewerFactory {
            starts: Arc::clone(&starts),
            stops: Arc::clone(&stops),
            last_options: Arc::clone(&last),
            fail: false,
        };
        let mut orch = Orchestrator::new(
            Box::new(FakeQemuDriver::new()),
            options_with_viewer(factory, Some("view-secret")),
        );

        orch.create_instance(json!({ "display": "vnc" }))
            .await
            .unwrap();
        assert_eq!(
            starts.load(SeqCst),
            1,
            "the Viewer must start for a vnc Display"
        );

        // The Viewer is wired to the human-facing gate and the server-fixed loopback
        // VNC endpoint, with a fresh 8-char armed password (never in argv).
        let opts = last.lock().unwrap().clone().unwrap();
        assert_eq!(opts.password, "view-secret");
        assert_eq!(opts.vnc_host, "127.0.0.1");
        assert_eq!(opts.vnc_port, 5900);
        assert_eq!(opts.vnc_password.len(), 8);

        orch.destroy_instance().await.unwrap();
        assert_eq!(stops.load(SeqCst), 1, "destroy must stop the Viewer");
        assert_eq!(orch.state(), InstanceState::None);
    }

    #[tokio::test]
    async fn a_headless_display_never_starts_the_viewer() {
        let starts = Arc::new(AtomicUsize::new(0));
        let factory = FakeViewerFactory {
            starts: Arc::clone(&starts),
            stops: Arc::new(AtomicUsize::new(0)),
            last_options: Arc::new(std::sync::Mutex::new(None)),
            fail: false,
        };
        let mut orch = Orchestrator::new(
            Box::new(FakeQemuDriver::new()),
            options_with_viewer(factory, Some("view-secret")),
        );
        // Default display is `none`: the Viewer surface never comes up.
        orch.create_instance(json!({})).await.unwrap();
        assert_eq!(
            starts.load(SeqCst),
            0,
            "a headless Instance starts no Viewer"
        );
    }

    #[tokio::test]
    async fn viewer_start_failure_tears_down_and_leaves_none() {
        let factory = FakeViewerFactory {
            starts: Arc::new(AtomicUsize::new(0)),
            stops: Arc::new(AtomicUsize::new(0)),
            last_options: Arc::new(std::sync::Mutex::new(None)),
            fail: true,
        };
        let mut orch = Orchestrator::new(
            Box::new(FakeQemuDriver::new()),
            options_with_viewer(factory, Some("view-secret")),
        );
        let err = orch
            .create_instance(json!({ "display": "vnc" }))
            .await
            .unwrap_err();
        assert!(
            err.0.contains("Failed to start the noVNC Viewer"),
            "got: {}",
            err.0
        );
        // The launched qemu was torn down and the slot released back to NONE.
        assert_eq!(orch.state(), InstanceState::None);
    }

    /// Poll until the shared Orchestrator reaches `state`, or panic. The exit-watch task
    /// reconciles on a background task, so the transition is observed with a bounded poll
    /// rather than assumed synchronous (never flaky: bounded to ~1s).
    async fn await_state(orch: &Arc<AsyncMutex<Orchestrator>>, state: InstanceState) {
        for _ in 0..1_000 {
            if orch.lock().await.state() == state {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("Orchestrator never reached state {state}");
    }

    #[tokio::test]
    async fn reconciles_to_none_on_unexpected_exit_and_allows_a_fresh_create() {
        // The issue #28 acceptance path: qemu exits WITHOUT destroy_instance; the
        // Orchestrator must reconcile to NONE, get_instance must reflect it, and a
        // subsequent create must be accepted (no phantom "already running").
        let driver = FakeQemuDriver::new();
        let exit_slot = driver.exit_slot();
        let launches = driver.launches();
        let orch = Orchestrator::new_shared(Box::new(driver), test_options());

        orch.lock().await.create_instance(json!({})).await.unwrap();
        assert_eq!(orch.lock().await.state(), InstanceState::Paused);

        // qemu vanishes on its own (crash/SIGKILL) — the exit signal fires without an
        // explicit destroy. Release the lock first so the watcher can reconcile.
        let _ = exit_slot
            .lock()
            .unwrap()
            .clone()
            .expect("create installs an exit signal")
            .send(true);

        await_state(&orch, InstanceState::None).await;
        {
            let guard = orch.lock().await;
            assert_eq!(guard.get_instance().state, InstanceState::None);
            assert!(guard.get_instance().spec.is_none());
            assert!(guard.get_instance().accel.is_none());
        }

        // The crashed Instance's handle was released, so a fresh create is accepted.
        orch.lock()
            .await
            .create_instance(json!({}))
            .await
            .expect("a fresh create is accepted after an unexpected exit");
        assert_eq!(orch.lock().await.state(), InstanceState::Paused);
        assert_eq!(
            launches.lock().unwrap().len(),
            2,
            "a second qemu was launched after the crash"
        );
    }

    #[tokio::test]
    async fn unexpected_exit_stops_the_viewer() {
        // Parity with the TS "stops the Viewer when qemu exits unexpectedly": the Viewer
        // lifetime equals the Instance's, so an unexpected exit stops it too (ADR-0010).
        let stops = Arc::new(AtomicUsize::new(0));
        let factory = FakeViewerFactory {
            starts: Arc::new(AtomicUsize::new(0)),
            stops: Arc::clone(&stops),
            last_options: Arc::new(std::sync::Mutex::new(None)),
            fail: false,
        };
        let driver = FakeQemuDriver::new();
        let exit_slot = driver.exit_slot();
        let orch = Orchestrator::new_shared(
            Box::new(driver),
            options_with_viewer(factory, Some("view-secret")),
        );
        orch.lock()
            .await
            .create_instance(json!({ "display": "vnc" }))
            .await
            .unwrap();

        let _ = exit_slot.lock().unwrap().clone().unwrap().send(true);
        await_state(&orch, InstanceState::None).await;
        assert_eq!(
            stops.load(SeqCst),
            1,
            "an unexpected exit must stop the Viewer"
        );
    }

    #[tokio::test]
    async fn a_stale_generation_watcher_never_clobbers_a_newer_instance() {
        // The generation guard: reconcile_unexpected_exit only fires for the Instance it
        // was armed for. A reconcile for any OTHER generation is inert.
        let mut orch = orchestrator_with(FakeQemuDriver::new());
        orch.create_instance(json!({})).await.unwrap();
        let gen1 = orch.current_generation();

        // A reconcile for an OLDER generation leaves the current Instance untouched.
        orch.reconcile_unexpected_exit(gen1 - 1).await;
        assert_eq!(orch.state(), InstanceState::Paused);

        // Destroy + recreate: the new Instance carries a NEWER generation.
        orch.destroy_instance().await.unwrap();
        orch.create_instance(json!({})).await.unwrap();
        assert_eq!(orch.current_generation(), gen1 + 1);

        // The stale watcher for the destroyed Instance (generation gen1) must NOT tear
        // down the newer one.
        orch.reconcile_unexpected_exit(gen1).await;
        assert_eq!(orch.state(), InstanceState::Paused);

        // The CURRENT generation does reconcile to NONE.
        let current = orch.current_generation();
        orch.reconcile_unexpected_exit(current).await;
        assert_eq!(orch.state(), InstanceState::None);
    }

    #[test]
    fn generate_vnc_password_is_eight_copy_safe_chars() {
        const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789";
        for _ in 0..64 {
            let password = generate_vnc_password();
            assert_eq!(password.len(), 8, "VNC password must be 8 chars");
            assert!(
                password.bytes().all(|b| ALPHABET.contains(&b)),
                "unexpected glyph in {password}"
            );
        }
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
    async fn create_loads_instance_paused_then_destroy_returns_to_none() {
        let mut orch = orchestrator_with(FakeQemuDriver::new());
        assert_eq!(orch.state(), InstanceState::None);
        assert_eq!(orch.get_instance().state, InstanceState::None);

        let result = orch.create_instance(json!({})).await.expect("create ok");
        // Default (no auto-start): loaded but frozen at -S, so the honest state is
        // PAUSED — and get_status agrees (query-status not running) — issue #10.
        assert_eq!(result.state, InstanceState::Paused);
        assert_eq!(result.accel, Accel::Tcg); // kvm_available == false → TCG
        assert_eq!(orch.get_status().await.unwrap()["running"], false);

        let view = orch.get_instance();
        assert_eq!(view.state, InstanceState::Paused);
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
    async fn derives_qemu_binary_from_machine_when_no_override() {
        // issue #18: with no QMP_MCP_QEMU_BINARY override, a virt machine picks the
        // aarch64 emulator without any env flip.
        let driver = FakeQemuDriver::new();
        let launches = driver.launches();
        let mut options = test_options();
        options.qemu_binary_override = None;
        let mut orch = Orchestrator::new(Box::new(driver), options);
        orch.create_instance(json!({ "machine": "virt", "accel": "tcg" }))
            .await
            .expect("create ok");
        assert_eq!(launches.lock().unwrap()[0].binary, "qemu-system-aarch64");
    }

    #[tokio::test]
    async fn explicit_binary_override_wins_over_machine_derivation() {
        // test_options sets the override to x86_64; a virt (aarch64) machine must NOT
        // flip the binary — the operator's explicit choice is honored (issue #18).
        let driver = FakeQemuDriver::new();
        let launches = driver.launches();
        let mut orch = orchestrator_with(driver);
        orch.create_instance(json!({ "machine": "virt", "accel": "tcg" }))
            .await
            .expect("create ok");
        assert_eq!(launches.lock().unwrap()[0].binary, "qemu-system-x86_64");
    }

    #[tokio::test]
    async fn accel_auto_falls_back_to_tcg_on_arch_mismatch() {
        // aarch64 `virt` guest on an x86_64 host (test_options host_arch): even with
        // /dev/kvm available, KVM cannot cross architectures, so accel resolves to TCG
        // (issue #18) rather than the qemu "invalid accelerator kvm" failure.
        let driver = FakeQemuDriver::new();
        let mut options = test_options();
        options.qemu_binary_override = None;
        options.kvm_available = Box::new(|| true);
        let mut orch = Orchestrator::new(Box::new(driver), options);
        let result = orch
            .create_instance(json!({ "machine": "virt" }))
            .await
            .expect("create ok");
        assert_eq!(result.accel, Accel::Tcg);
        assert!(result.accel_reason.contains("does not match host arch"));
    }

    #[tokio::test]
    async fn accel_auto_keys_off_the_override_binary_arch_not_the_machine() {
        // machine q35 -> x86_64 (== host), but the operator overrode the binary to an
        // aarch64 emulator: KVM must NOT be chosen — the launched binary is aarch64 (#18).
        let driver = FakeQemuDriver::new();
        let mut options = test_options();
        options.qemu_binary_override = Some("qemu-system-aarch64".to_string());
        options.kvm_available = Box::new(|| true);
        let mut orch = Orchestrator::new(Box::new(driver), options);
        let result = orch
            .create_instance(json!({ "machine": "q35" }))
            .await
            .expect("create ok");
        assert_eq!(result.accel, Accel::Tcg);
        assert!(result
            .accel_reason
            .contains("guest arch aarch64 does not match host arch x86_64"));
    }

    #[tokio::test]
    async fn create_while_running_is_rejected_with_actionable_message() {
        let mut orch = orchestrator_with(FakeQemuDriver::new());
        orch.create_instance(json!({})).await.unwrap();

        let err = orch.create_instance(json!({})).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("An Instance already exists"), "got: {msg}");
        assert!(msg.contains("state PAUSED"), "got: {msg}");
        assert!(msg.contains("destroy_instance"), "got: {msg}");
        // Still exactly one Instance (PAUSED — auto-start off in these tests; issue #10).
        assert_eq!(orch.state(), InstanceState::Paused);
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
        // Auto-start so the Guest begins RUNNING; then exercise pause/resume.
        let mut orch = orchestrator_autostart(FakeQemuDriver::new());
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
        // Auto-start so the Guest is RUNNING; reset/powerdown must leave it RUNNING.
        let mut orch = orchestrator_autostart(FakeQemuDriver::new());
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
        let mut orch = orchestrator_autostart(FakeQemuDriver::new());
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
                    assert_eq!(result.state, InstanceState::Paused);
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
        assert_eq!(orch.lock().await.state(), InstanceState::Paused);
    }

    #[test]
    fn ring_push_bounded_evicts_oldest_beyond_cap() {
        // The socket ring keeps only the newest `cap` bytes — overwrite-oldest, like ringbuf.
        let mut ring = VecDeque::new();
        ring_push_bounded(&mut ring, b"abcdef", 4);
        assert_eq!(ring.iter().copied().collect::<Vec<u8>>(), b"cdef");
        ring_push_bounded(&mut ring, b"XY", 4);
        assert_eq!(ring.iter().copied().collect::<Vec<u8>>(), b"efXY");
        // A single push larger than cap keeps only the last cap bytes.
        let mut ring = VecDeque::new();
        ring_push_bounded(&mut ring, b"0123456789", 3);
        assert_eq!(ring.iter().copied().collect::<Vec<u8>>(), b"789");
    }

    #[test]
    fn base64_decode_roundtrips_and_rejects_garbage() {
        assert_eq!(base64_decode("Aw==").unwrap(), vec![0x03]); // Ctrl-C
        assert_eq!(base64_decode("aGk=").unwrap(), b"hi");
        assert_eq!(base64_decode("").unwrap(), Vec::<u8>::new());
        // Round-trips against the encoder over non-UTF8 bytes.
        let data = b"\x00\x01\x02hello\xff";
        assert_eq!(base64_decode(&base64_encode(data)).unwrap(), data);
        // Invalid alphabet and trailing junk after padding are rejected.
        assert!(base64_decode("!!!!").is_err());
        assert!(base64_decode("aa=a").is_err());
        // A dangling sextet (4n+1 body) is invalid — reject, don't silently truncate.
        assert!(base64_decode("a").is_err());
        assert!(base64_decode("YWJja").is_err());
        // Valid non-padded lengths (4n+2, 4n+3) still decode.
        assert_eq!(base64_decode("YWI").unwrap(), b"ab");
        assert_eq!(base64_decode("YWJj").unwrap(), b"abc");
    }

    #[tokio::test]
    async fn serial_socket_bridge_reads_ring_and_writes_socket() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::UnixListener;

        // Stand in for QEMU: a UNIX listener at the serial.sock sibling of a temp QMP path.
        let dir = std::env::temp_dir().join(format!("qmp-mcp-serialbridge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let qmp_path = dir.join("qmp.sock").to_string_lossy().into_owned();
        let serial_path = serial_socket_path(&qmp_path);
        let listener = UnixListener::bind(&serial_path).unwrap();
        let accept = tokio::spawn(async move { listener.accept().await.unwrap().0 });

        let bridge = attach_serial_bridge(&qmp_path, 1 << 20).await.unwrap();
        let mut server = accept.await.unwrap();

        // Guest→server: bytes the "guest" emits are drained into the bridge ring (read path).
        server.write_all(b"boot log\n").await.unwrap();
        server.flush().await.unwrap();
        let mut got = Vec::new();
        for _ in 0..100 {
            {
                let ring = bridge.ring.lock().unwrap();
                if !ring.is_empty() {
                    got = ring.iter().copied().collect();
                    break;
                }
            }
            sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(got, b"boot log\n");

        // Server→guest: bytes written through the bridge arrive on the socket (write path).
        {
            let mut w = bridge.write.lock().await;
            w.write_all(b"id\n").await.unwrap();
            w.flush().await.unwrap();
        }
        let mut buf = [0u8; 3];
        server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"id\n");

        // Teardown stops the reader and unlinks the socket file.
        teardown_serial_bridge(bridge).await;
        assert!(!std::path::Path::new(&serial_path).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_ffmpeg_args_wires_wallclock_vfr_and_tunes_only_libx264() {
        // ADR-0017: libx264 gets the screen-content `-tune stillimage`; the frame source is stdin
        // via image2pipe with wallclock timestamps + VFR (no input -framerate), and CRF/pixfmt/-y
        // are threaded through in order.
        let args = build_ffmpeg_args("libx264", 22, 15, "yuv420p", "/out/rec.mkv");
        assert_eq!(
            args,
            vec![
                "-f",
                "image2pipe",
                "-use_wallclock_as_timestamps",
                "1",
                "-i",
                "pipe:0",
                "-c:v",
                "libx264",
                "-tune",
                "stillimage",
                "-crf",
                "22",
                "-pix_fmt",
                "yuv420p",
                "-fps_mode",
                "vfr",
                "-y",
                "/out/rec.mkv",
            ]
        );

        // A non-libx264 codec omits `-tune stillimage` but keeps everything else.
        let args = build_ffmpeg_args("libx265", 28, 30, "yuv444p", "/out/rec.mkv");
        assert!(!args.iter().any(|a| a == "stillimage"));
        assert_eq!(
            args,
            vec![
                "-f",
                "image2pipe",
                "-use_wallclock_as_timestamps",
                "1",
                "-i",
                "pipe:0",
                "-c:v",
                "libx265",
                "-crf",
                "28",
                "-pix_fmt",
                "yuv444p",
                "-fps_mode",
                "vfr",
                "-y",
                "/out/rec.mkv",
            ]
        );

        // max_fps never leaks into the argv (wallclock + VFR make it a pacing cap only); CRF 0 is
        // accepted by the builder (range is enforced at config load, not here).
        let a = build_ffmpeg_args("libx264", 0, 5, "yuv420p", "/out/x.mkv");
        let b = build_ffmpeg_args("libx264", 0, 60, "yuv420p", "/out/x.mkv");
        assert_eq!(a, b);
        assert!(!a.iter().any(|arg| arg == "-framerate"));
    }

    #[test]
    fn recording_capability_reason_precedence_and_availability() {
        // No ffmpeg → unavailable, naming QMP_MCP_FFMPEG_BINARY (checked before the dir/codec).
        let (available, reason) = recording_capability(None, Some("/out"), "libx264", || true);
        assert!(!available);
        assert!(reason.contains("QMP_MCP_FFMPEG_BINARY"), "got: {reason}");

        // ffmpeg present but no output dir → names QMP_MCP_RECORDING_DIR.
        let (available, reason) =
            recording_capability(Some("/usr/bin/ffmpeg"), None, "libx264", || true);
        assert!(!available);
        assert!(reason.contains("QMP_MCP_RECORDING_DIR"), "got: {reason}");

        // ffmpeg + dir present but the codec is absent → names the codec + QMP_MCP_RECORDING_CODEC.
        let (available, reason) =
            recording_capability(Some("/usr/bin/ffmpeg"), Some("/out"), "libx265", || false);
        assert!(!available);
        assert!(reason.contains("libx265"), "got: {reason}");
        assert!(reason.contains("QMP_MCP_RECORDING_CODEC"), "got: {reason}");

        // All three satisfied → available.
        let (available, reason) =
            recording_capability(Some("/usr/bin/ffmpeg"), Some("/out"), "libx264", || true);
        assert!(available);
        assert!(reason.contains("available"), "got: {reason}");
    }

    #[test]
    fn recording_capability_does_not_probe_encoder_until_ffmpeg_and_dir_present() {
        // The encoder probe (a spawn in production) must not run when ffmpeg/dir are missing.
        let probed = Arc::new(AtomicUsize::new(0));
        let p = Arc::clone(&probed);
        let _ = recording_capability(None, None, "libx264", || {
            p.fetch_add(1, SeqCst);
            true
        });
        assert_eq!(
            probed.load(SeqCst),
            0,
            "encoder probe must be short-circuited"
        );
    }

    #[test]
    fn resolve_recording_path_validates_names_and_generates_unique_ones() {
        // A valid name resolves under the root with the .mkv container.
        let p = resolve_recording_path(Some("/out"), Some("demo")).unwrap();
        assert_eq!(p, "/out/demo.mkv");

        // A traversing / absolute / dotted name is refused (same allowlist as serialSpool).
        for bad in ["../escape", "a/b", "/abs", ".hidden", "has space"] {
            let err = resolve_recording_path(Some("/out"), Some(bad)).unwrap_err();
            assert!(
                err.0.contains("QMP_MCP_RECORDING_DIR"),
                "name {bad:?} gave {:?}",
                err.0
            );
        }

        // No name → a unique generated file under the root; two calls differ.
        let a = resolve_recording_path(Some("/out"), None).unwrap();
        let b = resolve_recording_path(Some("/out"), None).unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with("/out/recording-") && a.ends_with(".mkv"));

        // No output dir → fails closed naming the variable.
        assert!(resolve_recording_path(None, Some("demo"))
            .unwrap_err()
            .0
            .contains("QMP_MCP_RECORDING_DIR"));
    }

    /// Options with recording fully configured (ffmpeg + dir + a stubbed encoder probe), so the
    /// capability is available without a real ffmpeg.
    fn options_with_recording(encoder_present: bool) -> OrchestratorOptions {
        let mut options = test_options();
        options.ffmpeg_binary = Some("/usr/bin/ffmpeg".to_string());
        options.recording_dir = Some("/tmp/qmp-mcp-recordings-test".to_string());
        options.recording_encoder_available = Box::new(move |_| encoder_present);
        options
    }

    #[test]
    fn describe_recording_reports_availability_and_config() {
        // Unavailable when unconfigured (test_options: no ffmpeg / no dir).
        let orch = orchestrator_with(FakeQemuDriver::new());
        let r = orch.describe_recording();
        assert!(!r.available);
        assert!(!r.active);
        assert_eq!(r.codec, "libx264");
        assert_eq!(r.crf, 22);
        assert_eq!(r.max_fps, 15);
        assert_eq!(r.pixfmt, "yuv420p");
        assert!(r.reason.contains("QMP_MCP_FFMPEG_BINARY"));
        // Converged shape (ADR-0012): outputDir + activePath are ALWAYS present (null when unset),
        // matching the TS report which emits null rather than omitting the keys.
        let json = serde_json::to_value(&r).unwrap();
        assert!(json.get("outputDir").is_some_and(|v| v.is_null()));
        assert!(json.get("activePath").is_some_and(|v| v.is_null()));
        // Exactly the converged key set.
        let keys: std::collections::BTreeSet<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "available",
                "reason",
                "codec",
                "crf",
                "maxFps",
                "pixfmt",
                "active",
                "activePath",
                "outputDir",
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<&str>>()
        );

        // Available when ffmpeg + dir + codec present.
        let orch = Orchestrator::new(
            Box::new(FakeQemuDriver::new()),
            options_with_recording(true),
        );
        let r = orch.describe_recording();
        assert!(r.available, "got reason: {}", r.reason);
        assert!(!r.active);
        assert_eq!(r.active_path, None);
        assert_eq!(
            r.output_dir.as_deref(),
            Some("/tmp/qmp-mcp-recordings-test")
        );

        // Configured but the selected codec is absent from the build → unavailable, naming it.
        let orch = Orchestrator::new(
            Box::new(FakeQemuDriver::new()),
            options_with_recording(false),
        );
        let r = orch.describe_recording();
        assert!(!r.available);
        assert!(r.reason.contains("libx264"));
    }

    #[tokio::test]
    async fn start_recording_requires_a_running_instance_then_a_display_adapter() {
        // No Instance → refused (never touches ffmpeg).
        let mut orch = Orchestrator::new(
            Box::new(FakeQemuDriver::new()),
            options_with_recording(true),
        );
        assert!(orch
            .start_recording(None)
            .await
            .unwrap_err()
            .0
            .contains("start a display recording"));

        // A headless Instance (displayDevice: none) → refused with the display-adapter message,
        // BEFORE the capability/ffmpeg spawn.
        orch.create_instance(json!({})).await.unwrap();
        let err = orch.start_recording(None).await.unwrap_err();
        assert!(err.0.contains("no display adapter"), "got: {}", err.0);
        assert!(err.0.contains("displayDevice"), "got: {}", err.0);
    }

    #[tokio::test]
    async fn start_recording_fails_closed_when_capability_unavailable() {
        // A displayed Instance but no ffmpeg configured → the capability check refuses it,
        // naming the variable, without spawning anything.
        let mut orch = orchestrator_with(FakeQemuDriver::new()); // no ffmpeg / no dir
        orch.create_instance(json!({ "displayDevice": "vga" }))
            .await
            .unwrap();
        let err = orch.start_recording(None).await.unwrap_err();
        assert!(err.0.contains("QMP_MCP_FFMPEG_BINARY"), "got: {}", err.0);
        assert!(orch.recording.is_none());
    }

    #[tokio::test]
    async fn start_recording_surfaces_an_immediate_ffmpeg_exit() {
        // R5: point the "ffmpeg" binary at /bin/false, which exits 1 immediately. The post-spawn
        // liveness check must catch it and surface an actionable error instead of leaving an
        // empty file + a dead encoder behind. Uses a real (tiny) spawn — no ffmpeg needed.
        let dir = std::env::temp_dir().join(format!("qmp-rec-r5-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut options = options_with_recording(true);
        options.ffmpeg_binary = Some("/bin/false".to_string());
        options.recording_dir = Some(dir.to_string_lossy().into_owned());
        let mut orch = Orchestrator::new(Box::new(FakeQemuDriver::new()), options);
        orch.create_instance(json!({ "displayDevice": "vga" }))
            .await
            .unwrap();
        let err = orch
            .start_recording(Some("dead".to_string()))
            .await
            .unwrap_err();
        assert!(err.0.contains("exited immediately"), "got: {}", err.0);
        // No recording is left installed, and the slot is free for a retry.
        assert!(orch.recording.is_none());
        assert!(!orch.describe_recording().active);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_recording_result_serializes_to_the_converged_shape() {
        // ADR-0012: exactly { path, name, codec, sizeBytes, durationMs } in camelCase.
        let json = serde_json::to_value(StopRecordingResult {
            path: "/rec/run1.mkv".to_string(),
            name: "run1.mkv".to_string(),
            codec: "libx264".to_string(),
            size_bytes: 4096,
            duration_ms: 1234,
        })
        .unwrap();
        let keys: std::collections::BTreeSet<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            ["path", "name", "codec", "sizeBytes", "durationMs"]
                .into_iter()
                .collect::<std::collections::BTreeSet<&str>>()
        );
        assert_eq!(json.get("durationMs").and_then(|v| v.as_u64()), Some(1234));

        // start result: exactly { path, name, codec }.
        let start = serde_json::to_value(StartRecordingResult {
            path: "/rec/run1.mkv".to_string(),
            name: "run1.mkv".to_string(),
            codec: "libx264".to_string(),
        })
        .unwrap();
        let start_keys: std::collections::BTreeSet<&str> = start
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            start_keys,
            ["path", "name", "codec"]
                .into_iter()
                .collect::<std::collections::BTreeSet<&str>>()
        );
    }

    #[test]
    fn parse_ffmpeg_encoders_extracts_names_after_the_separator() {
        // Mirrors the TS parseFfmpegEncoders corpus: names come from the second token of each
        // post-separator line whose first token is a 6-char [A-Z.] flags column.
        let output = "Encoders:\n V..... = Video\n ------\n \
             V....D libx264              libx264 H.264\n \
             V....D libx265              libx265 H.265\n \
             A....D aac                  AAC audio\n";
        let set = parse_ffmpeg_encoders(output);
        assert!(set.contains("libx264"));
        assert!(set.contains("libx265"));
        assert!(set.contains("aac"));
        assert_eq!(set.len(), 3);
        // No encoder table → empty.
        assert!(parse_ffmpeg_encoders("ffmpeg version 10\nunrelated\n").is_empty());
    }

    #[tokio::test]
    async fn attach_serial_bridge_times_out_when_nothing_listens() {
        // No listener at the derived path → the dial fails closed with an actionable message.
        let dir =
            std::env::temp_dir().join(format!("qmp-mcp-serialbridge-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let qmp_path = dir.join("qmp.sock").to_string_lossy().into_owned();
        let err = attach_serial_bridge(&qmp_path, 1 << 20).await.unwrap_err();
        assert!(err.0.contains("Serial Port socket"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
