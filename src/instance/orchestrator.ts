/**
 * The single-instance lifecycle Orchestrator (ADR-0001/0004). It holds the one
 * managed Instance and drives it through its lifecycle:
 *
 *   NONE → STARTING → RUNNING ⇄ PAUSED → STOPPED → NONE
 *
 * This slice implements the create/destroy transitions (NONE → STARTING →
 * RUNNING → STOPPED → NONE); PAUSED is reserved for the pause/resume slice.
 *
 * The Orchestrator depends on the {@link QemuDriver} port by constructor
 * injection, so its whole lifecycle is testable against the fake driver. The
 * process-global {@link orchestrator} singleton wires in the real driver.
 */

import { stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { resolveImageDir, resolveIsoDir } from '../config.js';
import { logger } from '../logger.js';
import type { InstanceProcess, QemuDriver } from '../qemu/driver.js';
import { RealQemuDriver } from '../qemu/real-driver.js';
import {
  type Accel,
  buildArgv,
  type HardwareSpec,
  parseHardwareSpec,
  probeKvm,
  resolveAccel,
} from './hardware-spec.js';

/**
 * The lifecycle states an Instance moves through. `PAUSED` is part of the
 * model but only reachable once the pause/resume slice lands.
 */
export type InstanceState = 'NONE' | 'STARTING' | 'RUNNING' | 'PAUSED' | 'STOPPED';

/** A read-only view of the current Instance for tools to return. */
export interface InstanceView {
  state: InstanceState;
  /** The validated Hardware Spec, when an Instance exists. */
  spec?: HardwareSpec;
  /** The accelerator the running Instance was launched with. */
  accel?: Accel;
}

/** The result of a successful {@link Orchestrator.createInstance}. */
export interface CreateInstanceResult {
  state: 'RUNNING';
  /** The validated Hardware Spec the Instance was built from. */
  spec: HardwareSpec;
  /** The accelerator actually chosen (KVM or TCG). */
  accel: Accel;
  /** Why that accelerator was chosen — reported to the agent (ADR-0008). */
  accelReason: string;
}

/** Knobs the Orchestrator needs that are not part of the Hardware Spec. */
export interface OrchestratorOptions {
  /** The `qemu-system-*` binary to launch. */
  binary: string;
  /** Server-managed path of the QMP UNIX socket. */
  qmpSocketPath: string;
  /**
   * Absolute path of the Image Store directory (ADR-0006). Disk names in the
   * spec are resolved against it when building the argv. Optional: a diskless
   * spec never needs it.
   */
  imageDir?: string;
  /**
   * Absolute path of the read-only ISO Store directory (ADR-0006). A cdrom's ISO
   * name in the spec is resolved against it when building the argv. Optional: a
   * spec with no cdrom never needs it.
   */
  isoDir?: string;
  /** Probe for KVM availability (injected for testability). */
  kvmAvailable: () => boolean;
  /**
   * Predicate for "is the QMP socket path already occupied". Injected so the
   * refuse-if-occupied branch is testable without touching the filesystem.
   */
  socketOccupied: (path: string) => Promise<boolean>;
}

/**
 * Raised for lifecycle violations (e.g. creating while an Instance exists). The
 * message is always actionable: it tells the agent what to do next.
 */
export class LifecycleError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'LifecycleError';
  }
}

/** Default QMP socket path: a per-server file under the OS runtime/temp dir. */
export function defaultQmpSocketPath(): string {
  return join(tmpdir(), 'qmp-mcp', 'qmp.sock');
}

/** Default occupied-check: the path exists (as a socket or anything else). */
export async function defaultSocketOccupied(path: string): Promise<boolean> {
  try {
    await stat(path);
    return true;
  } catch {
    return false;
  }
}

/**
 * Holds the single managed Instance as a process-global singleton: exactly one
 * Instance exists at a time. Requesting a new Instance while one exists is
 * rejected rather than auto-replaced (ADR-0004).
 */
export class Orchestrator {
  #driver: QemuDriver;
  #options: OrchestratorOptions;
  #state: InstanceState = 'NONE';
  #process?: InstanceProcess;
  #spec?: HardwareSpec;
  #accel?: Accel;
  /**
   * Identifies the create_instance call that currently owns the reserved slot.
   * A call only mutates the singleton's fields while its own token is installed,
   * so a superseded launch cannot clobber a slot another call has since taken.
   */
  #launchToken?: symbol;

  constructor(driver: QemuDriver, options: OrchestratorOptions) {
    this.#driver = driver;
    this.#options = options;
  }

  /** Return the current Instance view. Reports `NONE` when nothing is running. */
  getInstance(): InstanceView {
    return { state: this.#state, spec: this.#spec, accel: this.#accel };
  }

  /**
   * Build and launch a new Instance from an untrusted candidate Hardware Spec,
   * negotiate its QMP Session, and bring it to `RUNNING`. Rejects when an
   * Instance already exists or the QMP socket path is occupied.
   */
  async createInstance(candidate: unknown): Promise<CreateInstanceResult> {
    if (this.#state !== 'NONE') {
      throw new LifecycleError(
        `An Instance already exists (state ${this.#state}). Only one Instance may run at a time — ` +
          'destroy it with destroy_instance before creating a new one.',
      );
    }

    // Reserve the single slot SYNCHRONOUSLY — before any await (spec parse, accel
    // probe, socket check, launch) — so two concurrent create_instance calls
    // cannot both pass the NONE guard above and spawn two qemu, orphaning one.
    this.#state = 'STARTING';
    const token = Symbol('launch');
    this.#launchToken = token;
    const ownsSlot = (): boolean => this.#launchToken === token;
    // Release the slot back to NONE, but only while this call still owns it.
    const release = (): void => {
      if (!ownsSlot()) return;
      this.#launchToken = undefined;
      this.#process = undefined;
      this.#spec = undefined;
      this.#accel = undefined;
      this.#state = 'NONE';
    };

    try {
      // Parse/accel are synchronous but may throw; a throw must free the slot.
      const spec = parseHardwareSpec(candidate);
      const resolution = resolveAccel(spec.accel, this.#options.kvmAvailable);

      const { qmpSocketPath, binary } = this.#options;
      if (await this.#options.socketOccupied(qmpSocketPath)) {
        throw new LifecycleError(
          `The QMP socket path ${qmpSocketPath} is already occupied — refusing to start rather than ` +
            'clobber or adopt a process this server did not launch. Remove the stale socket (or stop the ' +
            'other process), then retry.',
        );
      }

      const argv = buildArgv(spec, {
        accel: resolution.accel,
        qmpSocketPath,
        imageDir: this.#options.imageDir,
        isoDir: this.#options.isoDir,
      });
      logger.info(`creating Instance (machine=${spec.machine}, accel=${resolution.accel})`);

      let process: InstanceProcess;
      try {
        process = await this.#driver.launch({ binary, argv, qmpSocketPath });
      } catch (err) {
        throw new LifecycleError(
          `Failed to create the Instance: ${err instanceof Error ? err.message : String(err)}`,
        );
      }

      // If we lost ownership while launch was in flight (e.g. a concurrent
      // destroy/reset), do NOT clobber the new owner's state; tear down the
      // process we just launched so it is not orphaned.
      if (!ownsSlot()) {
        void process.close().catch(() => undefined);
        throw new LifecycleError(
          'Instance creation was superseded before it completed; the launched process was torn down.',
        );
      }

      this.#process = process;
      this.#spec = spec;
      this.#accel = resolution.accel;
      this.#state = 'RUNNING';
      this.#launchToken = undefined;
      // If the process exits on its own, reflect that the Instance is gone.
      void process.exited.then(() => this.#onProcessExit());

      logger.info(`Instance RUNNING (${resolution.reason})`);
      return {
        state: 'RUNNING',
        spec,
        accel: resolution.accel,
        accelReason: resolution.reason,
      };
    } catch (err) {
      release();
      throw err;
    }
  }

  /**
   * Terminate the running Instance's `qemu-system-*` process, close its QMP
   * Session, and return to `NONE`. Rejects when no Instance exists.
   */
  async destroyInstance(): Promise<{ state: 'NONE' }> {
    if (this.#state === 'NONE' || !this.#process) {
      throw new LifecycleError(
        'No Instance is running, so there is nothing to destroy. Create one with create_instance first.',
      );
    }
    // Claim the teardown SYNCHRONOUSLY: capture the handle and clear it (and the
    // slot/spec/accel) before the first await, so a concurrent destroyInstance
    // hits the no-Instance guard above instead of double-closing.
    const process = this.#process;
    this.#process = undefined;
    this.#spec = undefined;
    this.#accel = undefined;
    this.#launchToken = undefined;
    this.#state = 'STOPPED';
    logger.info('destroying Instance');
    try {
      await process.close();
    } finally {
      this.#state = 'NONE';
    }
    logger.info('Instance destroyed (state NONE)');
    return { state: 'NONE' };
  }

  /**
   * Return the live QMP `query-status` result for the running Instance. Rejects
   * when no Instance is running.
   */
  async getStatus(): Promise<unknown> {
    if (!this.#process) {
      throw new LifecycleError(
        'No Instance is running. Create one with create_instance before querying its status.',
      );
    }
    return this.#process.execute('query-status');
  }

  /** Reconcile state when the process exits without an explicit destroy. */
  #onProcessExit(): void {
    if (this.#state === 'STOPPED' || this.#state === 'NONE') return;
    logger.warning('Instance process exited unexpectedly; resetting state to NONE');
    const process = this.#process;
    this.#process = undefined;
    this.#spec = undefined;
    this.#accel = undefined;
    this.#launchToken = undefined;
    this.#state = 'NONE';
    // Release the handle so the managed QMP socket is removed. A crashed/SIGKILLed
    // qemu leaves its socket file behind; without this, every future create would
    // refuse with 'occupied'. close() is idempotent and best-effort here.
    void process?.close().catch(() => undefined);
  }
}

/** The process-global Orchestrator singleton, wired to the real QEMU driver. */
export const orchestrator = new Orchestrator(new RealQemuDriver(), {
  binary: 'qemu-system-x86_64',
  qmpSocketPath: defaultQmpSocketPath(),
  // Resolve disk names against the configured Image Store (ADR-0006).
  imageDir: resolveImageDir(process.env),
  // Resolve cdrom ISO names against the configured read-only ISO Store (ADR-0006).
  isoDir: resolveIsoDir(process.env),
  // `/dev/kvm` probe (single source of truth) from the hardware-spec module.
  kvmAvailable: probeKvm,
  socketOccupied: defaultSocketOccupied,
});
