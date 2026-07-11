/**
 * The Hardware Spec — the structured, validated description of an Instance's
 * hardware (see CONTEXT.md and ADR-0002). The agent never supplies raw QEMU
 * argv; it fills this spec and the server generates the `qemu-system-*` argv
 * from it via the pure {@link buildArgv} function.
 *
 * This module is deliberately side-effect free: validation is zod (v3) and argv
 * generation is a pure function of its inputs. The one impure concern — probing
 * `/dev/kvm` to resolve `accel: 'auto'` — is isolated in {@link resolveAccel},
 * which takes the probe as an injected predicate so it stays testable.
 */

import { accessSync, constants } from 'node:fs';
import { z } from 'zod';
import { DEFAULT_HOSTFWD_PORT_RANGE, type PortRange } from '../config.js';
import { IMAGE_FORMATS, ImageStoreError, resolveImagePath } from './image-store.js';
import { IsoStoreError, resolveIsoPath } from './iso-store.js';

/** Concrete accelerators QEMU can be launched with. */
export type Accel = 'kvm' | 'tcg';

/** Guest-visible disk controller a disk attaches through. `sd` is the SD/MMC
 * slot the `raspi*` boards boot from (`if=sd`); the image must be sized to a
 * power of two or QEMU rejects it at launch. */
export const DISK_INTERFACES = ['virtio', 'ide', 'scsi', 'sd'] as const;
export type DiskInterface = (typeof DISK_INTERFACES)[number];

/**
 * The requested accelerator. `auto` probes `/dev/kvm` and falls back to TCG;
 * `kvm` hard-fails when unavailable; `tcg` is always available (ADR-0008).
 */
export const ACCEL_MODES = ['auto', 'kvm', 'tcg'] as const;
export type AccelMode = (typeof ACCEL_MODES)[number];

/**
 * Guest Display modes (ADR-0010). `none` (the default) is today's fully headless
 * Instance — no display backend at all. `vnc` attaches a LOOPBACK-only VNC server
 * to the Guest's framebuffer (`-vnc 127.0.0.1:N`), the portable QEMU Display the
 * optional noVNC Viewer bridges over. The VNC password is NOT emitted into argv:
 * `password=on` only *requires* a password, which the Orchestrator sets after
 * launch over QMP (`set_password`), so no VNC secret ever reaches `ps`.
 */
export const DISPLAY_MODES = ['none', 'vnc'] as const;
export type DisplayMode = (typeof DISPLAY_MODES)[number];

/**
 * The Guest's display adapter — the device that produces a framebuffer for the VNC
 * Display to show. `-nodefaults` strips a machine's default adapter, and machines
 * like `virt`/`q35` have none built in, so WITHOUT one of these the VNC surface is
 * blank. `virtio-gpu` exposes a DRM device (`/dev/dri`) so Wayland/X desktops
 * render; `vga` is the legacy adapter; `ramfb` is a simple firmware framebuffer for
 * boards without PCI. `none` (default) is headless. The `raspi*` boards have a
 * BUILT-IN framebuffer and must stay `none`. Each value is a CLOSED enum, mapped to
 * a fixed `-device` name — no agent free-text reaches the argv.
 *
 * Picking between them: `virtio-gpu` shows nothing until the guest loads its DRM
 * driver, so it suits a system that boots straight into a DRM/Wayland desktop. `vga`
 * has a legacy text mode, so it renders the WHOLE path — bootloader, kernel console,
 * then X — which is what you want for a **live ISO** or anything where the boot menu
 * or early console must be visible (an ISO's bootloader can't draw on `virtio-gpu`).
 */
export const DISPLAY_DEVICES = ['none', 'virtio-gpu', 'vga', 'ramfb'] as const;
export type DisplayDevice = (typeof DISPLAY_DEVICES)[number];

/** Map a {@link DisplayDevice} to the QEMU `-device` model it emits (`none` → none). */
const DISPLAY_DEVICE_QEMU: Record<Exclude<DisplayDevice, 'none'>, string> = {
  'virtio-gpu': 'virtio-gpu-pci',
  vga: 'VGA',
  ramfb: 'ramfb',
};

/**
 * The single Instance's loopback VNC Display endpoint (ADR-0010). It is fixed
 * because exactly one Instance runs at a time: VNC display number N maps to TCP
 * port 5900+N, so the server both emits `-vnc 127.0.0.1:N` and points the Viewer's
 * proxy at 127.0.0.1:(5900+N) from this one source of truth. Bound to loopback so
 * the raw VNC port is never reachable off-host — the Viewer is its only client.
 */
export const VNC_LOOPBACK_HOST = '127.0.0.1';
export const VNC_DISPLAY_NUMBER = 0;
export const VNC_LOOPBACK_PORT = 5900 + VNC_DISPLAY_NUMBER;

/**
 * The minimal validated Hardware Spec for this slice. Unknown fields are
 * rejected (`.strict()`) so a typo fails closed rather than being silently
 * ignored. Every field has a default, so an empty spec is valid.
 */
/**
 * A single guest disk. The image is referenced by NAME within the Image Store
 * (ADR-0006), never by host path. `format` is part of the spec so it can be
 * pinned explicitly into the argv — QEMU's format auto-probing is a known
 * security footgun and is never relied upon.
 */
export const diskSchema = z
  .object({
    image: z
      .string()
      .min(1)
      .describe('Name of a disk image in the Image Store (a bare name, never a host path).'),
    interface: z
      .enum(DISK_INTERFACES)
      .default('virtio')
      .describe("Disk controller: 'virtio' (default), 'ide', or 'scsi'."),
    format: z
      .enum(IMAGE_FORMATS)
      .default('qcow2')
      .describe("Image format pinned explicitly into the argv: 'qcow2' (default) or 'raw'."),
    readonly: z.boolean().default(false).describe('Attach the disk read-only.'),
  })
  .strict();

/** A validated disk entry (all defaults resolved). */
export type Disk = z.infer<typeof diskSchema>;

/**
 * A CD-ROM drive backed by an ISO from the read-only ISO Store (ADR-0006). The
 * ISO is referenced by NAME within the ISO Store, never by host path — the same
 * containment boundary as disks, but against a separate, read-only directory.
 */
export const cdromSchema = z
  .object({
    iso: z
      .string()
      .min(1)
      .describe('Name of an ISO in the read-only ISO Store (a bare name, never a host path).'),
  })
  .strict();

/** A validated CD-ROM entry. */
export type Cdrom = z.infer<typeof cdromSchema>;

/**
 * The fixed virtio-9p mount tag for the shared host folder (ADR-0014). It is a
 * server-owned CONSTANT — never operator- or agent-supplied — so it can never inject
 * an extra `-device` property. The guest mounts it with
 * `mount -t 9p -o trans=virtio,version=9p2000.L share <mountpoint>`.
 */
export const SHARE_MOUNT_TAG = 'share';

/**
 * The fixed QEMU chardev id for the Serial Port's ring buffer (ADR-0015). A server-owned
 * CONSTANT — never operator- or agent-supplied — so it can never inject an extra property.
 * Mirrors the Rust `SERIAL_CHARDEV_ID`.
 */
export const SERIAL_CHARDEV_ID = 'serialbuf';

/**
 * Strict allowlist of guest NIC models the agent may pick (ADR-0009). The model
 * is emitted verbatim into `-device <model>,netdev=...`, so it is a CLOSED enum,
 * never a free string: a free string could carry a comma to inject extra
 * `-device` properties (e.g. a second device, an `addr=`), or an unknown model.
 * Keep this list short and boring — paravirtual `virtio-net-pci` plus two widely
 * emulated legacy NICs for guests without virtio drivers, and `usb-net` for boards
 * with a USB bus but no PCI (the `raspi*` machines). The three PCI models need a PCI
 * bus; `usb-net` needs a USB bus. {@link buildNetworkArgs} refuses a model the
 * machine can't host, so QEMU never aborts on an unattachable device.
 */
export const NIC_MODELS = ['virtio-net-pci', 'e1000', 'rtl8139', 'usb-net'] as const;
export type NicModel = (typeof NIC_MODELS)[number];

/** NIC models that attach to a PCI bus (everything except `usb-net`). */
const PCI_NIC_MODELS: ReadonlySet<NicModel> = new Set(['virtio-net-pci', 'e1000', 'rtl8139']);

/**
 * Guest networking backend. `user` is QEMU user-mode networking (SLiRP): NAT'd
 * outbound with the host network unexposed and inbound only via explicit
 * port-forwards — the safe, unprivileged default (ADR-0009). `tap`/`bridge` put
 * the guest on the host LAN and need host privileges, so they are env-gated off
 * (see `QMP_MCP_ALLOW_HOST_NET`). `none` attaches NO NIC at all — the escape hatch
 * for boards where the default PCI NIC can't attach (e.g. the `raspi*` machines).
 */
export const NETWORK_MODES = ['user', 'tap', 'bridge', 'none'] as const;
export type NetworkMode = (typeof NETWORK_MODES)[number];

/** Transport protocol for a user-mode port-forward. */
export const NET_PROTOCOLS = ['tcp', 'udp'] as const;
export type NetProtocol = (typeof NET_PROTOCOLS)[number];

/**
 * A single user-mode port-forward: expose guest `guestPort` on host `hostPort`.
 * Both ports are integers (1..65535) and `proto` is a closed enum, so the
 * generated `hostfwd=` value carries no agent free-text — it is built from
 * validated ints only. `hostPort` is additionally bounded to a configurable host
 * range at argv time (see `QMP_MCP_HOSTFWD_PORT_RANGE`).
 */
export const hostForwardSchema = z
  .object({
    hostPort: z
      .number()
      .int()
      .min(1)
      .max(65535)
      .describe('Host TCP/UDP port to bind (1-65535, and within QMP_MCP_HOSTFWD_PORT_RANGE).'),
    guestPort: z
      .number()
      .int()
      .min(1)
      .max(65535)
      .describe('Guest port the forward targets (1-65535).'),
    proto: z
      .enum(NET_PROTOCOLS)
      .default('tcp')
      .describe("Forward protocol: 'tcp' (default) or 'udp'."),
  })
  .strict();

/** A validated port-forward (proto default resolved). */
export type HostForward = z.infer<typeof hostForwardSchema>;

/**
 * The guest NIC. Defaults to a single user-mode (SLiRP) `virtio-net-pci` with no
 * port-forwards: working outbound connectivity, zero host exposure, no privileges
 * (ADR-0009). `model` and `mode` are closed enums (never free strings) so neither
 * can inject extra `-device`/`-netdev` options. `hostForwards` apply only to
 * `user` mode.
 */
export const networkSchema = z
  .object({
    mode: z
      .enum(NETWORK_MODES)
      .default('user')
      .describe(
        "Networking backend: 'user' (default, SLiRP NAT), 'tap'/'bridge' (host networking, " +
          "requires QMP_MCP_ALLOW_HOST_NET=true), or 'none' (no NIC — required for the raspi* " +
          'boards, which have no PCI bus for the default NIC).',
      ),
    model: z
      .enum(NIC_MODELS)
      .default('virtio-net-pci')
      .describe(
        "Guest NIC model from a fixed allowlist: 'virtio-net-pci' (default), 'e1000', 'rtl8139' " +
          "(all PCI), or 'usb-net' (USB — for boards with a USB bus but no PCI, e.g. raspi*).",
      ),
    hostForwards: z
      .array(hostForwardSchema)
      .default([])
      .describe(
        'User-mode inbound port-forwards (host port -> guest port). Only valid for mode "user".',
      ),
  })
  .strict();

/** A validated guest NIC (all defaults resolved). */
export type Network = z.infer<typeof networkSchema>;

/**
 * Strict allowlist for the `-boot order=` value: one or more of QEMU's legal boot
 * drive letters — `a`/`b` (floppy), `c` (first disk), `d` (first CD-ROM),
 * `n`-`p` (network). This is the security-critical rule for boot: it admits ONLY
 * those letters, so a value can carry no comma, `=`, space, or other QemuOpts
 * separator that would let it inject a second `-boot` option (e.g. `menu=on`,
 * `reboot-timeout=`) or split off an extra argv token. The server always emits it
 * as the single `order=<letters>` form.
 */
const VALID_BOOT_ORDER = /^[a-dnp]+$/;

const bootOrderMessage =
  'boot must match ^[a-dnp]+ — one or more QEMU boot drive letters (a/b floppy, c disk, ' +
  "d cd-rom, n-p network), with no comma, '=', or spaces (these could inject extra -boot options).";

/**
 * Conservative charset for `-machine`/`-cpu` model names: a leading alphanumeric
 * then alphanumerics, dot, underscore, plus, or hyphen. It excludes the comma,
 * space, and `=` that QEMU treats as QemuOpts property separators — a comma in
 * `machine` would otherwise inject extra `-machine` properties. Raw multi-property
 * machine/cpu strings are exactly the raw-args this design forbids.
 */
const VALID_MACHINE_CPU = /^[A-Za-z0-9][A-Za-z0-9._+-]*$/;

const machineCpuMessage =
  'must match ^[A-Za-z0-9][A-Za-z0-9._+-]* — letters, digits, dot, underscore, plus, ' +
  "or hyphen, with no leading hyphen and no comma, space, or '=' (these could inject " +
  'QEMU -machine/-cpu properties).';

/**
 * QEMU Raspberry Pi machine types. These are Single-Board-Computer boards with
 * FIXED hardware — the CPU model, core count and RAM are baked into the machine —
 * so the argv generator OMITS `-cpu`/`-smp`/`-m` for them (QEMU rejects those,
 * e.g. "Invalid RAM size, should be 1024 MB"). They also do not read a bootloader
 * from the SD card, so they must be direct-kernel-booted (`kernel`/`dtb`).
 * `raspi4b` needs QEMU >= 9.2; the rest are in QEMU >= 7.2.
 */
export const RASPI_MACHINES = new Set([
  'raspi0',
  'raspi1ap',
  'raspi2b',
  'raspi3ap',
  'raspi3b',
  'raspi4b',
]);

/** True when `machine` is a fixed-hardware Raspberry Pi board (see {@link RASPI_MACHINES}). */
export function isRaspiMachine(machine: string): boolean {
  return RASPI_MACHINES.has(machine);
}

/**
 * QEMU guest architecture we can auto-select a `qemu-system-*` binary for; the value
 * is the binary suffix (`qemu-system-<arch>`). Every ARM machine — the raspi* boards
 * (even the 32-bit `raspi0`/`raspi1ap`/`raspi2b`) and `virt`/`sbsa-ref` — maps to
 * `aarch64`, because `qemu-system-aarch64` is a superset emulator that hosts them all.
 */
export type QemuArch = 'x86_64' | 'aarch64';

/**
 * Non-raspi machines that imply an aarch64 guest. The raspi* boards are covered by
 * {@link isRaspiMachine}; `q35`/`pc` and any unrecognized machine fall through to x86_64.
 */
const AARCH64_MACHINES = new Set(['virt', 'sbsa-ref']);

/**
 * Guest architecture implied by a `machine` (issue #18). raspi* and `virt`/`sbsa-ref`
 * are aarch64; every other machine — `q35`/`pc`/`microvm` and any unrecognized name —
 * is x86_64, the architecture of {@link DEFAULT_QEMU_BINARY}. Mapping unknown machines
 * to x86_64 makes binary derivation degrade to the historical default, so an exotic
 * machine still works once the operator points `QMP_MCP_QEMU_BINARY` at its emulator.
 * Mirrors the Rust `machine_arch`.
 */
export function machineArch(machine: string): QemuArch {
  return isRaspiMachine(machine) || AARCH64_MACHINES.has(machine) ? 'aarch64' : 'x86_64';
}

/**
 * The `qemu-system-*` binary implied by a `machine` (issue #18): `q35` → x86_64,
 * `virt`/raspi* → aarch64. This lets the per-instance `machine` pick the emulator, so
 * no `QMP_MCP_QEMU_BINARY` flip + container recreate is needed to switch guest
 * architectures. An explicitly-set `QMP_MCP_QEMU_BINARY` still overrides it.
 */
export function qemuBinaryForMachine(machine: string): string {
  return `qemu-system-${machineArch(machine)}`;
}

/**
 * This host's QEMU architecture, for the `accel: auto` guest/host match. Node's
 * `process.arch` uses `x64`/`arm64`; normalize to the `qemu-system-*` suffix
 * (`x86_64`/`aarch64`). An unrecognized host arch is returned verbatim — it cannot
 * equal any guest arch, so `accel: auto` safely falls back to TCG. Mirrors the Rust
 * `host_qemu_arch` (which reads `std::env::consts::ARCH`, already in this form).
 */
export function hostQemuArch(arch: string = process.arch): string {
  if (arch === 'x64') return 'x86_64';
  if (arch === 'arm64') return 'aarch64';
  return arch;
}

/**
 * The guest architecture a `qemu-system-*` binary emulates, for the `accel: auto`
 * guest/host match (issue #18). Parses the `qemu-system-<arch>` suffix after stripping
 * any absolute-path directory — so it reflects the ACTUAL binary being launched, not the
 * machine (a `QMP_MCP_QEMU_BINARY` override can differ from the machine's arch). A name
 * that doesn't fit that shape returns its basename, which won't equal any host arch, so
 * `accel: auto` safely falls back to TCG for an unrecognized override. Mirrors the Rust
 * `qemu_arch_of_binary`.
 */
export function qemuArchOfBinary(binary: string): string {
  const base = binary.slice(binary.lastIndexOf('/') + 1);
  const prefix = 'qemu-system-';
  return base.startsWith(prefix) ? base.slice(prefix.length) : base;
}

/**
 * The kernel command line (`-append`) is emitted as a SINGLE argv token, so
 * spaces/`=`/`,` are safe (they cannot split off another argv element or inject a
 * QemuOpts property). The one thing we forbid is control characters (newlines,
 * NULs), which have no place on a cmdline and would corrupt logs.
 */
// biome-ignore lint/suspicious/noControlCharactersInRegex: intentionally forbids control chars.
const VALID_APPEND_CMDLINE = /^[^\x00-\x1f\x7f]+$/;

const appendCmdlineMessage =
  'appendCmdline must be a single line of printable characters (no control characters or ' +
  'newlines). It is emitted as one -append token, so spaces are fine.';

export const hardwareSpecSchema = z
  .object({
    machine: z
      .string()
      .regex(VALID_MACHINE_CPU, `machine ${machineCpuMessage}`)
      .default('q35')
      .describe('QEMU machine type, e.g. "q35" (default) or "pc".'),
    cpu: z
      .string()
      .regex(VALID_MACHINE_CPU, `cpu ${machineCpuMessage}`)
      .default('max')
      .describe('CPU model passed to -cpu, e.g. "max" (default) or "host".'),
    vcpus: z.number().int().min(1).max(255).default(1).describe('Number of virtual CPUs (1-255).'),
    memoryMb: z
      .number()
      .int()
      .min(1)
      .max(1_048_576)
      .default(256)
      .describe('Guest RAM in MiB (1-1048576).'),
    accel: z
      .enum(ACCEL_MODES)
      .default('auto')
      .describe(
        "Accelerator: 'auto' probes /dev/kvm and falls back to TCG, 'kvm' requires /dev/kvm, 'tcg' is software emulation.",
      ),
    display: z
      .enum(DISPLAY_MODES)
      .default('none')
      .describe(
        "Guest Display: 'none' (default, fully headless) or 'vnc' (a loopback-only VNC " +
          'Display the optional noVNC Viewer bridges over — requires QMP_MCP_VIEWER_PASSWORD).',
      ),
    displayDevice: z
      .enum(DISPLAY_DEVICES)
      .default('none')
      .describe(
        "Guest display adapter that produces the framebuffer the VNC Display shows: 'none' " +
          "(default, headless), 'virtio-gpu' (DRM/Wayland-capable, for desktops), 'vga', or " +
          "'ramfb'. Machines like virt/q35 have no built-in adapter, so display:vnc needs one of " +
          'these to show anything; the raspi* boards have a built-in framebuffer and must stay none.',
      ),
    disks: z
      .array(diskSchema)
      .default([])
      .describe('Guest disks, each referencing an image by name in the Image Store.'),
    cdrom: cdromSchema
      .optional()
      .describe('Optional CD-ROM drive backed by an ISO (by name) from the read-only ISO Store.'),
    share: z
      .boolean()
      .default(false)
      .describe(
        'Share the operator-configured host folder (QMP_MCP_HOST_SHARE_DIR) into the guest via ' +
          'virtio-9p. Boolean opt-in only — the host path is never agent-supplied. Read-only unless ' +
          'the operator set QMP_MCP_ALLOW_SHARE_WRITE. Mount it in the guest with the command ' +
          'get_share reports. Not supported on raspi* boards (no PCI bus).',
      ),
    serial: z
      .boolean()
      .default(false)
      .describe(
        'Attach the Guest Serial Port and capture its output (ADR-0015). Boolean opt-in only; ' +
          'the ring-buffer size is the operator QMP_MCP_SERIAL_BUFFER_BYTES. Read it with read_serial ' +
          '(each read drains the buffer); get_serial reports the expected guest console device.',
      ),
    boot: z
      .string()
      .regex(VALID_BOOT_ORDER, bootOrderMessage)
      .optional()
      .describe(
        "Optional boot order as QEMU drive letters, e.g. 'd' (CD-ROM first) or 'dc'. Emitted as -boot order=.",
      ),
    kernel: z
      .string()
      .min(1)
      .optional()
      .describe(
        'Optional external kernel image to direct-boot, by NAME in the Image Store (emitted as ' +
          '-kernel). REQUIRED for the raspi* boards, which do not read a bootloader from the SD ' +
          'card; also usable for any direct-kernel boot (e.g. the "virt" machine).',
      ),
    initrd: z
      .string()
      .min(1)
      .optional()
      .describe(
        'Optional initramfs to pass the kernel, by NAME in the Image Store (emitted as -initrd). ' +
          'Requires kernel. Needed to direct-kernel-boot most distros (kernel + initrd + rootfs).',
      ),
    dtb: z
      .string()
      .min(1)
      .optional()
      .describe(
        'Optional device tree blob to pass the kernel, by NAME in the Image Store (emitted as ' +
          '-dtb). Requires kernel. For a raspi, extract it from the SD image (and merge the ' +
          'disable-bt overlay on Pi 3 so the console attaches to the PL011 UART).',
      ),
    appendCmdline: z
      .string()
      .max(2048)
      .regex(VALID_APPEND_CMDLINE, appendCmdlineMessage)
      .optional()
      .describe(
        'Optional kernel command line, emitted as a single -append token. Requires kernel. For a ' +
          'raspi framebuffer console visible over the noVNC Viewer, include "console=tty1".',
      ),
    network: networkSchema
      .default({})
      .describe('Guest NIC; defaults to user-mode (SLiRP) networking with no port-forwards.'),
    extraArgs: z
      .array(z.string())
      .optional()
      .describe(
        'Raw QEMU arguments appended verbatim to the generated argv. Opt-in escape hatch ' +
          '(ADR-0002): REFUSED unless QMP_MCP_ALLOW_RAW_ARGS=true. Host-compromise-equivalent — ' +
          'intended only for trusted single-tenant hosts; omit it in the default safe-by-construction mode.',
      ),
  })
  .strict();

/** A fully-validated Hardware Spec (all defaults resolved). */
export type HardwareSpec = z.infer<typeof hardwareSpecSchema>;

/**
 * Raised when a candidate Hardware Spec fails validation. The message names the
 * offending field(s) and the constraint, so the caller can fix it.
 */
export class HardwareSpecError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'HardwareSpecError';
  }
}

/**
 * Validate an untrusted candidate spec, returning a fully-defaulted
 * {@link HardwareSpec}. Throws an actionable {@link HardwareSpecError} naming
 * the offending field on failure.
 */
export function parseHardwareSpec(candidate: unknown): HardwareSpec {
  const result = hardwareSpecSchema.safeParse(candidate);
  if (result.success) {
    // Cross-field rules kept out of the zod object so `hardwareSpecSchema` stays a
    // plain ZodObject (create_instance reuses it as the MCP tool inputSchema).
    // -dtb/-append are meaningless without a -kernel to hand them to; fail closed
    // rather than emit an ineffective flag.
    const spec = result.data;
    if (spec.dtb !== undefined && spec.kernel === undefined) {
      throw new HardwareSpecError(
        'Invalid Hardware Spec — dtb: dtb requires kernel (a device tree is only passed to a ' +
          'direct-booted kernel). Set kernel, or remove dtb.',
      );
    }
    if (spec.appendCmdline !== undefined && spec.kernel === undefined) {
      throw new HardwareSpecError(
        'Invalid Hardware Spec — appendCmdline: appendCmdline requires kernel (a command line is ' +
          'only passed to a direct-booted kernel). Set kernel, or remove appendCmdline.',
      );
    }
    if (spec.initrd !== undefined && spec.kernel === undefined) {
      throw new HardwareSpecError(
        'Invalid Hardware Spec — initrd: initrd requires kernel (an initramfs is only passed to a ' +
          'direct-booted kernel). Set kernel, or remove initrd.',
      );
    }
    return spec;
  }
  const detail = result.error.issues
    .map((issue) => {
      const path = issue.path.length > 0 ? issue.path.join('.') : '(spec root)';
      return `${path}: ${issue.message}`;
    })
    .join('; ');
  throw new HardwareSpecError(`Invalid Hardware Spec — ${detail}.`);
}

/** The outcome of resolving the requested accelerator to a concrete one. */
export interface AccelResolution {
  /** The accelerator QEMU will actually be launched with. */
  accel: Accel;
  /** The mode the caller requested (for reporting). */
  requested: AccelMode;
  /** Human-readable reason for the choice, suitable for reporting to the agent. */
  reason: string;
}

/**
 * Raised when `accel: 'kvm'` is forced but `/dev/kvm` is not accessible.
 */
export class AccelError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'AccelError';
  }
}

/**
 * Default `/dev/kvm` probe: KVM is usable when the device exists and is both
 * readable and writable by this (unprivileged) process. Any failure — missing
 * device, permission denied — reads as "unavailable".
 */
export function probeKvm(): boolean {
  try {
    accessSync('/dev/kvm', constants.R_OK | constants.W_OK);
    return true;
  } catch {
    return false;
  }
}

/**
 * Resolve the requested accelerator mode to a concrete accelerator, reporting
 * which was chosen and why (ADR-0008). `kvmAvailable` is injected so this is
 * testable without a real `/dev/kvm`.
 *
 * - `tcg`  → always TCG.
 * - `kvm`  → KVM, or {@link AccelError} when `/dev/kvm` is inaccessible.
 * - `auto` → KVM only when it is actually viable: the guest arch (of the launched
 *   binary) matches the host arch, the machine is not a fixed-CPU `raspi*` board, and
 *   `/dev/kvm` is available; otherwise TCG. KVM cannot cross architectures and cannot
 *   virtualize the raspi boards' baked CPU, so those resolve to TCG (issue #18).
 */
export function resolveAccel(
  requested: AccelMode,
  guestArch: string,
  hostArch: string,
  machine: string,
  kvmAvailable: () => boolean = probeKvm,
): AccelResolution {
  switch (requested) {
    case 'tcg':
      return {
        accel: 'tcg',
        requested,
        reason: 'accel=tcg requested; using TCG software emulation.',
      };
    case 'kvm':
      if (!kvmAvailable()) {
        throw new AccelError(
          'accel=kvm was requested but /dev/kvm is not accessible. Grant the container/user access to ' +
            '/dev/kvm (add it as a device and join the kvm group), or use accel=auto or accel=tcg.',
        );
      }
      return { accel: 'kvm', requested, reason: 'accel=kvm requested; /dev/kvm is accessible.' };
    default:
      // accel=auto: KVM cannot cross architectures, so it is only viable when the
      // launched binary's arch matches the host. Otherwise — e.g. an aarch64 `virt`
      // guest on an x86_64 host — qemu rejects KVM ("invalid accelerator kvm"), so fall
      // back to TCG before probing /dev/kvm (issue #18).
      if (guestArch !== hostArch) {
        return {
          accel: 'tcg',
          requested,
          reason: `accel=auto: guest arch ${guestArch} does not match host arch ${hostArch}; KVM cannot cross architectures, so using TCG.`,
        };
      }
      // The raspi boards bake a fixed CPU (arm1176/cortex-a7/a53/a72) that KVM can't
      // virtualize (KVM only runs the host CPU), so even on a matching aarch64 host they
      // must use TCG. Set accel=kvm explicitly to override (it will then hard-fail).
      if (isRaspiMachine(machine)) {
        return {
          accel: 'tcg',
          requested,
          reason: `accel=auto: the ${machine} board has a fixed CPU that KVM cannot virtualize; using TCG.`,
        };
      }
      return kvmAvailable()
        ? { accel: 'kvm', requested, reason: 'accel=auto: /dev/kvm is accessible, using KVM.' }
        : {
            accel: 'tcg',
            requested,
            reason: 'accel=auto: /dev/kvm is not accessible, falling back to TCG.',
          };
  }
}

/** Inputs for {@link buildArgv} that are not part of the Hardware Spec itself. */
export interface ArgvOptions {
  /** The concrete accelerator (already resolved from {@link resolveAccel}). */
  accel: Accel;
  /** Absolute path of the server-managed QMP UNIX socket. */
  qmpSocketPath: string;
  /**
   * Absolute path of the Image Store directory (ADR-0006). Required only when the
   * spec has disks; each disk's image name is resolved against it. Omitting it
   * for a spec that has disks fails closed.
   */
  imageDir?: string;
  /**
   * Absolute path of the read-only ISO Store directory (ADR-0006). Required only
   * when the spec has a cdrom; the ISO name is resolved against it. Omitting it
   * for a spec that has a cdrom fails closed naming `QMP_MCP_ISO_DIR`.
   */
  isoDir?: string;
  /**
   * Host directory shared into the guest via virtio-9p (`QMP_MCP_HOST_SHARE_DIR`,
   * ADR-0014). Required only when the spec sets `share: true`; a `share` with no host
   * dir configured fails closed naming `QMP_MCP_HOST_SHARE_DIR`.
   */
  hostShareDir?: string;
  /**
   * Whether the shared folder is mounted read-only. Fail-closed: omitted or `true`
   * ⇒ `readonly=on`; only `false` (operator set `QMP_MCP_ALLOW_SHARE_WRITE`) mounts
   * it read-write. The agent can never make it writable.
   */
  shareReadonly?: boolean;
  /**
   * Ring-buffer size (bytes) for the Serial Port's QEMU `ringbuf` chardev when the spec sets
   * `serial: true` (`QMP_MCP_SERIAL_BUFFER_BYTES`, ADR-0015; power-of-two).
   */
  serialBufferBytes: number;
  /**
   * Inclusive host-port range a user-mode port-forward's `hostPort` must fall
   * within (`QMP_MCP_HOSTFWD_PORT_RANGE`). A forward outside it is rejected
   * naming the range. Defaults to {@link DEFAULT_HOSTFWD_PORT_RANGE} when omitted
   * (ADR-0009).
   */
  hostfwdPortRange?: PortRange;
  /**
   * Whether host-level (`tap`/`bridge`) networking is permitted
   * (`QMP_MCP_ALLOW_HOST_NET`). Defaults to false: a `tap`/`bridge` spec is
   * refused with an actionable error unless this is explicitly enabled (ADR-0009).
   */
  allowHostNet?: boolean;
  /**
   * Hard cap, in MiB, on the spec's `memoryMb` (`QMP_MCP_MAX_MEMORY_MB`). The cap
   * is env-configurable, so it is INJECTED here rather than baked into the schema
   * as a static zod max. A spec over the cap is rejected at argv time (before qemu
   * is spawned) naming the cap and the requested-vs-allowed values. Omitting it
   * skips the check — the Orchestrator always injects it, so create_instance is
   * fail-closed (issue #9).
   */
  maxMemoryMb?: number;
  /**
   * Hard cap on the spec's `vcpus` (`QMP_MCP_MAX_VCPUS`). Injected for the same
   * reason as {@link maxMemoryMb}; a spec over the cap is rejected at argv time
   * naming the cap and the requested-vs-allowed values. Omitting it skips the
   * check (issue #9).
   */
  maxVcpus?: number;
  /**
   * Whether the raw-args escape hatch is enabled (`QMP_MCP_ALLOW_RAW_ARGS`). When
   * true, a spec's `extraArgs` are appended verbatim to the generated argv; when
   * false (the default) a spec carrying `extraArgs` is REFUSED with an actionable
   * {@link HardwareSpecError} naming the flag, rather than silently dropped — raw
   * args are host-compromise-equivalent, so this fails closed (ADR-0002). The
   * Orchestrator always injects the env-resolved value.
   */
  allowRawArgs?: boolean;
}

/**
 * Comma-escape a value interpolated into a QemuOpts property string
 * (`-drive`/`-machine`), where a literal comma must be doubled (`,,`). This is
 * defense-in-depth: the validators already reject commas in agent-controlled
 * names, but the resolved file path is host/Store-derived, so escaping it here
 * guarantees a comma in the Image Store path can never split off an extra
 * property no matter what the path contains.
 */
function escapeQemuOptsValue(value: string): string {
  return value.replaceAll(',', ',,');
}

/**
 * Resolve a disk's image name to a safe in-Store path and render a `-drive`
 * argument pair. The format is taken from the validated spec and written as an
 * explicit `format=` — QEMU's auto-probing is never relied upon. Any out-of-store,
 * absolute, traversal, or symlink-escape reference is rejected here (argv time)
 * as a {@link HardwareSpecError}.
 */
function buildDriveArgs(disk: Disk, imageDir: string | undefined): [string, string] {
  if (imageDir === undefined || imageDir.trim() === '') {
    throw new HardwareSpecError(
      `Disk "${disk.image}" was requested but the Image Store directory is not configured. ` +
        `Set QMP_MCP_IMAGE_DIR to the Image Store path.`,
    );
  }
  let path: string;
  try {
    path = resolveImagePath(disk.image, imageDir);
  } catch (err) {
    const detail = err instanceof ImageStoreError ? err.message : String(err);
    throw new HardwareSpecError(`Invalid disk reference: ${detail}`);
  }
  // Explicit format= defeats QEMU format auto-probing (a known security footgun).
  // The path is comma-escaped so it cannot inject extra -drive properties; format
  // and interface are closed enums, so they carry no comma to escape.
  const parts = [
    `file=${escapeQemuOptsValue(path)}`,
    `format=${disk.format}`,
    `if=${disk.interface}`,
    'media=disk',
  ];
  if (disk.readonly) parts.push('readonly=on');
  return ['-drive', parts.join(',')];
}

/**
 * Resolve a CD-ROM's ISO name to a safe in-Store path and render a read-only
 * `-drive ...,media=cdrom,readonly=on` argument pair. The ISO is resolved against
 * the SEPARATE read-only ISO Store; `format=raw` is pinned explicitly (an ISO is
 * a raw image — never rely on QEMU's auto-probing), and the path is comma-escaped
 * so it cannot inject extra `-drive` properties. Any out-of-store, absolute,
 * traversal, or symlink-escape reference is rejected here (argv time) as a
 * {@link HardwareSpecError}; a cdrom with no ISO Store configured fails closed.
 */
function buildCdromArgs(cdrom: Cdrom, isoDir: string | undefined): [string, string] {
  if (isoDir === undefined || isoDir.trim() === '') {
    throw new HardwareSpecError(
      `A CD-ROM with ISO "${cdrom.iso}" was requested but the ISO Store directory is not configured. ` +
        `Set QMP_MCP_ISO_DIR to the read-only ISO Store path.`,
    );
  }
  let path: string;
  try {
    path = resolveIsoPath(cdrom.iso, isoDir);
  } catch (err) {
    const detail = err instanceof IsoStoreError ? err.message : String(err);
    throw new HardwareSpecError(`Invalid ISO reference: ${detail}`);
  }
  const parts = [
    `file=${escapeQemuOptsValue(path)}`,
    'media=cdrom',
    'readonly=on',
    // Explicit format= defeats QEMU format auto-probing (a known security footgun);
    // an ISO is a raw image.
    'format=raw',
  ];
  return ['-drive', parts.join(',')];
}

/**
 * Render the virtio-9p folder-share argument pair for a spec that opted in with
 * `share: true` (ADR-0014). The host directory is the OPERATOR's
 * `QMP_MCP_HOST_SHARE_DIR` (never agent-supplied) and is comma-escaped so a comma in
 * the path can never split off an extra `-fsdev` property. `security_model=mapped-xattr`
 * stores guest uid/gid/mode in host xattrs, so the guest cannot create host setuid or
 * device nodes and symlinks stay inside the shared tree. Read-only unless the operator
 * enabled writes (`shareReadonly === false`). virtio-9p is a PCI device, so it is
 * refused fail-closed on the raspi* boards (no PCI bus), exactly like a PCI NIC; and a
 * `share` with no host dir configured fails closed naming `QMP_MCP_HOST_SHARE_DIR`.
 */
function buildShareArgs(options: ArgvOptions, raspi: boolean, machine: string): string[] {
  const hostDir = options.hostShareDir;
  if (hostDir === undefined || hostDir.trim() === '') {
    throw new HardwareSpecError(
      'Folder sharing (share: true) was requested but no host share directory is configured. ' +
        'Set QMP_MCP_HOST_SHARE_DIR to the host directory to share into the guest.',
    );
  }
  if (raspi) {
    throw new HardwareSpecError(
      `Folder sharing cannot be attached to the raspi* board "${machine}": virtio-9p needs a PCI bus, ` +
        'which these boards do not have. Use a q35/pc/virt machine to share a folder.',
    );
  }
  const fsdev = [
    'local',
    'id=fsdev0',
    `path=${escapeQemuOptsValue(hostDir)}`,
    'security_model=mapped-xattr',
  ];
  // Fail-closed: read-only unless the operator explicitly enabled writes.
  if (options.shareReadonly !== false) fsdev.push('readonly=on');
  return [
    '-fsdev',
    fsdev.join(','),
    '-device',
    `virtio-9p-pci,fsdev=fsdev0,mount_tag=${SHARE_MOUNT_TAG}`,
  ];
}

/**
 * Render the Serial Port argument pair for a spec that opted in with `serial: true`
 * (ADR-0015, ringbuf backend): a QEMU in-tree `ringbuf` chardev of the operator's configured
 * size, bound to the machine's first serial port with `-serial chardev:`. Board-agnostic — no
 * per-UART device name — and the explicit `-serial` redirect wins over the `-nographic` console
 * mux. Mirrors the Rust `build_serial_args`.
 */
function buildSerialArgs(options: ArgvOptions): string[] {
  return [
    '-chardev',
    `ringbuf,id=${SERIAL_CHARDEV_ID},size=${options.serialBufferBytes}`,
    '-serial',
    `chardev:${SERIAL_CHARDEV_ID}`,
  ];
}

/**
 * Resolve a boot artifact (kernel or dtb) name to a safe in-Store path for
 * `-kernel`/`-dtb`. Like a disk, it is referenced by NAME in the Image Store and
 * resolved through the same containment boundary (any traversal/out-of-store name
 * is rejected here at argv time). It is emitted as its own standalone argv token
 * (not a QemuOpts property), so no comma-escaping is needed. A boot artifact with
 * no Image Store configured fails closed naming `QMP_MCP_IMAGE_DIR`.
 */
function resolveBootArtifact(
  name: string,
  imageDir: string | undefined,
  kind: 'kernel' | 'dtb' | 'initrd',
): string {
  if (imageDir === undefined || imageDir.trim() === '') {
    throw new HardwareSpecError(
      `A ${kind} ("${name}") was requested but the Image Store directory is not configured. ` +
        `Set QMP_MCP_IMAGE_DIR to the Image Store path.`,
    );
  }
  try {
    return resolveImagePath(name, imageDir);
  } catch (err) {
    const detail = err instanceof ImageStoreError ? err.message : String(err);
    throw new HardwareSpecError(`Invalid ${kind} reference: ${detail}`);
  }
}

/**
 * Fixed id tying a `-netdev` backend to its `-device` NIC. A single NIC is
 * supported per Instance in this slice, so a constant id is sufficient — and
 * being a constant (not agent-controlled) it can carry no injected option.
 */
const NETDEV_ID = 'net0';

/**
 * Render the `-netdev`/`-device` pair for the guest NIC (ADR-0009).
 *
 * - `user` mode emits `-netdev user,id=net0[,hostfwd=...]` (SLiRP NAT) plus
 *   `-device <model>,netdev=net0`. Each `hostForwards` entry becomes a
 *   `hostfwd=<proto>:127.0.0.1:<hostPort>-:<guestPort>` built from validated
 *   ints/enums only. The host address is pinned to `127.0.0.1` (loopback) so the
 *   forward is reachable only from the host itself, never the host LAN — an empty
 *   host-address field would bind `0.0.0.0` and contradict ADR-0009's zero host
 *   exposure default. `hostPort` is bounded to `hostfwdPortRange` here (argv
 *   time), and a port outside it is rejected with a {@link HardwareSpecError}
 *   NAMING the range and the offending value.
 * - `tap`/`bridge` mode is host networking and is REFUSED unless `allowHostNet`
 *   is true; when allowed it emits `-netdev <mode>,id=net0` plus the same
 *   `-device` (the operator is responsible for host privileges/configuration —
 *   the server never configures the host). `hostForwards` are a user-mode-only
 *   concept, so supplying them with `tap`/`bridge` is REFUSED rather than
 *   silently ignored.
 * - `none` mode attaches NO NIC (no `-netdev`/`-device`) — the escape hatch for
 *   boards whose bus can't host any allowlisted NIC (e.g. the `raspi*` machines
 *   have no PCI bus for the PCI models).
 *
 * The NIC `model`/`mode` are closed enums, so no agent free-text reaches the option
 * string; the model is comma-escaped anyway as defense-in-depth. `model` is checked
 * against the machine's bus (PCI models need a PCI bus, `usb-net` needs a USB bus),
 * so the server never emits a `-device` QEMU would reject with "No 'PCI'/'USB' bus".
 */
function buildNetworkArgs(network: Network, machine: string, options: ArgvOptions): string[] {
  // hostForwards are a user-mode (SLiRP) concept only; QEMU has nowhere to apply
  // them under tap/bridge/none. Refuse rather than silently drop them so the agent
  // is told its forwards would not take effect (fail-closed, ADR-0009).
  if (network.mode !== 'user' && network.hostForwards.length > 0) {
    throw new HardwareSpecError(
      `network.hostForwards are only valid for user-mode networking (mode "user"), but mode is ` +
        `"${network.mode}". Remove hostForwards, or use mode "user".`,
    );
  }

  // 'none' — attach no NIC. The escape hatch for boards where no allowlisted NIC
  // can attach; emit nothing.
  if (network.mode === 'none') return [];

  // NIC model must match a bus the machine actually has, or QEMU aborts at launch
  // ("No 'PCI' bus found for device ..."). Fail closed here with an actionable
  // message instead. The raspi* boards have a USB bus but no PCI bus; every other
  // machine we target has PCI (via -nodefaults) but no built-in USB controller.
  const raspi = isRaspiMachine(machine);
  if (network.model === 'usb-net' && !raspi) {
    throw new HardwareSpecError(
      `network.model "usb-net" needs a USB bus, but machine "${machine}" has none (only the ` +
        `raspi* boards expose a built-in USB bus). Use a PCI NIC (virtio-net-pci/e1000/rtl8139), ` +
        `or network.mode "none".`,
    );
  }
  if (PCI_NIC_MODELS.has(network.model) && raspi) {
    throw new HardwareSpecError(
      `network.model "${network.model}" is a PCI NIC, but the raspi* board "${machine}" has no PCI ` +
        `bus. Use network.model "usb-net", or network.mode "none" for no networking.`,
    );
  }

  // model is an allowlisted enum; escape defensively so it can never inject an
  // extra -device property no matter how the allowlist evolves.
  const device = `${escapeQemuOptsValue(network.model)},netdev=${NETDEV_ID}`;

  if (network.mode === 'user') {
    const range = options.hostfwdPortRange ?? DEFAULT_HOSTFWD_PORT_RANGE;
    const netdevParts = ['user', `id=${NETDEV_ID}`];
    for (const fwd of network.hostForwards) {
      if (fwd.hostPort < range.low || fwd.hostPort > range.high) {
        throw new HardwareSpecError(
          `Host port-forward hostPort ${fwd.hostPort} is outside the allowed host-port range ` +
            `${range.low}-${range.high} (QMP_MCP_HOSTFWD_PORT_RANGE). Choose a hostPort within ` +
            `${range.low}-${range.high}, or widen the range via QMP_MCP_HOSTFWD_PORT_RANGE.`,
        );
      }
      // proto is an enum and both ports are validated ints — no free-text here.
      // The host address is pinned to 127.0.0.1 (loopback) so the forward is
      // reachable only from the host itself, not the host LAN: an empty host
      // address would bind 0.0.0.0 and break ADR-0009's zero host exposure.
      netdevParts.push(`hostfwd=${fwd.proto}:127.0.0.1:${fwd.hostPort}-:${fwd.guestPort}`);
    }
    return ['-netdev', netdevParts.join(','), '-device', device];
  }

  // tap / bridge: host-level networking, env-gated off by default (ADR-0008/0009).
  if (options.allowHostNet !== true) {
    throw new HardwareSpecError(
      `network.mode "${network.mode}" requests host-level (${network.mode}) networking, which puts the ` +
        'guest on the host LAN and needs host privileges, so it is refused by default (ADR-0009). ' +
        'Use mode "user" for default user-mode networking, or set QMP_MCP_ALLOW_HOST_NET=true to opt ' +
        'in (the operator must provision the host bridge/tap; the server does not configure the host).',
    );
  }
  // mode is an allowlisted enum; escape defensively as with the model.
  return ['-netdev', `${escapeQemuOptsValue(network.mode)},id=${NETDEV_ID}`, '-device', device];
}

/**
 * Enforce the env-configurable resource caps on a validated spec (issue #9). The
 * caps live OUTSIDE the schema (they are operator policy, configured per
 * deployment), so they are injected via {@link ArgvOptions} and checked here
 * rather than as static zod maxes. A spec over a cap is rejected with a
 * {@link HardwareSpecError} that NAMES the cap variable and the
 * requested-vs-allowed values, e.g. "memoryMb 32768 exceeds
 * QMP_MCP_MAX_MEMORY_MB=4096". An omitted cap skips its check; the Orchestrator
 * always injects both, so create_instance is fail-closed before qemu is spawned.
 */
function assertWithinResourceCaps(spec: HardwareSpec, options: ArgvOptions): void {
  if (options.maxMemoryMb !== undefined && spec.memoryMb > options.maxMemoryMb) {
    throw new HardwareSpecError(
      `memoryMb ${spec.memoryMb} exceeds QMP_MCP_MAX_MEMORY_MB=${options.maxMemoryMb}. ` +
        `Request ${options.maxMemoryMb} MiB or less, or raise QMP_MCP_MAX_MEMORY_MB.`,
    );
  }
  if (options.maxVcpus !== undefined && spec.vcpus > options.maxVcpus) {
    throw new HardwareSpecError(
      `vcpus ${spec.vcpus} exceeds QMP_MCP_MAX_VCPUS=${options.maxVcpus}. ` +
        `Request ${options.maxVcpus} vCPU(s) or less, or raise QMP_MCP_MAX_VCPUS.`,
    );
  }
}

/**
 * Generate the full `qemu-system-*` argv (excluding the program name) from a
 * validated Hardware Spec. Pure: same inputs always yield the same array.
 *
 * The argv is headless and minimal by construction: `-nodefaults -nographic`
 * drop QEMU's implicit devices, and `-S` freezes the vCPUs at startup so the
 * Instance reaches a deterministic, agent-inspectable state before any Guest
 * code runs. The QMP monitor is exposed on a UNIX socket the server owns.
 *
 * Before generating argv it enforces the injected resource caps
 * ({@link assertWithinResourceCaps}), so an over-cap spec fails closed here
 * rather than reaching qemu.
 */
export function buildArgv(spec: HardwareSpec, options: ArgvOptions): string[] {
  // Resource caps (memory/vCPUs) are operator policy, injected from config and
  // checked before anything else so an over-cap spec never reaches qemu.
  assertWithinResourceCaps(spec, options);
  const raspi = isRaspiMachine(spec.machine);
  const argv = ['-machine', `${escapeQemuOptsValue(spec.machine)},accel=${options.accel}`];
  // The raspi* boards have fixed CPU/core-count/RAM baked into the machine and
  // QEMU rejects -cpu/-smp/-m for them, so emit those ONLY for other machines.
  if (!raspi) {
    argv.push('-cpu', spec.cpu, '-smp', String(spec.vcpus), '-m', String(spec.memoryMb));
  }
  // Direct-kernel boot (-kernel/-dtb/-append). Required for the raspi* machines
  // (they do not boot the SD card's firmware), and usable for any machine. The
  // schema guarantees dtb/appendCmdline only appear alongside a kernel. -append is
  // one argv token, so its spaces cannot split off another token.
  if (spec.kernel !== undefined) {
    argv.push('-kernel', resolveBootArtifact(spec.kernel, options.imageDir, 'kernel'));
  }
  if (spec.initrd !== undefined) {
    argv.push('-initrd', resolveBootArtifact(spec.initrd, options.imageDir, 'initrd'));
  }
  if (spec.dtb !== undefined) {
    argv.push('-dtb', resolveBootArtifact(spec.dtb, options.imageDir, 'dtb'));
  }
  if (spec.appendCmdline !== undefined) {
    argv.push('-append', spec.appendCmdline);
  }
  argv.push('-nodefaults', '-nographic', '-S');
  if (spec.display === 'vnc') {
    // Loopback-only VNC Display (ADR-0010). `password=on` REQUIRES a password but
    // does NOT carry one: the Orchestrator sets it after launch over QMP
    // (set_password), so no VNC secret ever appears in argv or `ps`. The bind is
    // pinned to 127.0.0.1 so the raw VNC port is unreachable off-host — the Viewer
    // is its sole client. The display number is fixed (single Instance).
    argv.push('-vnc', `${VNC_LOOPBACK_HOST}:${VNC_DISPLAY_NUMBER},password=on`);
  }
  // Display adapter (issue #15). `-nodefaults` strips a machine's default adapter, and
  // machines like virt/q35 have none, so a VNC Display needs an explicit device to
  // show anything. The raspi* boards have a built-in framebuffer (and no PCI bus for a
  // PCI adapter), so they must stay displayDevice:none and render directly.
  if (spec.displayDevice !== 'none') {
    if (raspi) {
      throw new HardwareSpecError(
        `displayDevice "${spec.displayDevice}" cannot be attached to the raspi* board "${spec.machine}": ` +
          'these boards render over their built-in framebuffer, so displayDevice must be "none" ' +
          '(display:vnc shows that framebuffer directly).',
      );
    }
    // model is a closed enum mapped to a fixed device name; no agent free-text here.
    argv.push('-device', DISPLAY_DEVICE_QEMU[spec.displayDevice]);
  }
  for (const disk of spec.disks) {
    argv.push(...buildDriveArgs(disk, options.imageDir));
  }
  if (spec.cdrom !== undefined) {
    argv.push(...buildCdromArgs(spec.cdrom, options.isoDir));
  }
  // Guest folder sharing (virtio-9p, ADR-0014): boolean opt-in; the host dir is
  // operator-configured, read-only unless enabled, refused on raspi (no PCI bus).
  if (spec.share) {
    argv.push(...buildShareArgs(options, raspi, spec.machine));
  }
  // Serial Port capture (ADR-0015). The explicit `-serial chardev:` redirect binds the
  // machine's first UART and wins over the earlier `-nographic` console mux.
  if (spec.serial) {
    argv.push(...buildSerialArgs(options));
  }
  if (spec.boot !== undefined) {
    // boot is already allowlisted to [a-dnp]+ by the schema; emit the single
    // order= form (and comma-escape as defense-in-depth) so it can never inject
    // a second -boot option or an extra argv token.
    argv.push('-boot', `order=${escapeQemuOptsValue(spec.boot)}`);
  }
  // Guest NIC: user-mode (SLiRP) by default; tap/bridge only when env-gated on;
  // 'none' emits nothing. The model is checked against the machine's bus.
  argv.push(...buildNetworkArgs(spec.network, spec.machine, options));
  argv.push('-qmp', `unix:${options.qmpSocketPath},server=on,wait=off`);
  // extraArgs (ADR-0002): the opt-in raw-args escape hatch. Raw QEMU arguments are
  // host-compromise-equivalent (e.g. `-drive file=/etc/shadow`, host `-netdev`
  // backends), so when a spec carries extraArgs but QMP_MCP_ALLOW_RAW_ARGS is not
  // enabled we REFUSE — fail-closed and explicit — rather than silently dropping
  // them. When enabled, they are appended verbatim after the generated argv.
  if (spec.extraArgs !== undefined && spec.extraArgs.length > 0) {
    if (options.allowRawArgs !== true) {
      throw new HardwareSpecError(
        `extraArgs were supplied, but the raw-args escape hatch is disabled. Raw QEMU arguments are ` +
          `host-compromise-equivalent, so they are refused by default (ADR-0002). Set ` +
          `QMP_MCP_ALLOW_RAW_ARGS=true to opt in (trusted single-tenant hosts only), or remove ` +
          `extraArgs and express the hardware through the Hardware Spec.`,
      );
    }
    argv.push(...spec.extraArgs);
  }
  return argv;
}
