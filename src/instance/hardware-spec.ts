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

/** Concrete accelerators QEMU can be launched with. */
export type Accel = 'kvm' | 'tcg';

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
export const hardwareSpecSchema = z
  .object({
    machine: z
      .string()
      .min(1)
      .default('q35')
      .describe('QEMU machine type, e.g. "q35" (default) or "pc".'),
    cpu: z
      .string()
      .min(1)
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
  return [
    '-machine',
    `${spec.machine},accel=${options.accel}`,
    '-cpu',
    spec.cpu,
    '-smp',
    String(spec.vcpus),
    '-m',
    String(spec.memoryMb),
    '-nodefaults',
    '-nographic',
    '-S',
    '-qmp',
    `unix:${options.qmpSocketPath},server=on,wait=off`,
  ];
}
