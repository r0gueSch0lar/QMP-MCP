//! The Hardware Spec — the structured, validated description of an Instance's
//! hardware (see the root `CONTEXT.md` and ADR-0002). The agent never supplies raw
//! QEMU argv; it fills this spec and the server generates the `qemu-system-*` argv
//! from it via the pure [`build_argv`] function.
//!
//! This is a second implementation of the shared bounded context (ADR-0011),
//! mirroring `../../typescript/src/instance/hardware-spec.ts` **behaviorally**: the same
//! defaults, the same security validation, and — asserted by the shared golden
//! fixtures (ADR-0012, `../../testdata/argv/*.json`) — byte-for-byte the same argv.
//!
//! Design (per the slice-2 stance in `../../PLAN.md`):
//! - **Enums** model the closed sets (disk interface, network mode, disk format,
//!   accel, display, NIC model, protocol), so an out-of-set value cannot even be
//!   represented — serde rejects it while deserializing the tool input.
//! - **Newtypes** ([`MachineCpu`], [`BootOrder`]) wrap the free-string fields that
//!   pass a charset allowlist, so a validated value cannot be constructed without
//!   passing its constructor — an unvalidated machine/cpu/boot cannot reach argv.
//! - **Hand-rolled validation** (no validation-DSL crate): charset allowlists,
//!   comma-escaping against `-drive`/`-machine` option injection, resource caps,
//!   the bounded host-port-forward range, and the `extraArgs` gate.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::{PortRange, DEFAULT_HOSTFWD_PORT_RANGE};

use super::image_store::{resolve_image_path, ImageFormat};
use super::iso_store::resolve_iso_path;

// ---------------------------------------------------------------------------
// Closed sets (enums)
// ---------------------------------------------------------------------------

/// The requested accelerator. `auto` probes `/dev/kvm` and falls back to TCG;
/// `kvm` hard-fails when unavailable; `tcg` is always available (ADR-0008).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AccelMode {
    /// Probe `/dev/kvm`, use KVM when accessible, else fall back to TCG.
    #[default]
    Auto,
    /// Require `/dev/kvm`; fail closed when it is inaccessible.
    Kvm,
    /// Software emulation; always available.
    Tcg,
}

/// A concrete accelerator QEMU is actually launched with (the resolution of an
/// [`AccelMode`] via [`resolve_accel`]). Only this reaches argv.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Accel {
    /// Hardware acceleration via `/dev/kvm`.
    Kvm,
    /// TCG software emulation.
    Tcg,
}

impl Accel {
    /// Canonical spelling emitted into the `-machine accel=` property.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Kvm => "kvm",
            Self::Tcg => "tcg",
        }
    }
}

/// Guest Display modes (ADR-0010). `none` (the default) is a fully headless
/// Instance. `vnc` attaches a LOOPBACK-only VNC server the optional noVNC Viewer
/// bridges over; the password is never emitted into argv (`password=on` only
/// *requires* one, set later over QMP).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DisplayMode {
    /// No display backend at all — fully headless (default).
    #[default]
    None,
    /// A loopback-only VNC Display.
    Vnc,
}

/// Guest-visible disk controller a disk attaches through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DiskInterface {
    /// Paravirtual virtio-blk (default).
    #[default]
    Virtio,
    /// Emulated IDE.
    Ide,
    /// Emulated SCSI.
    Scsi,
    /// SD/MMC slot the `raspi*` boards boot from (`if=sd`). The image must be sized
    /// to a power of two or QEMU rejects it at launch.
    Sd,
}

impl DiskInterface {
    /// Canonical spelling emitted into the `-drive if=` property.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Virtio => "virtio",
            Self::Ide => "ide",
            Self::Scsi => "scsi",
            Self::Sd => "sd",
        }
    }
}

/// Strict allowlist of guest NIC models (ADR-0009). The model is emitted verbatim
/// into `-device <model>,netdev=...`, so it is a CLOSED enum, never a free string:
/// a free string could carry a comma to inject extra `-device` properties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
pub enum NicModel {
    /// Paravirtual virtio NIC (default). PCI.
    #[default]
    #[serde(rename = "virtio-net-pci")]
    VirtioNetPci,
    /// Widely emulated Intel e1000. PCI.
    #[serde(rename = "e1000")]
    E1000,
    /// Widely emulated Realtek RTL8139. PCI.
    #[serde(rename = "rtl8139")]
    Rtl8139,
    /// USB CDC/RNDIS NIC — for boards with a USB bus but no PCI (the `raspi*` boards).
    #[serde(rename = "usb-net")]
    UsbNet,
}

impl NicModel {
    /// Canonical spelling emitted into the `-device` model field.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VirtioNetPci => "virtio-net-pci",
            Self::E1000 => "e1000",
            Self::Rtl8139 => "rtl8139",
            Self::UsbNet => "usb-net",
        }
    }

    /// True when this NIC attaches to a PCI bus (everything except `usb-net`).
    pub fn is_pci(self) -> bool {
        !matches!(self, Self::UsbNet)
    }
}

/// Guest networking backend (ADR-0009). `user` is QEMU user-mode networking
/// (SLiRP): NAT'd outbound with the host network unexposed and inbound only via
/// explicit port-forwards — the safe, unprivileged default. `tap`/`bridge` put the
/// guest on the host LAN and need host privileges, so they are env-gated off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    /// User-mode (SLiRP) NAT (default).
    #[default]
    User,
    /// Host TAP device (requires `QMP_MCP_ALLOW_HOST_NET`).
    Tap,
    /// Host bridge (requires `QMP_MCP_ALLOW_HOST_NET`).
    Bridge,
    /// No NIC at all — the escape hatch for boards where no allowlisted NIC can
    /// attach (e.g. the `raspi*` machines have no PCI bus for the PCI models).
    None,
}

impl NetworkMode {
    /// Canonical spelling emitted into the `-netdev` backend field.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Tap => "tap",
            Self::Bridge => "bridge",
            Self::None => "none",
        }
    }
}

/// Transport protocol for a user-mode port-forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NetProtocol {
    /// TCP (default).
    #[default]
    Tcp,
    /// UDP.
    Udp,
}

impl NetProtocol {
    /// Canonical spelling emitted into the `hostfwd=` value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

// ---------------------------------------------------------------------------
// Validation charsets & messages (mirror the TS regex sources + wording exactly)
// ---------------------------------------------------------------------------

/// The `-machine`/`-cpu` model-name charset message. The charset is a leading
/// alphanumeric then alphanumerics, dot, underscore, plus, or hyphen — excluding
/// the comma, space, and `=` that QEMU treats as QemuOpts separators.
const MACHINE_CPU_MESSAGE: &str =
    "must match ^[A-Za-z0-9][A-Za-z0-9._+-]* — letters, digits, dot, \
     underscore, plus, or hyphen, with no leading hyphen and no comma, space, or '=' (these could \
     inject QEMU -machine/-cpu properties).";

const BOOT_ORDER_MESSAGE: &str = "boot must match ^[a-dnp]+ — one or more QEMU boot drive letters \
     (a/b floppy, c disk, d cd-rom, n-p network), with no comma, '=', or spaces (these could inject \
     extra -boot options).";

/// True when `s` matches the machine/cpu charset (hand-rolled; no regex crate).
fn is_valid_machine_cpu(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
}

/// True when `s` is one or more of the QEMU boot drive letters `a`,`b`,`c`,`d`,`n`,`p`
/// (the `^[a-dnp]+$` allowlist).
fn is_valid_boot_order(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| matches!(c, 'a'..='d' | 'n' | 'p'))
}

/// QEMU Raspberry Pi machine types. These are Single-Board-Computer boards with
/// FIXED hardware — CPU model, core count and RAM are baked into the machine — so
/// [`build_argv`] OMITS `-cpu`/`-smp`/`-m` for them (QEMU rejects those, e.g.
/// "Invalid RAM size, should be 1024 MB"). They also do not boot the SD card's
/// firmware, so they must be direct-kernel-booted (`kernel`/`dtb`). `raspi4b` needs
/// QEMU >= 9.2; the rest are in QEMU >= 7.2. Mirrors the TS `RASPI_MACHINES`.
const RASPI_MACHINES: [&str; 6] = [
    "raspi0", "raspi1ap", "raspi2b", "raspi3ap", "raspi3b", "raspi4b",
];

/// True when `machine` is a fixed-hardware Raspberry Pi board (see [`RASPI_MACHINES`]).
fn is_raspi_machine(machine: &str) -> bool {
    RASPI_MACHINES.contains(&machine)
}

const APPEND_CMDLINE_MESSAGE: &str =
    "appendCmdline must be a single line of printable characters (no control characters or \
     newlines). It is emitted as one -append token, so spaces are fine.";

/// Validate the kernel command line. It is emitted as a SINGLE argv token, so
/// spaces/`=`/`,` are safe (they cannot split off another argv element); the only
/// thing forbidden is control characters (newlines, NULs). Mirrors the TS
/// `VALID_APPEND_CMDLINE` regex `^[^\x00-\x1f\x7f]+$` plus the 2048-char cap.
fn validate_append_cmdline(s: &str) -> Result<(), HardwareSpecError> {
    if s.is_empty() || s.chars().count() > 2048 || s.chars().any(|c| c.is_control()) {
        return Err(HardwareSpecError(format!(
            "Invalid Hardware Spec — appendCmdline: {APPEND_CMDLINE_MESSAGE}"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Newtypes for validated free-string fields
// ---------------------------------------------------------------------------

/// A validated `-machine`/`-cpu` model name. Constructing one proves it passed the
/// [`VALID_MACHINE_CPU`] charset allowlist, so an unvalidated value cannot reach
/// argv generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineCpu(String);

impl MachineCpu {
    /// Validate `raw` for the named field (`machine` or `cpu`), or return an
    /// actionable [`HardwareSpecError`] mirroring the TS wording.
    pub fn parse(field: &str, raw: &str) -> Result<Self, HardwareSpecError> {
        if is_valid_machine_cpu(raw) {
            Ok(Self(raw.to_string()))
        } else {
            Err(HardwareSpecError(format!(
                "Invalid Hardware Spec — {field}: {field} {MACHINE_CPU_MESSAGE}."
            )))
        }
    }

    /// The validated model name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A validated `-boot order=` value: one or more QEMU boot drive letters
/// (`^[a-dnp]+`). Constructing one proves it carries no comma/`=`/space that could
/// inject a second `-boot` option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootOrder(String);

impl BootOrder {
    /// Validate a boot-order string, or return an actionable [`HardwareSpecError`].
    pub fn parse(raw: &str) -> Result<Self, HardwareSpecError> {
        if is_valid_boot_order(raw) {
            Ok(Self(raw.to_string()))
        } else {
            Err(HardwareSpecError(format!(
                "Invalid Hardware Spec — boot: {BOOT_ORDER_MESSAGE}."
            )))
        }
    }

    /// The validated boot order.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// Deserialized (untrusted) parameters — the tool input shape (serde + schemars)
// ---------------------------------------------------------------------------

/// A single guest disk as supplied by the agent. `image` is a bare name in the
/// Image Store (resolved + containment-checked at argv time); `interface`/`format`
/// are closed enums. Unknown fields are rejected (fail closed).
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiskParams {
    /// Name of a disk image in the Image Store (a bare name, never a host path).
    pub image: String,
    /// Disk controller: `virtio` (default), `ide`, or `scsi`.
    #[serde(default)]
    pub interface: DiskInterface,
    /// Image format pinned explicitly into argv: `qcow2` (default) or `raw`.
    #[serde(default)]
    pub format: ImageFormat,
    /// Attach the disk read-only.
    #[serde(default)]
    pub readonly: bool,
}

/// A CD-ROM drive backed by an ISO from the read-only ISO Store.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CdromParams {
    /// Name of an ISO in the read-only ISO Store (a bare name, never a host path).
    pub iso: String,
}

/// A single user-mode port-forward as supplied by the agent: expose guest
/// `guestPort` on host `hostPort`. Ports are validated to `1..=65535`; `proto` is a
/// closed enum.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostForwardParams {
    /// Host TCP/UDP port to bind (1-65535, and within `QMP_MCP_HOSTFWD_PORT_RANGE`).
    pub host_port: i64,
    /// Guest port the forward targets (1-65535).
    pub guest_port: i64,
    /// Forward protocol: `tcp` (default) or `udp`.
    #[serde(default)]
    pub proto: NetProtocol,
}

/// The guest NIC as supplied by the agent. Defaults to a single user-mode
/// `virtio-net-pci` with no port-forwards. `model`/`mode` are closed enums.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetworkParams {
    /// Networking backend: `user` (default) or `tap`/`bridge` (host networking).
    #[serde(default)]
    pub mode: NetworkMode,
    /// Guest NIC model from a fixed allowlist.
    #[serde(default)]
    pub model: NicModel,
    /// User-mode inbound port-forwards (host port -> guest port). Only valid for
    /// mode `user`.
    #[serde(default)]
    pub host_forwards: Vec<HostForwardParams>,
}

/// The untrusted candidate Hardware Spec as deserialized from the tool input.
/// Unknown fields are rejected (`deny_unknown_fields`, mirroring zod `.strict()`),
/// every field has a default, and the closed sets are enums. The free-string and
/// numeric fields are validated by [`parse_hardware_spec`] into a [`HardwareSpec`].
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HardwareSpecParams {
    /// QEMU machine type, e.g. `q35` (default) or `pc`.
    #[serde(default = "default_machine")]
    pub machine: String,
    /// CPU model passed to `-cpu`, e.g. `max` (default) or `host`.
    #[serde(default = "default_cpu")]
    pub cpu: String,
    /// Number of virtual CPUs (1-255).
    #[serde(default = "default_vcpus")]
    pub vcpus: i64,
    /// Guest RAM in MiB (1-1048576).
    #[serde(default = "default_memory_mb")]
    pub memory_mb: i64,
    /// Accelerator selection.
    #[serde(default)]
    pub accel: AccelMode,
    /// Guest Display mode.
    #[serde(default)]
    pub display: DisplayMode,
    /// Guest disks, each referencing an image by name in the Image Store.
    #[serde(default)]
    pub disks: Vec<DiskParams>,
    /// Optional CD-ROM drive backed by an ISO from the read-only ISO Store.
    #[serde(default)]
    pub cdrom: Option<CdromParams>,
    /// Optional boot order as QEMU drive letters, e.g. `d` or `dc`.
    #[serde(default)]
    pub boot: Option<String>,
    /// Optional external kernel image to direct-boot, by NAME in the Image Store
    /// (emitted as `-kernel`). REQUIRED for the raspi* boards; also usable for any
    /// direct-kernel boot (e.g. the `virt` machine).
    #[serde(default)]
    pub kernel: Option<String>,
    /// Optional device tree blob, by NAME in the Image Store (emitted as `-dtb`).
    /// Requires `kernel`.
    #[serde(default)]
    pub dtb: Option<String>,
    /// Optional kernel command line, emitted as a single `-append` token. Requires
    /// `kernel`. For a raspi framebuffer console visible over the noVNC Viewer,
    /// include `console=tty1`.
    #[serde(default)]
    pub append_cmdline: Option<String>,
    /// Guest NIC; defaults to user-mode (SLiRP) networking with no port-forwards.
    #[serde(default)]
    pub network: NetworkParams,
    /// Raw QEMU arguments appended verbatim; REFUSED unless `QMP_MCP_ALLOW_RAW_ARGS`.
    #[serde(default)]
    pub extra_args: Option<Vec<String>>,
}

fn default_machine() -> String {
    "q35".to_string()
}
fn default_cpu() -> String {
    "max".to_string()
}
fn default_vcpus() -> i64 {
    1
}
fn default_memory_mb() -> i64 {
    256
}

// ---------------------------------------------------------------------------
// Validated Hardware Spec (all defaults resolved, all fields proven safe)
// ---------------------------------------------------------------------------

/// A validated disk entry (all defaults resolved). The `image` name is validated at
/// argv time by the Image Store containment boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disk {
    /// Bare image name in the Image Store.
    pub image: String,
    /// Disk controller.
    pub interface: DiskInterface,
    /// Explicit image format.
    pub format: ImageFormat,
    /// Whether the disk is attached read-only.
    pub readonly: bool,
}

/// A validated CD-ROM entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cdrom {
    /// Bare ISO name in the ISO Store.
    pub iso: String,
}

/// A validated port-forward (ports in range, proto resolved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostForward {
    /// Host port (1-65535; bounded further to the configured range at argv time).
    pub host_port: u16,
    /// Guest port (1-65535).
    pub guest_port: u16,
    /// Forward protocol.
    pub proto: NetProtocol,
}

/// A validated guest NIC (all defaults resolved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    /// Networking backend.
    pub mode: NetworkMode,
    /// Guest NIC model.
    pub model: NicModel,
    /// User-mode inbound port-forwards.
    pub host_forwards: Vec<HostForward>,
}

/// A fully-validated Hardware Spec (all defaults resolved). Built only by
/// [`parse_hardware_spec`]; the newtype fields cannot be constructed unvalidated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardwareSpec {
    /// Validated machine type.
    pub machine: MachineCpu,
    /// Validated CPU model.
    pub cpu: MachineCpu,
    /// Virtual CPUs (1-255).
    pub vcpus: u32,
    /// Guest RAM in MiB (1-1048576).
    pub memory_mb: u32,
    /// Requested accelerator mode (resolved to a concrete [`Accel`] separately).
    pub accel: AccelMode,
    /// Guest Display mode.
    pub display: DisplayMode,
    /// Guest disks.
    pub disks: Vec<Disk>,
    /// Optional CD-ROM.
    pub cdrom: Option<Cdrom>,
    /// Optional validated boot order.
    pub boot: Option<BootOrder>,
    /// Optional external kernel image (by name in the Image Store) to direct-boot.
    pub kernel: Option<String>,
    /// Optional device tree blob (by name in the Image Store). Only set with `kernel`.
    pub dtb: Option<String>,
    /// Optional kernel command line (emitted as one `-append` token). Only set with `kernel`.
    pub append_cmdline: Option<String>,
    /// Guest NIC.
    pub network: Network,
    /// Optional raw-args escape hatch (gated by `QMP_MCP_ALLOW_RAW_ARGS`).
    pub extra_args: Option<Vec<String>>,
}

/// Raised when a candidate Hardware Spec fails validation, or when argv generation
/// refuses a spec (out-of-store disk, over-cap resource, gated feature). The
/// message names the offending field/constraint and the remediation. Mirrors the
/// TS `HardwareSpecError`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct HardwareSpecError(pub String);

/// Validate `raw` (an integer already deserialized as `i64`) into `lo..=hi`,
/// returning it or an actionable [`HardwareSpecError`] naming `field`.
fn validate_int_in(field: &str, raw: i64, lo: i64, hi: i64) -> Result<i64, HardwareSpecError> {
    if raw < lo || raw > hi {
        return Err(HardwareSpecError(format!(
            "Invalid Hardware Spec — {field}: {field} must be an integer between {lo} and {hi} \
             (got {raw})."
        )));
    }
    Ok(raw)
}

fn validate_disk(d: DiskParams) -> Result<Disk, HardwareSpecError> {
    if d.image.is_empty() {
        return Err(HardwareSpecError(
            "Invalid Hardware Spec — image: image must be a non-empty string.".to_string(),
        ));
    }
    Ok(Disk {
        image: d.image,
        interface: d.interface,
        format: d.format,
        readonly: d.readonly,
    })
}

fn validate_cdrom(c: CdromParams) -> Result<Cdrom, HardwareSpecError> {
    if c.iso.is_empty() {
        return Err(HardwareSpecError(
            "Invalid Hardware Spec — iso: iso must be a non-empty string.".to_string(),
        ));
    }
    Ok(Cdrom { iso: c.iso })
}

fn validate_host_forward(f: HostForwardParams) -> Result<HostForward, HardwareSpecError> {
    let host_port = validate_int_in("hostPort", f.host_port, 1, 65535)? as u16;
    let guest_port = validate_int_in("guestPort", f.guest_port, 1, 65535)? as u16;
    Ok(HostForward {
        host_port,
        guest_port,
        proto: f.proto,
    })
}

fn validate_network(n: NetworkParams) -> Result<Network, HardwareSpecError> {
    let host_forwards = n
        .host_forwards
        .into_iter()
        .map(validate_host_forward)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Network {
        mode: n.mode,
        model: n.model,
        host_forwards,
    })
}

/// Validate an untrusted candidate spec (already shaped/enum-checked by serde),
/// returning a fully-defaulted [`HardwareSpec`]. Returns an actionable
/// [`HardwareSpecError`] naming the offending field on failure. Mirrors the TS
/// `parseHardwareSpec`.
pub fn parse_hardware_spec(
    candidate: serde_json::Value,
) -> Result<HardwareSpec, HardwareSpecError> {
    let params: HardwareSpecParams = serde_json::from_value(candidate)
        .map_err(|e| HardwareSpecError(format!("Invalid Hardware Spec — {e}.")))?;

    let machine = MachineCpu::parse("machine", &params.machine)?;
    let cpu = MachineCpu::parse("cpu", &params.cpu)?;
    let vcpus = validate_int_in("vcpus", params.vcpus, 1, 255)? as u32;
    let memory_mb = validate_int_in("memoryMb", params.memory_mb, 1, 1_048_576)? as u32;
    let boot = params.boot.map(|b| BootOrder::parse(&b)).transpose()?;
    let disks = params
        .disks
        .into_iter()
        .map(validate_disk)
        .collect::<Result<Vec<_>, _>>()?;
    let cdrom = params.cdrom.map(validate_cdrom).transpose()?;
    let network = validate_network(params.network)?;

    // Boot artifacts. Names are containment-checked against the Image Store at argv
    // time (like disks), so here we only reject an empty name and enforce the
    // cross-field rule that -dtb/-append are meaningless without a -kernel.
    let kernel = validate_optional_name("kernel", params.kernel)?;
    let dtb = validate_optional_name("dtb", params.dtb)?;
    let append_cmdline = params.append_cmdline;
    if let Some(cmdline) = &append_cmdline {
        validate_append_cmdline(cmdline)?;
    }
    if dtb.is_some() && kernel.is_none() {
        return Err(HardwareSpecError(
            "Invalid Hardware Spec — dtb: dtb requires kernel (a device tree is only passed to a \
             direct-booted kernel). Set kernel, or remove dtb."
                .to_string(),
        ));
    }
    if append_cmdline.is_some() && kernel.is_none() {
        return Err(HardwareSpecError(
            "Invalid Hardware Spec — appendCmdline: appendCmdline requires kernel (a command line \
             is only passed to a direct-booted kernel). Set kernel, or remove appendCmdline."
                .to_string(),
        ));
    }

    Ok(HardwareSpec {
        machine,
        cpu,
        vcpus,
        memory_mb,
        accel: params.accel,
        display: params.display,
        disks,
        cdrom,
        boot,
        kernel,
        dtb,
        append_cmdline,
        network,
        extra_args: params.extra_args,
    })
}

/// Validate an optional bare name (kernel/dtb): reject an empty string, matching
/// the TS `z.string().min(1)`. Containment against the Image Store happens at argv
/// time in [`resolve_boot_artifact`].
fn validate_optional_name(
    field: &str,
    value: Option<String>,
) -> Result<Option<String>, HardwareSpecError> {
    match value {
        Some(s) if s.is_empty() => Err(HardwareSpecError(format!(
            "Invalid Hardware Spec — {field}: {field} must be a non-empty string."
        ))),
        other => Ok(other),
    }
}

// ---------------------------------------------------------------------------
// Accelerator resolution (ADR-0008)
// ---------------------------------------------------------------------------

/// The outcome of resolving the requested accelerator to a concrete one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccelResolution {
    /// The accelerator QEMU will actually be launched with.
    pub accel: Accel,
    /// The mode the caller requested (for reporting).
    pub requested: AccelMode,
    /// Human-readable reason for the choice.
    pub reason: String,
}

/// Raised when `accel: kvm` is forced but `/dev/kvm` is not accessible.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct AccelError(pub String);

/// Default `/dev/kvm` probe: KVM is usable when the device exists and is both
/// readable and writable by this (unprivileged) process. Any failure reads as
/// "unavailable". Mirrors the TS `probeKvm`.
pub fn probe_kvm() -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok()
}

/// Resolve the requested accelerator mode to a concrete accelerator, reporting
/// which was chosen and why (ADR-0008). `kvm_available` is injected so this is
/// testable without a real `/dev/kvm` (pass [`probe_kvm`] in production).
pub fn resolve_accel(
    requested: AccelMode,
    kvm_available: impl Fn() -> bool,
) -> Result<AccelResolution, AccelError> {
    match requested {
        AccelMode::Tcg => Ok(AccelResolution {
            accel: Accel::Tcg,
            requested,
            reason: "accel=tcg requested; using TCG software emulation.".to_string(),
        }),
        AccelMode::Kvm => {
            if !kvm_available() {
                return Err(AccelError(
                    "accel=kvm was requested but /dev/kvm is not accessible. Grant the \
                     container/user access to /dev/kvm (add it as a device and join the kvm \
                     group), or use accel=auto or accel=tcg."
                        .to_string(),
                ));
            }
            Ok(AccelResolution {
                accel: Accel::Kvm,
                requested,
                reason: "accel=kvm requested; /dev/kvm is accessible.".to_string(),
            })
        }
        AccelMode::Auto => {
            if kvm_available() {
                Ok(AccelResolution {
                    accel: Accel::Kvm,
                    requested,
                    reason: "accel=auto: /dev/kvm is accessible, using KVM.".to_string(),
                })
            } else {
                Ok(AccelResolution {
                    accel: Accel::Tcg,
                    requested,
                    reason: "accel=auto: /dev/kvm is not accessible, falling back to TCG."
                        .to_string(),
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The pure spec -> argv generator
// ---------------------------------------------------------------------------

/// The single Instance's loopback VNC Display endpoint (ADR-0010): bind host and
/// fixed display number 0 (exactly one Instance runs at a time).
pub const VNC_LOOPBACK_HOST: &str = "127.0.0.1";
/// The fixed VNC display number.
pub const VNC_DISPLAY_NUMBER: u16 = 0;
/// The loopback TCP port of the VNC Display: QEMU maps `-vnc host:N` to TCP port
/// `5900 + N`, so the Viewer's proxy always dials this one server-fixed endpoint
/// (never a client-supplied target). Mirrors the TS `VNC_LOOPBACK_PORT`.
pub const VNC_LOOPBACK_PORT: u16 = 5900 + VNC_DISPLAY_NUMBER;

/// Fixed id tying a `-netdev` backend to its `-device` NIC. A single NIC per
/// Instance in this slice, so a constant (non-agent-controlled) id suffices.
const NETDEV_ID: &str = "net0";

/// Inputs for [`build_argv`] that are not part of the Hardware Spec itself. Mirrors
/// the TS `ArgvOptions`; the Orchestrator injects the env-resolved values.
#[derive(Debug, Clone)]
pub struct ArgvOptions {
    /// The concrete accelerator (already resolved from [`resolve_accel`]).
    pub accel: Accel,
    /// Absolute path of the server-managed QMP UNIX socket.
    pub qmp_socket_path: String,
    /// Absolute path of the Image Store directory; required when the spec has disks.
    pub image_dir: Option<String>,
    /// Absolute path of the read-only ISO Store; required when the spec has a cdrom.
    pub iso_dir: Option<String>,
    /// Inclusive host-port range a forward's `hostPort` must fall within; defaults
    /// to [`DEFAULT_HOSTFWD_PORT_RANGE`] when `None`.
    pub hostfwd_port_range: Option<PortRange>,
    /// Whether host-level (`tap`/`bridge`) networking is permitted.
    pub allow_host_net: bool,
    /// Hard cap on `memoryMb` (`QMP_MCP_MAX_MEMORY_MB`); `None` skips the check.
    pub max_memory_mb: Option<u32>,
    /// Hard cap on `vcpus` (`QMP_MCP_MAX_VCPUS`); `None` skips the check.
    pub max_vcpus: Option<u32>,
    /// Whether the raw-args escape hatch is enabled (`QMP_MCP_ALLOW_RAW_ARGS`).
    pub allow_raw_args: bool,
}

/// Comma-escape a value interpolated into a QemuOpts property string
/// (`-drive`/`-machine`), where a literal comma must be doubled (`,,`). Defense in
/// depth: the validators already reject commas in agent-controlled names, but the
/// resolved file path is host/Store-derived, so escaping it here guarantees a comma
/// in the path can never split off an extra property.
fn escape_qemu_opts_value(value: &str) -> String {
    value.replace(',', ",,")
}

/// Resolve a disk's image name to a safe in-Store path and render a `-drive`
/// argument pair with an explicit `format=` (QEMU auto-probing is never relied
/// upon). Any out-of-store/absolute/traversal/symlink-escape reference is rejected
/// here (argv time).
fn build_drive_args(
    disk: &Disk,
    image_dir: Option<&str>,
) -> Result<Vec<String>, HardwareSpecError> {
    let dir = match image_dir {
        Some(d) if !d.trim().is_empty() => d,
        _ => {
            return Err(HardwareSpecError(format!(
                "Disk \"{}\" was requested but the Image Store directory is not configured. Set \
                 QMP_MCP_IMAGE_DIR to the Image Store path.",
                disk.image
            )));
        }
    };
    let path = resolve_image_path(&disk.image, dir)
        .map_err(|e| HardwareSpecError(format!("Invalid disk reference: {}", e.0)))?;
    let mut parts = vec![
        format!("file={}", escape_qemu_opts_value(&path)),
        format!("format={}", disk.format.as_str()),
        format!("if={}", disk.interface.as_str()),
        "media=disk".to_string(),
    ];
    if disk.readonly {
        parts.push("readonly=on".to_string());
    }
    Ok(vec!["-drive".to_string(), parts.join(",")])
}

/// Resolve a CD-ROM's ISO name to a safe in-Store path and render a read-only
/// `-drive ...,media=cdrom,readonly=on,format=raw` argument pair against the
/// SEPARATE read-only ISO Store. Any escaping reference is rejected here.
fn build_cdrom_args(
    cdrom: &Cdrom,
    iso_dir: Option<&str>,
) -> Result<Vec<String>, HardwareSpecError> {
    let dir = match iso_dir {
        Some(d) if !d.trim().is_empty() => d,
        _ => {
            return Err(HardwareSpecError(format!(
                "A CD-ROM with ISO \"{}\" was requested but the ISO Store directory is not \
                 configured. Set QMP_MCP_ISO_DIR to the read-only ISO Store path.",
                cdrom.iso
            )));
        }
    };
    let path = resolve_iso_path(&cdrom.iso, dir)
        .map_err(|e| HardwareSpecError(format!("Invalid ISO reference: {}", e.0)))?;
    let parts = [
        format!("file={}", escape_qemu_opts_value(&path)),
        "media=cdrom".to_string(),
        "readonly=on".to_string(),
        "format=raw".to_string(),
    ];
    Ok(vec!["-drive".to_string(), parts.join(",")])
}

/// Resolve a boot artifact (kernel or dtb) name to a safe in-Store path for
/// `-kernel`/`-dtb`. Referenced by NAME in the Image Store and resolved through the
/// same containment boundary as a disk (any traversal/out-of-store name is rejected
/// here at argv time). Emitted as its own standalone argv token (not a QemuOpts
/// property), so no comma-escaping is needed. A boot artifact with no Image Store
/// configured fails closed naming `QMP_MCP_IMAGE_DIR`. Mirrors the TS
/// `resolveBootArtifact`.
fn resolve_boot_artifact(
    name: &str,
    image_dir: Option<&str>,
    kind: &str,
) -> Result<String, HardwareSpecError> {
    let dir = match image_dir {
        Some(d) if !d.trim().is_empty() => d,
        _ => {
            return Err(HardwareSpecError(format!(
                "A {kind} (\"{name}\") was requested but the Image Store directory is not \
                 configured. Set QMP_MCP_IMAGE_DIR to the Image Store path."
            )));
        }
    };
    resolve_image_path(name, dir)
        .map_err(|e| HardwareSpecError(format!("Invalid {kind} reference: {}", e.0)))
}

/// Render the `-netdev`/`-device` pair for the guest NIC (ADR-0009). `user` mode
/// emits SLiRP NAT plus any loopback-pinned `hostfwd=` entries (host port bounded
/// to `hostfwd_port_range`); `tap`/`bridge` is refused unless `allow_host_net`.
/// `hostForwards` with a non-user mode are refused rather than silently dropped.
fn build_network_args(
    network: &Network,
    machine: &str,
    options: &ArgvOptions,
) -> Result<Vec<String>, HardwareSpecError> {
    if network.mode != NetworkMode::User && !network.host_forwards.is_empty() {
        let mode = network.mode.as_str();
        return Err(HardwareSpecError(format!(
            "network.hostForwards are only valid for user-mode networking (mode \"user\"), but \
             mode is \"{mode}\". Remove hostForwards, or use mode \"user\"."
        )));
    }

    // 'none' — attach no NIC. The escape hatch for boards where no allowlisted NIC
    // can attach; emit nothing.
    if network.mode == NetworkMode::None {
        return Ok(vec![]);
    }

    // NIC model must match a bus the machine actually has, or QEMU aborts at launch
    // ("No 'PCI' bus found for device ..."). Fail closed here with an actionable
    // message instead. The raspi* boards have a USB bus but no PCI bus; every other
    // machine we target has PCI (via -nodefaults) but no built-in USB controller.
    let raspi = is_raspi_machine(machine);
    if network.model == NicModel::UsbNet && !raspi {
        return Err(HardwareSpecError(format!(
            "network.model \"usb-net\" needs a USB bus, but machine \"{machine}\" has none (only \
             the raspi* boards expose a built-in USB bus). Use a PCI NIC \
             (virtio-net-pci/e1000/rtl8139), or network.mode \"none\"."
        )));
    }
    if network.model.is_pci() && raspi {
        return Err(HardwareSpecError(format!(
            "network.model \"{}\" is a PCI NIC, but the raspi* board \"{machine}\" has no PCI bus. \
             Use network.model \"usb-net\", or network.mode \"none\" for no networking.",
            network.model.as_str()
        )));
    }

    // model is an allowlisted enum; escape defensively so it can never inject an
    // extra -device property no matter how the allowlist evolves.
    let device = format!(
        "{},netdev={NETDEV_ID}",
        escape_qemu_opts_value(network.model.as_str())
    );

    if network.mode == NetworkMode::User {
        let range = options
            .hostfwd_port_range
            .unwrap_or(DEFAULT_HOSTFWD_PORT_RANGE);
        let mut netdev_parts = vec!["user".to_string(), format!("id={NETDEV_ID}")];
        for fwd in &network.host_forwards {
            if fwd.host_port < range.low || fwd.host_port > range.high {
                return Err(HardwareSpecError(format!(
                    "Host port-forward hostPort {} is outside the allowed host-port range {}-{} \
                     (QMP_MCP_HOSTFWD_PORT_RANGE). Choose a hostPort within {}-{}, or widen the \
                     range via QMP_MCP_HOSTFWD_PORT_RANGE.",
                    fwd.host_port, range.low, range.high, range.low, range.high
                )));
            }
            // proto is an enum and both ports are validated ints — no free-text.
            // The host address is pinned to 127.0.0.1 (loopback) so the forward is
            // reachable only from the host itself, not the host LAN.
            netdev_parts.push(format!(
                "hostfwd={}:127.0.0.1:{}-:{}",
                fwd.proto.as_str(),
                fwd.host_port,
                fwd.guest_port
            ));
        }
        return Ok(vec![
            "-netdev".to_string(),
            netdev_parts.join(","),
            "-device".to_string(),
            device,
        ]);
    }

    // tap / bridge: host-level networking, env-gated off by default (ADR-0008/0009).
    if !options.allow_host_net {
        let mode = network.mode.as_str();
        return Err(HardwareSpecError(format!(
            "network.mode \"{mode}\" requests host-level ({mode}) networking, which puts the guest \
             on the host LAN and needs host privileges, so it is refused by default (ADR-0009). \
             Use mode \"user\" for default user-mode networking, or set QMP_MCP_ALLOW_HOST_NET=true \
             to opt in (the operator must provision the host bridge/tap; the server does not \
             configure the host)."
        )));
    }
    // mode is an allowlisted enum; escape defensively as with the model.
    Ok(vec![
        "-netdev".to_string(),
        format!(
            "{},id={NETDEV_ID}",
            escape_qemu_opts_value(network.mode.as_str())
        ),
        "-device".to_string(),
        device,
    ])
}

/// Enforce the env-configurable resource caps on a validated spec (issue #9). The
/// caps live OUTSIDE the schema (operator policy), so they are injected via
/// [`ArgvOptions`] and checked here. An over-cap spec is rejected naming the cap
/// variable and the requested-vs-allowed values; an omitted cap skips its check.
fn assert_within_resource_caps(
    spec: &HardwareSpec,
    options: &ArgvOptions,
) -> Result<(), HardwareSpecError> {
    if let Some(cap) = options.max_memory_mb {
        if spec.memory_mb > cap {
            return Err(HardwareSpecError(format!(
                "memoryMb {} exceeds QMP_MCP_MAX_MEMORY_MB={cap}. Request {cap} MiB or less, or \
                 raise QMP_MCP_MAX_MEMORY_MB.",
                spec.memory_mb
            )));
        }
    }
    if let Some(cap) = options.max_vcpus {
        if spec.vcpus > cap {
            return Err(HardwareSpecError(format!(
                "vcpus {} exceeds QMP_MCP_MAX_VCPUS={cap}. Request {cap} vCPU(s) or less, or raise \
                 QMP_MCP_MAX_VCPUS.",
                spec.vcpus
            )));
        }
    }
    Ok(())
}

/// Generate the full `qemu-system-*` argv (excluding the program name) from a
/// validated Hardware Spec. Pure: same inputs always yield the same array. The argv
/// is headless and minimal by construction (`-nodefaults -nographic`), and `-S`
/// freezes the vCPUs at startup so the Instance reaches a deterministic,
/// agent-inspectable state before any Guest code runs. Mirrors the TS `buildArgv`
/// byte-for-byte (flag ordering and spelling included).
pub fn build_argv(
    spec: &HardwareSpec,
    options: &ArgvOptions,
) -> Result<Vec<String>, HardwareSpecError> {
    // Resource caps are operator policy, injected from config and checked before
    // anything else so an over-cap spec never reaches qemu.
    assert_within_resource_caps(spec, options)?;

    let raspi = is_raspi_machine(spec.machine.as_str());
    let mut argv: Vec<String> = vec![
        "-machine".to_string(),
        format!(
            "{},accel={}",
            escape_qemu_opts_value(spec.machine.as_str()),
            options.accel.as_str()
        ),
    ];
    // The raspi* boards have fixed CPU/core-count/RAM baked into the machine and
    // QEMU rejects -cpu/-smp/-m for them, so emit those ONLY for other machines.
    if !raspi {
        argv.push("-cpu".to_string());
        argv.push(spec.cpu.as_str().to_string());
        argv.push("-smp".to_string());
        argv.push(spec.vcpus.to_string());
        argv.push("-m".to_string());
        argv.push(spec.memory_mb.to_string());
    }
    // Direct-kernel boot (-kernel/-dtb/-append). Required for the raspi* machines
    // (they do not boot the SD card's firmware), and usable for any machine. The
    // schema guarantees dtb/appendCmdline only appear alongside a kernel. -append is
    // one argv token, so its spaces cannot split off another token.
    if let Some(kernel) = &spec.kernel {
        argv.push("-kernel".to_string());
        argv.push(resolve_boot_artifact(
            kernel,
            options.image_dir.as_deref(),
            "kernel",
        )?);
    }
    if let Some(dtb) = &spec.dtb {
        argv.push("-dtb".to_string());
        argv.push(resolve_boot_artifact(
            dtb,
            options.image_dir.as_deref(),
            "dtb",
        )?);
    }
    if let Some(cmdline) = &spec.append_cmdline {
        argv.push("-append".to_string());
        argv.push(cmdline.clone());
    }
    argv.push("-nodefaults".to_string());
    argv.push("-nographic".to_string());
    argv.push("-S".to_string());

    if spec.display == DisplayMode::Vnc {
        // Loopback-only VNC Display (ADR-0010). `password=on` REQUIRES a password
        // but does NOT carry one: the Orchestrator sets it after launch over QMP,
        // so no VNC secret ever appears in argv or `ps`.
        argv.push("-vnc".to_string());
        argv.push(format!(
            "{VNC_LOOPBACK_HOST}:{VNC_DISPLAY_NUMBER},password=on"
        ));
    }

    for disk in &spec.disks {
        argv.extend(build_drive_args(disk, options.image_dir.as_deref())?);
    }
    if let Some(cdrom) = &spec.cdrom {
        argv.extend(build_cdrom_args(cdrom, options.iso_dir.as_deref())?);
    }
    if let Some(boot) = &spec.boot {
        // boot is allowlisted to [a-dnp]+ already; emit the single order= form (and
        // comma-escape as defense-in-depth).
        argv.push("-boot".to_string());
        argv.push(format!("order={}", escape_qemu_opts_value(boot.as_str())));
    }

    // Guest NIC: user-mode (SLiRP) by default; tap/bridge only when env-gated on.
    argv.extend(build_network_args(
        &spec.network,
        spec.machine.as_str(),
        options,
    )?);

    // The QMP monitor UNIX socket the server owns. The path is NOT comma-escaped —
    // it is emitted verbatim, matching the TS generator exactly.
    argv.push("-qmp".to_string());
    argv.push(format!(
        "unix:{},server=on,wait=off",
        options.qmp_socket_path
    ));

    // extraArgs (ADR-0002): the opt-in raw-args escape hatch. Raw QEMU arguments
    // are host-compromise-equivalent, so when a spec carries them but
    // QMP_MCP_ALLOW_RAW_ARGS is not enabled we REFUSE (fail-closed) rather than
    // silently dropping them. When enabled, they are appended verbatim.
    if let Some(extra) = &spec.extra_args {
        if !extra.is_empty() {
            if !options.allow_raw_args {
                return Err(HardwareSpecError(
                    "extraArgs were supplied, but the raw-args escape hatch is disabled. Raw QEMU \
                     arguments are host-compromise-equivalent, so they are refused by default \
                     (ADR-0002). Set QMP_MCP_ALLOW_RAW_ARGS=true to opt in (trusted single-tenant \
                     hosts only), or remove extraArgs and express the hardware through the Hardware \
                     Spec."
                        .to_string(),
                ));
            }
            argv.extend(extra.iter().cloned());
        }
    }

    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SOCK: &str = "/run/qmp-mcp/qmp.sock";

    /// Parse a raw candidate (as an agent would supply it), asserting success.
    fn spec(candidate: serde_json::Value) -> HardwareSpec {
        parse_hardware_spec(candidate).expect("valid spec")
    }

    /// Minimal argv options: TCG, a fixed socket, no stores/caps.
    fn opts() -> ArgvOptions {
        ArgvOptions {
            accel: Accel::Tcg,
            qmp_socket_path: SOCK.to_string(),
            image_dir: None,
            iso_dir: None,
            hostfwd_port_range: None,
            allow_host_net: false,
            max_memory_mb: None,
            max_vcpus: None,
            allow_raw_args: false,
        }
    }

    fn index_of(argv: &[String], flag: &str) -> usize {
        argv.iter().position(|s| s == flag).expect("flag present")
    }
    fn value_after<'a>(argv: &'a [String], flag: &str) -> &'a str {
        &argv[index_of(argv, flag) + 1]
    }

    // --- parse_hardware_spec ------------------------------------------------

    #[test]
    fn fills_every_field_with_a_default_for_an_empty_spec() {
        let s = spec(json!({}));
        assert_eq!(s.machine.as_str(), "q35");
        assert_eq!(s.cpu.as_str(), "max");
        assert_eq!(s.vcpus, 1);
        assert_eq!(s.memory_mb, 256);
        assert_eq!(s.accel, AccelMode::Auto);
        assert_eq!(s.display, DisplayMode::None);
        assert!(s.disks.is_empty());
        assert!(s.cdrom.is_none());
        assert!(s.boot.is_none());
        assert_eq!(s.network.mode, NetworkMode::User);
        assert_eq!(s.network.model, NicModel::VirtioNetPci);
        assert!(s.network.host_forwards.is_empty());
        assert!(s.extra_args.is_none());
    }

    #[test]
    fn defaults_a_disk_entry() {
        let s = spec(json!({ "disks": [{ "image": "root.qcow2" }] }));
        assert_eq!(s.disks.len(), 1);
        assert_eq!(s.disks[0].image, "root.qcow2");
        assert_eq!(s.disks[0].interface, DiskInterface::Virtio);
        assert_eq!(s.disks[0].format, ImageFormat::Qcow2);
        assert!(!s.disks[0].readonly);
    }

    #[test]
    fn rejects_unknown_fields_failing_closed() {
        assert!(parse_hardware_spec(json!({ "disk": "foo.qcow2" })).is_err());
        assert!(parse_hardware_spec(json!({ "disks": [{ "image": "d", "path": "/x" }] })).is_err());
        assert!(parse_hardware_spec(json!({ "network": { "foo": "bar" } })).is_err());
        assert!(parse_hardware_spec(json!({ "cdrom": { "iso": "d.iso", "file": "/x" } })).is_err());
    }

    #[test]
    fn names_the_offending_field_on_a_bad_value() {
        assert!(parse_hardware_spec(json!({ "vcpus": 0 }))
            .unwrap_err()
            .0
            .contains("vcpus"));
        assert!(parse_hardware_spec(json!({ "accel": "xen" })).is_err());
    }

    #[test]
    fn coerces_nothing_non_integer_vcpu_rejected() {
        assert!(parse_hardware_spec(json!({ "vcpus": 1.5 })).is_err());
    }

    #[test]
    fn display_defaults_none_accepts_vnc_rejects_unknown() {
        assert_eq!(spec(json!({})).display, DisplayMode::None);
        assert_eq!(spec(json!({ "display": "vnc" })).display, DisplayMode::Vnc);
        assert!(parse_hardware_spec(json!({ "display": "spice" })).is_err());
    }

    #[test]
    fn rejects_machine_and_cpu_with_injected_property() {
        let e = parse_hardware_spec(json!({ "machine": "q35,accel=tcg" })).unwrap_err();
        assert!(e.0.contains("machine"));
        assert!(parse_hardware_spec(json!({ "cpu": "host,+vmx" }))
            .unwrap_err()
            .0
            .contains("cpu"));
        // A plain model is accepted.
        assert_eq!(spec(json!({ "cpu": "max" })).cpu.as_str(), "max");
    }

    // --- boot & cdrom parse -------------------------------------------------

    #[test]
    fn accepts_valid_boot_orders_and_rejects_injection() {
        assert_eq!(spec(json!({ "boot": "d" })).boot.unwrap().as_str(), "d");
        assert_eq!(spec(json!({ "boot": "dc" })).boot.unwrap().as_str(), "dc");
        assert_eq!(spec(json!({ "boot": "cdn" })).boot.unwrap().as_str(), "cdn");
        for bad in ["c,menu=on", "d order=c", "c,reboot-timeout=-1", "z", ""] {
            assert!(
                parse_hardware_spec(json!({ "boot": bad })).is_err(),
                "boot {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn accepts_a_cdrom_by_name_and_rejects_unknown_field() {
        assert_eq!(
            spec(json!({ "cdrom": { "iso": "debian.iso" } }))
                .cdrom
                .unwrap()
                .iso,
            "debian.iso"
        );
        assert!(parse_hardware_spec(json!({ "cdrom": { "iso": "d.iso", "x": 1 } })).is_err());
    }

    // --- network parse ------------------------------------------------------

    #[test]
    fn network_defaults_and_port_forward_proto_default() {
        let s = spec(json!({
            "network": { "hostForwards": [{ "hostPort": 8022, "guestPort": 22 }] }
        }));
        assert_eq!(s.network.host_forwards[0].proto, NetProtocol::Tcp);
        assert_eq!(s.network.host_forwards[0].host_port, 8022);
    }

    #[test]
    fn rejects_non_allowlisted_nic_model_and_injected_values() {
        assert!(parse_hardware_spec(json!({ "network": { "model": "pcnet" } })).is_err());
        assert!(
            parse_hardware_spec(json!({ "network": { "model": "virtio-net-pci,addr=0x4" } }))
                .is_err()
        );
        assert!(parse_hardware_spec(json!({ "network": { "mode": "user,smb=on" } })).is_err());
    }

    #[test]
    fn rejects_out_of_range_ports_and_non_enum_proto() {
        for port in [json!(0), json!(-1), json!(1.5), json!(70000)] {
            assert!(parse_hardware_spec(
                json!({ "network": { "hostForwards": [{ "hostPort": 2000, "guestPort": port }] } })
            )
            .is_err());
            assert!(parse_hardware_spec(
                json!({ "network": { "hostForwards": [{ "hostPort": port, "guestPort": 22 }] } })
            )
            .is_err());
        }
        assert!(parse_hardware_spec(
            json!({ "network": { "hostForwards": [{ "hostPort": 2000, "guestPort": 22, "proto": "icmp" }] } })
        )
        .is_err());
    }

    // --- buildArgv core -----------------------------------------------------

    #[test]
    fn maps_machine_cpu_smp_memory_and_accel() {
        let argv = build_argv(
            &spec(json!({ "machine": "pc", "cpu": "host", "vcpus": 4, "memoryMb": 2048 })),
            &opts(),
        )
        .unwrap();
        assert_eq!(value_after(&argv, "-machine"), "pc,accel=tcg");
        assert_eq!(value_after(&argv, "-cpu"), "host");
        assert_eq!(value_after(&argv, "-smp"), "4");
        assert_eq!(value_after(&argv, "-m"), "2048");
    }

    #[test]
    fn encodes_kvm_accel_and_wires_qmp_socket_headless_frozen() {
        let mut o = opts();
        o.accel = Accel::Kvm;
        let argv = build_argv(&spec(json!({})), &o).unwrap();
        assert_eq!(value_after(&argv, "-machine"), "q35,accel=kvm");
        assert!(argv.iter().any(|s| s == "-nodefaults"));
        assert!(argv.iter().any(|s| s == "-nographic"));
        assert!(argv.iter().any(|s| s == "-S"));
        assert_eq!(
            value_after(&argv, "-qmp"),
            format!("unix:{SOCK},server=on,wait=off")
        );
    }

    #[test]
    fn is_pure_same_inputs_same_argv() {
        assert_eq!(
            build_argv(&spec(json!({})), &opts()).unwrap(),
            build_argv(&spec(json!({})), &opts()).unwrap()
        );
    }

    // --- display ------------------------------------------------------------

    #[test]
    fn display_none_stays_headless_no_vnc() {
        let argv = build_argv(&spec(json!({})), &opts()).unwrap();
        assert!(!argv.iter().any(|s| s == "-vnc"));
    }

    #[test]
    fn display_vnc_emits_loopback_with_no_plaintext_password() {
        let argv = build_argv(&spec(json!({ "display": "vnc" })), &opts()).unwrap();
        assert_eq!(value_after(&argv, "-vnc"), "127.0.0.1:0,password=on");
        // No `password=<secret>` form anywhere.
        assert!(!argv.join(" ").contains("password=on,")); // no trailing injected props
        assert!(argv.iter().any(|s| s == "-S"));
    }

    // --- resource caps ------------------------------------------------------

    #[test]
    fn rejects_memory_over_cap_naming_it() {
        let mut o = opts();
        o.max_memory_mb = Some(4096);
        let e = build_argv(&spec(json!({ "memoryMb": 8192 })), &o).unwrap_err();
        assert!(e
            .0
            .contains("memoryMb 8192 exceeds QMP_MCP_MAX_MEMORY_MB=4096"));
    }

    #[test]
    fn rejects_vcpus_over_cap_naming_it() {
        let mut o = opts();
        o.max_vcpus = Some(2);
        let e = build_argv(&spec(json!({ "vcpus": 8 })), &o).unwrap_err();
        assert!(e.0.contains("vcpus 8 exceeds QMP_MCP_MAX_VCPUS=2"));
    }

    #[test]
    fn accepts_at_cap_and_skips_when_no_caps() {
        let mut o = opts();
        o.max_memory_mb = Some(4096);
        o.max_vcpus = Some(2);
        let argv = build_argv(&spec(json!({ "memoryMb": 4096, "vcpus": 2 })), &o).unwrap();
        assert_eq!(value_after(&argv, "-m"), "4096");
        assert_eq!(value_after(&argv, "-smp"), "2");
        // No caps injected: a large spec is admitted.
        assert!(build_argv(&spec(json!({ "memoryMb": 1000000, "vcpus": 200 })), &opts()).is_ok());
    }

    // --- extraArgs gate -----------------------------------------------------

    #[test]
    fn appends_extra_args_only_when_allowed() {
        let mut o = opts();
        o.allow_raw_args = true;
        let argv = build_argv(&spec(json!({ "extraArgs": ["-vga", "std"] })), &o).unwrap();
        assert_eq!(
            &argv[argv.len() - 2..],
            ["-vga".to_string(), "std".to_string()]
        );
    }

    #[test]
    fn rejects_extra_args_by_default_naming_the_flag() {
        let e = build_argv(
            &spec(json!({ "extraArgs": ["-drive", "file=/etc/shadow"] })),
            &opts(),
        )
        .unwrap_err();
        assert!(e.0.contains("QMP_MCP_ALLOW_RAW_ARGS"));
        // Empty extraArgs is a no-op even when disabled.
        let base = build_argv(&spec(json!({})), &opts()).unwrap();
        assert_eq!(
            build_argv(&spec(json!({ "extraArgs": [] })), &opts()).unwrap(),
            base
        );
    }

    // --- disks (containment + escaping) ------------------------------------

    #[test]
    fn emits_drive_with_explicit_format_and_honours_readonly_interface() {
        let store = TempDir::new("hw-disks");
        std::fs::write(store.path.join("root.qcow2"), b"").unwrap();
        let mut o = opts();
        o.image_dir = Some(store.path.to_string_lossy().into_owned());

        let argv = build_argv(&spec(json!({ "disks": [{ "image": "root.qcow2" }] })), &o).unwrap();
        let drive = value_after(&argv, "-drive");
        assert!(drive.contains("format=qcow2"));
        assert!(drive.contains("if=virtio"));
        assert!(drive.contains("media=disk"));
        assert!(!drive.contains("readonly=on"));

        let argv = build_argv(
            &spec(json!({ "disks": [{ "image": "root.qcow2", "interface": "ide", "readonly": true }] })),
            &o,
        )
        .unwrap();
        let drive = value_after(&argv, "-drive");
        assert!(drive.contains("if=ide"));
        assert!(drive.contains("readonly=on"));
    }

    #[test]
    fn rejects_absolute_traversal_injection_and_missing_store() {
        let store = TempDir::new("hw-disks2");
        let mut o = opts();
        o.image_dir = Some(store.path.to_string_lossy().into_owned());

        assert!(build_argv(&spec(json!({ "disks": [{ "image": "/etc/passwd" }] })), &o).is_err());
        assert!(build_argv(
            &spec(json!({ "disks": [{ "image": "../escape.qcow2" }] })),
            &o
        )
        .is_err());
        assert!(build_argv(
            &spec(json!({ "disks": [{ "image": "root.qcow2,readonly=on" }] })),
            &o
        )
        .is_err());
        // Missing Image Store dir fails closed naming the env var.
        let e = build_argv(
            &spec(json!({ "disks": [{ "image": "root.qcow2" }] })),
            &opts(),
        )
        .unwrap_err();
        assert!(e.0.contains("QMP_MCP_IMAGE_DIR"));
    }

    #[test]
    fn comma_escapes_a_store_path_containing_a_comma() {
        let store = TempDir::new("hw,disks"); // deliberate comma in the path
        assert!(store.path.to_string_lossy().contains(','));
        std::fs::write(store.path.join("root.qcow2"), b"").unwrap();
        let mut o = opts();
        o.image_dir = Some(store.path.to_string_lossy().into_owned());
        let argv = build_argv(&spec(json!({ "disks": [{ "image": "root.qcow2" }] })), &o).unwrap();
        let drive = value_after(&argv, "-drive");
        let escaped = store
            .path
            .join("root.qcow2")
            .to_string_lossy()
            .replace(',', ",,");
        assert_eq!(
            drive,
            format!("file={escaped},format=qcow2,if=virtio,media=disk")
        );
        // Splitting on single commas (doubled commas are literal) yields exactly 4 props.
        assert_eq!(drive.replace(",,", " ").split(',').count(), 4);
    }

    #[test]
    fn rejects_symlink_that_escapes_the_store() {
        let store = TempDir::new("hw-symlink");
        let mut o = opts();
        o.image_dir = Some(store.path.to_string_lossy().into_owned());
        std::os::unix::fs::symlink("/etc/passwd", store.path.join("evil.qcow2")).unwrap();
        let e = build_argv(&spec(json!({ "disks": [{ "image": "evil.qcow2" }] })), &o).unwrap_err();
        assert!(e.0.contains("symlink escape"));
    }

    // --- cdrom & boot argv --------------------------------------------------

    #[test]
    fn emits_readonly_cdrom_with_explicit_raw_format() {
        let iso = TempDir::new("hw-iso");
        std::fs::write(iso.path.join("debian.iso"), b"").unwrap();
        let mut o = opts();
        o.iso_dir = Some(iso.path.to_string_lossy().into_owned());
        let argv = build_argv(&spec(json!({ "cdrom": { "iso": "debian.iso" } })), &o).unwrap();
        let drive = value_after(&argv, "-drive");
        assert!(drive.contains("media=cdrom"));
        assert!(drive.contains("readonly=on"));
        assert!(drive.contains("format=raw"));
        // Missing ISO Store fails closed naming the env var.
        let e =
            build_argv(&spec(json!({ "cdrom": { "iso": "debian.iso" } })), &opts()).unwrap_err();
        assert!(e.0.contains("QMP_MCP_ISO_DIR"));
    }

    #[test]
    fn emits_boot_order_token_or_omits_it() {
        let argv = build_argv(&spec(json!({ "boot": "dc" })), &opts()).unwrap();
        assert_eq!(value_after(&argv, "-boot"), "order=dc");
        let argv = build_argv(&spec(json!({})), &opts()).unwrap();
        assert!(!argv.iter().any(|s| s == "-boot"));
    }

    // --- network argv -------------------------------------------------------

    #[test]
    fn emits_default_user_nic_and_honours_model() {
        let argv = build_argv(&spec(json!({})), &opts()).unwrap();
        assert_eq!(value_after(&argv, "-netdev"), "user,id=net0");
        assert_eq!(value_after(&argv, "-device"), "virtio-net-pci,netdev=net0");
        let argv = build_argv(&spec(json!({ "network": { "model": "e1000" } })), &opts()).unwrap();
        assert_eq!(value_after(&argv, "-device"), "e1000,netdev=net0");
    }

    #[test]
    fn emits_loopback_hostfwd_entries_from_validated_ints() {
        let mut o = opts();
        o.hostfwd_port_range = Some(PortRange {
            low: 1024,
            high: 65535,
        });
        let argv = build_argv(
            &spec(json!({
                "network": { "hostForwards": [
                    { "hostPort": 8022, "guestPort": 22, "proto": "tcp" },
                    { "hostPort": 15353, "guestPort": 53, "proto": "udp" }
                ] }
            })),
            &o,
        )
        .unwrap();
        assert_eq!(
            value_after(&argv, "-netdev"),
            "user,id=net0,hostfwd=tcp:127.0.0.1:8022-:22,hostfwd=udp:127.0.0.1:15353-:53"
        );
    }

    #[test]
    fn rejects_hostport_outside_range_naming_it() {
        let mut o = opts();
        o.hostfwd_port_range = Some(PortRange {
            low: 1024,
            high: 65535,
        });
        let e = build_argv(
            &spec(json!({ "network": { "hostForwards": [{ "hostPort": 80, "guestPort": 80 }] } })),
            &o,
        )
        .unwrap_err();
        assert!(e.0.contains("1024-65535"));
        assert!(e.0.contains("80"));
        assert!(e.0.contains("QMP_MCP_HOSTFWD_PORT_RANGE"));
        // Falls back to the default range when none configured.
        assert!(build_argv(
            &spec(json!({ "network": { "hostForwards": [{ "hostPort": 80, "guestPort": 80 }] } })),
            &opts()
        )
        .unwrap_err()
        .0
        .contains("1024-65535"));
    }

    #[test]
    fn rejects_tap_bridge_by_default_and_emits_when_enabled() {
        let e = build_argv(&spec(json!({ "network": { "mode": "tap" } })), &opts()).unwrap_err();
        assert!(e.0.contains("QMP_MCP_ALLOW_HOST_NET"));
        assert!(
            build_argv(&spec(json!({ "network": { "mode": "bridge" } })), &opts())
                .unwrap_err()
                .0
                .contains("QMP_MCP_ALLOW_HOST_NET")
        );

        let mut o = opts();
        o.allow_host_net = true;
        let argv = build_argv(&spec(json!({ "network": { "mode": "tap" } })), &o).unwrap();
        assert_eq!(value_after(&argv, "-netdev"), "tap,id=net0");
        let argv = build_argv(
            &spec(json!({ "network": { "mode": "bridge", "model": "e1000" } })),
            &o,
        )
        .unwrap();
        assert_eq!(value_after(&argv, "-netdev"), "bridge,id=net0");
        assert_eq!(value_after(&argv, "-device"), "e1000,netdev=net0");
    }

    #[test]
    fn rejects_hostforwards_with_non_user_mode() {
        let mut o = opts();
        o.allow_host_net = true;
        let e = build_argv(
            &spec(json!({
                "network": { "mode": "tap", "hostForwards": [{ "hostPort": 8022, "guestPort": 22 }] }
            })),
            &o,
        )
        .unwrap_err();
        assert!(e.0.contains("hostForwards are only valid for user-mode"));
        assert!(e.0.contains("mode \"user\""));
    }

    // --- resolveAccel -------------------------------------------------------

    #[test]
    fn resolve_accel_covers_auto_kvm_tcg() {
        assert_eq!(
            resolve_accel(AccelMode::Auto, || true).unwrap().accel,
            Accel::Kvm
        );
        assert_eq!(
            resolve_accel(AccelMode::Auto, || false).unwrap().accel,
            Accel::Tcg
        );
        assert_eq!(
            resolve_accel(AccelMode::Tcg, || true).unwrap().accel,
            Accel::Tcg
        );
        assert_eq!(
            resolve_accel(AccelMode::Kvm, || true).unwrap().accel,
            Accel::Kvm
        );
        let e = resolve_accel(AccelMode::Kvm, || false).unwrap_err();
        assert!(e.0.contains("/dev/kvm"));
    }

    // --- raspi / direct-kernel boot (issue #4) ------------------------------

    #[test]
    fn omits_cpu_smp_mem_for_a_fixed_hardware_raspi_board() {
        let argv = build_argv(
            &spec(json!({ "machine": "raspi3b", "network": { "mode": "none" } })),
            &opts(),
        )
        .unwrap();
        assert!(!argv.iter().any(|s| s == "-cpu"));
        assert!(!argv.iter().any(|s| s == "-smp"));
        assert!(!argv.iter().any(|s| s == "-m"));
        assert_eq!(value_after(&argv, "-machine"), "raspi3b,accel=tcg");
    }

    #[test]
    fn emits_kernel_dtb_append_and_if_sd_for_raspi() {
        let store = TempDir::new("hw-raspi");
        for f in ["kernel8.img", "merged.dtb", "dietpi.img"] {
            std::fs::write(store.path.join(f), b"").unwrap();
        }
        let mut o = opts();
        o.image_dir = Some(store.path.to_string_lossy().into_owned());

        let argv = build_argv(
            &spec(json!({
                "machine": "raspi3b",
                "display": "vnc",
                "kernel": "kernel8.img",
                "dtb": "merged.dtb",
                "appendCmdline": "console=tty1 root=/dev/mmcblk0p2 rootwait rw",
                "disks": [{ "image": "dietpi.img", "interface": "sd", "format": "raw" }],
                "network": { "model": "usb-net" }
            })),
            &o,
        )
        .unwrap();
        assert!(value_after(&argv, "-kernel").ends_with("/kernel8.img"));
        assert!(value_after(&argv, "-dtb").ends_with("/merged.dtb"));
        // -append is one token — spaces stay inside it.
        assert_eq!(
            value_after(&argv, "-append"),
            "console=tty1 root=/dev/mmcblk0p2 rootwait rw"
        );
        assert!(value_after(&argv, "-drive").contains("if=sd"));
        // The kernel block sits before -nodefaults.
        assert!(index_of(&argv, "-kernel") < index_of(&argv, "-nodefaults"));
    }

    #[test]
    fn keeps_cpu_smp_mem_for_a_non_raspi_direct_kernel_boot() {
        let store = TempDir::new("hw-virt");
        std::fs::write(store.path.join("vmlinuz"), b"").unwrap();
        let mut o = opts();
        o.image_dir = Some(store.path.to_string_lossy().into_owned());

        let argv = build_argv(
            &spec(json!({
                "machine": "virt", "cpu": "cortex-a72", "vcpus": 2, "memoryMb": 512,
                "kernel": "vmlinuz"
            })),
            &o,
        )
        .unwrap();
        assert_eq!(value_after(&argv, "-cpu"), "cortex-a72");
        assert_eq!(value_after(&argv, "-smp"), "2");
        assert_eq!(value_after(&argv, "-m"), "512");
        assert!(value_after(&argv, "-kernel").ends_with("/vmlinuz"));
    }

    #[test]
    fn kernel_without_image_store_fails_closed() {
        let e = build_argv(
            &spec(json!({ "machine": "raspi3b", "kernel": "kernel8.img", "network": { "mode": "none" } })),
            &opts(),
        )
        .unwrap_err();
        assert!(e.0.contains("QMP_MCP_IMAGE_DIR"));
    }

    #[test]
    fn rejects_traversing_kernel_reference_at_argv_time() {
        let store = TempDir::new("hw-raspi2");
        let mut o = opts();
        o.image_dir = Some(store.path.to_string_lossy().into_owned());
        let e = build_argv(
            &spec(json!({ "machine": "raspi3b", "kernel": "../vmlinuz", "network": { "mode": "none" } })),
            &o,
        )
        .unwrap_err();
        assert!(e.0.contains("kernel reference"));
    }

    #[test]
    fn network_none_emits_no_nic() {
        let argv = build_argv(
            &spec(json!({ "machine": "raspi3b", "network": { "mode": "none" } })),
            &opts(),
        )
        .unwrap();
        assert!(!argv.iter().any(|s| s == "-netdev"));
        assert!(!argv.iter().any(|s| s == "-device"));
    }

    #[test]
    fn emits_usb_net_for_raspi_and_refuses_pci_or_usb_mismatch() {
        // usb-net on a raspi (USB bus, no PCI) is emitted verbatim.
        let argv = build_argv(
            &spec(json!({ "machine": "raspi3b", "network": { "model": "usb-net" } })),
            &opts(),
        )
        .unwrap();
        assert_eq!(value_after(&argv, "-device"), "usb-net,netdev=net0");
        assert_eq!(value_after(&argv, "-netdev"), "user,id=net0");
        // A PCI NIC (the default) on a raspi is refused, naming usb-net / none.
        let e = build_argv(&spec(json!({ "machine": "raspi3b" })), &opts()).unwrap_err();
        assert!(e.0.contains("no PCI bus"));
        assert!(e.0.contains("usb-net") && e.0.contains("none"));
        // usb-net on a non-raspi machine (no USB bus) is refused.
        let e = build_argv(
            &spec(json!({ "machine": "q35", "network": { "model": "usb-net" } })),
            &opts(),
        )
        .unwrap_err();
        assert!(e.0.contains("usb-net") && e.0.contains("needs a USB bus"));
    }

    #[test]
    fn rejects_dtb_or_append_without_kernel_and_bad_cmdline() {
        let e =
            parse_hardware_spec(json!({ "machine": "raspi3b", "dtb": "merged.dtb" })).unwrap_err();
        assert!(e.0.contains("dtb requires kernel"));
        let e =
            parse_hardware_spec(json!({ "machine": "raspi3b", "appendCmdline": "console=tty1" }))
                .unwrap_err();
        assert!(e.0.contains("appendCmdline requires kernel"));
        // A control character (newline) in the cmdline is rejected.
        assert!(parse_hardware_spec(
            json!({ "machine": "raspi3b", "kernel": "kernel8.img", "appendCmdline": "a\nb" })
        )
        .is_err());
    }

    #[test]
    fn accepts_sd_as_a_disk_interface() {
        let s = spec(json!({ "disks": [{ "image": "dietpi.img", "interface": "sd" }] }));
        assert_eq!(s.disks[0].interface, DiskInterface::Sd);
    }

    /// A best-effort self-cleaning temp directory for the filesystem-touching argv
    /// tests (no external crate; mirrors the TS tests' `mkdtemp`).
    struct TempDir {
        path: std::path::PathBuf,
    }
    impl TempDir {
        fn new(prefix: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}
