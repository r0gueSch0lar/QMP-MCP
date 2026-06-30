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

/** Guest-visible disk controller a disk attaches through. */
export const DISK_INTERFACES = ['virtio', 'ide', 'scsi'] as const;
export type DiskInterface = (typeof DISK_INTERFACES)[number];

/**
 * The requested accelerator. `auto` probes `/dev/kvm` and falls back to TCG;
 * `kvm` hard-fails when unavailable; `tcg` is always available (ADR-0008).
 */
export const ACCEL_MODES = ['auto', 'kvm', 'tcg'] as const;
export type AccelMode = (typeof ACCEL_MODES)[number];

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
 * Strict allowlist of guest NIC models the agent may pick (ADR-0009). The model
 * is emitted verbatim into `-device <model>,netdev=...`, so it is a CLOSED enum,
 * never a free string: a free string could carry a comma to inject extra
 * `-device` properties (e.g. a second device, an `addr=`), or an unknown model.
 * Keep this list short and boring — paravirtual `virtio-net-pci` plus two widely
 * emulated legacy NICs for guests without virtio drivers.
 */
export const NIC_MODELS = ['virtio-net-pci', 'e1000', 'rtl8139'] as const;
export type NicModel = (typeof NIC_MODELS)[number];

/**
 * Guest networking backend. `user` is QEMU user-mode networking (SLiRP): NAT'd
 * outbound with the host network unexposed and inbound only via explicit
 * port-forwards — the safe, unprivileged default (ADR-0009). `tap`/`bridge` put
 * the guest on the host LAN and need host privileges, so they are env-gated off
 * (see `QMP_MCP_ALLOW_HOST_NET`).
 */
export const NETWORK_MODES = ['user', 'tap', 'bridge'] as const;
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
        "Networking backend: 'user' (default, SLiRP NAT) or 'tap'/'bridge' (host networking, " +
          'requires QMP_MCP_ALLOW_HOST_NET=true).',
      ),
    model: z
      .enum(NIC_MODELS)
      .default('virtio-net-pci')
      .describe(
        "Guest NIC model from a fixed allowlist: 'virtio-net-pci' (default), 'e1000', or 'rtl8139'.",
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
    disks: z
      .array(diskSchema)
      .default([])
      .describe('Guest disks, each referencing an image by name in the Image Store.'),
    cdrom: cdromSchema
      .optional()
      .describe('Optional CD-ROM drive backed by an ISO (by name) from the read-only ISO Store.'),
    boot: z
      .string()
      .regex(VALID_BOOT_ORDER, bootOrderMessage)
      .optional()
      .describe(
        "Optional boot order as QEMU drive letters, e.g. 'd' (CD-ROM first) or 'dc'. Emitted as -boot order=.",
      ),
    network: networkSchema
      .default({})
      .describe('Guest NIC; defaults to user-mode (SLiRP) networking with no port-forwards.'),
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
  if (result.success) return result.data;
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
 * - `auto` → KVM when available, otherwise TCG.
 */
export function resolveAccel(
  requested: AccelMode,
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
 *
 * The NIC `model` and `mode` are closed enums, so no agent free-text reaches the
 * option string; the model is comma-escaped anyway as defense-in-depth so it can
 * never split off an extra `-device`/`-netdev` property.
 */
function buildNetworkArgs(network: Network, options: ArgvOptions): string[] {
  // model is an allowlisted enum; escape defensively so it can never inject an
  // extra -device property no matter how the allowlist evolves.
  const device = `${escapeQemuOptsValue(network.model)},netdev=${NETDEV_ID}`;

  // hostForwards are a user-mode (SLiRP) concept only; QEMU has nowhere to apply
  // them under tap/bridge. Refuse rather than silently drop them so the agent
  // is told its forwards would not take effect (fail-closed, ADR-0009).
  if (network.mode !== 'user' && network.hostForwards.length > 0) {
    throw new HardwareSpecError(
      `network.hostForwards are only valid for user-mode networking (mode "user"), but mode is ` +
        `"${network.mode}". Host-level (${network.mode}) networking puts the guest on the host LAN, ` +
        `where QEMU user-mode port-forwards do not apply. Remove hostForwards, or use mode "user".`,
    );
  }

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
  const argv = [
    '-machine',
    `${escapeQemuOptsValue(spec.machine)},accel=${options.accel}`,
    '-cpu',
    spec.cpu,
    '-smp',
    String(spec.vcpus),
    '-m',
    String(spec.memoryMb),
    '-nodefaults',
    '-nographic',
    '-S',
  ];
  for (const disk of spec.disks) {
    argv.push(...buildDriveArgs(disk, options.imageDir));
  }
  if (spec.cdrom !== undefined) {
    argv.push(...buildCdromArgs(spec.cdrom, options.isoDir));
  }
  if (spec.boot !== undefined) {
    // boot is already allowlisted to [a-dnp]+ by the schema; emit the single
    // order= form (and comma-escape as defense-in-depth) so it can never inject
    // a second -boot option or an extra argv token.
    argv.push('-boot', `order=${escapeQemuOptsValue(spec.boot)}`);
  }
  // Guest NIC: user-mode (SLiRP) by default; tap/bridge only when env-gated on.
  argv.push(...buildNetworkArgs(spec.network, options));
  argv.push('-qmp', `unix:${options.qmpSocketPath},server=on,wait=off`);
  return argv;
}
