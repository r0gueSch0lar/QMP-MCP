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
import { IMAGE_FORMATS, ImageStoreError, resolveImagePath } from './image-store.js';

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
 * Generate the full `qemu-system-*` argv (excluding the program name) from a
 * validated Hardware Spec. Pure: same inputs always yield the same array.
 *
 * The argv is headless and minimal by construction: `-nodefaults -nographic`
 * drop QEMU's implicit devices, and `-S` freezes the vCPUs at startup so the
 * Instance reaches a deterministic, agent-inspectable state before any Guest
 * code runs. The QMP monitor is exposed on a UNIX socket the server owns.
 */
export function buildArgv(spec: HardwareSpec, options: ArgvOptions): string[] {
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
  argv.push('-qmp', `unix:${options.qmpSocketPath},server=on,wait=off`);
  return argv;
}
