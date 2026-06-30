/**
 * The fake {@link QemuDriver} — the in-memory test double behind the driver
 * port. It spawns no process and opens no socket; it records launch requests and
 * answers QMP commands from a small, configurable table. This is what makes the
 * Orchestrator's lifecycle testable end-to-end without a real QEMU.
 */

import type { InstanceProcess, LaunchRequest, QemuDriver, QmpEvent } from './driver.js';

/** Tunes how the fake behaves for a given test. */
export interface FakeQemuDriverOptions {
  /**
   * Canned responses keyed by QMP command name. `query-status` defaults to a
   * running VM. A handler may be a value or a function of the command args.
   */
  responses?: Record<string, unknown | ((args?: Record<string, unknown>) => unknown)>;
  /** When set, {@link QemuDriver.launch} rejects with this error. */
  launchError?: Error;
}

const DEFAULT_RESPONSES: Record<string, unknown> = {
  qmp_capabilities: {},
  'query-status': { status: 'running', running: true, singlestep: false },
};

/**
 * Records every launch and hands back a {@link FakeInstanceProcess}. Tests can
 * inspect {@link launches} and {@link lastProcess} to assert what the
 * Orchestrator did.
 */
export class FakeQemuDriver implements QemuDriver {
  readonly launches: LaunchRequest[] = [];
  lastProcess?: FakeInstanceProcess;
  #options: FakeQemuDriverOptions;

  constructor(options: FakeQemuDriverOptions = {}) {
    this.#options = options;
  }

  async launch(request: LaunchRequest): Promise<InstanceProcess> {
    if (this.#options.launchError) throw this.#options.launchError;
    this.launches.push(request);
    const process = new FakeInstanceProcess({
      ...DEFAULT_RESPONSES,
      ...this.#options.responses,
    });
    this.lastProcess = process;
    return process;
  }
}

/** An in-memory {@link InstanceProcess} that answers from a response table. */
export class FakeInstanceProcess implements InstanceProcess {
  readonly exited: Promise<void>;
  readonly executed: Array<{ command: string; args?: Record<string, unknown> }> = [];
  closed = false;

  #responses: Record<string, unknown | ((args?: Record<string, unknown>) => unknown)>;
  #listeners = new Set<(event: QmpEvent) => void>();
  #resolveExited!: () => void;

  constructor(responses: Record<string, unknown | ((args?: Record<string, unknown>) => unknown)>) {
    this.#responses = responses;
    this.exited = new Promise<void>((resolve) => {
      this.#resolveExited = resolve;
    });
  }

  async execute(command: string, args?: Record<string, unknown>): Promise<unknown> {
    if (this.closed) throw new Error('Instance is closed.');
    this.executed.push({ command, args });
    if (!(command in this.#responses)) {
      throw new Error(`FakeInstanceProcess has no canned response for QMP command "${command}".`);
    }
    const response = this.#responses[command];
    return typeof response === 'function'
      ? (response as (args?: Record<string, unknown>) => unknown)(args)
      : response;
  }

  onEvent(listener: (event: QmpEvent) => void): () => void {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  }

  /** Test helper: deliver an async QMP event to all subscribers. */
  emitEvent(event: QmpEvent): void {
    for (const listener of this.#listeners) listener(event);
  }

  /**
   * Test seam: resolve `exited` WITHOUT going through {@link close}, simulating
   * an unexpected qemu exit (crash/SIGKILL). Lets a test drive the Orchestrator's
   * exit-reconciliation path. {@link closed} stays false until the Orchestrator
   * releases the handle.
   */
  simulateExit(): void {
    this.#resolveExited();
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    this.#resolveExited();
  }
}
