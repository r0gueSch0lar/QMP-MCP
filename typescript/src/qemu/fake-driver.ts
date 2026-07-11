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
   * Canned responses keyed by QMP command name. A handler may be a value or a
   * function of the command args (so a test can inspect what was sent, e.g. the
   * server-chosen `screendump` filename). `query-status` needs no canned response:
   * the fake answers it from a simulated run-state that `stop`/`cont` flip, so
   * `get_status` reflects a pause without the test wiring it up. An explicit
   * `query-status` entry still overrides that simulated default.
   */
  responses?: Record<string, unknown | ((args?: Record<string, unknown>) => unknown)>;
  /** When set, {@link QemuDriver.launch} rejects with this error. */
  launchError?: Error;
}

/**
 * Commands every fake Instance answers out of the box. The lifecycle power
 * commands (`stop`/`cont`/`system_reset`/`system_powerdown`) return QMP's empty
 * success `{}` so a test need only assert they were *issued*; `stop`/`cont` also
 * flip the simulated run-state (see {@link FakeInstanceProcess}). `query-status`
 * is intentionally absent — it is answered dynamically from that run-state.
 */
const DEFAULT_RESPONSES: Record<string, unknown> = {
  qmp_capabilities: {},
  stop: {},
  cont: {},
  system_reset: {},
  system_powerdown: {},
  // Arming a vnc Display's password after launch (ADR-0010) returns QMP's empty
  // success; a test need only assert it was issued with the vnc protocol.
  set_password: {},
  // Serial Port (ADR-0015): ringbuf-read returns buffered console text; ringbuf-write
  // returns QMP's empty success.
  'ringbuf-read': 'fake-serial-output',
  'ringbuf-write': {},
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
  /**
   * Simulated Guest-CPU run-state: `stop` pauses it, `cont` resumes it. Starts
   * FALSE, modelling QEMU's `-S` startup pause — the Guest is loaded but frozen
   * until the first `cont` (an explicit resume_instance, or create's auto-start).
   * So `query-status` reads `paused` right after create, matching the PAUSED
   * lifecycle state the Orchestrator lands in by default (issue #10).
   */
  #running = false;

  constructor(responses: Record<string, unknown | ((args?: Record<string, unknown>) => unknown)>) {
    this.#responses = responses;
    this.exited = new Promise<void>((resolve) => {
      this.#resolveExited = resolve;
    });
  }

  async execute(command: string, args?: Record<string, unknown>): Promise<unknown> {
    if (this.closed) throw new Error('Instance is closed.');
    this.executed.push({ command, args });
    // Keep the simulated run-state consistent so a later `query-status` reflects
    // the pause/resume the Orchestrator just performed (mirrors real QEMU).
    if (command === 'stop') this.#running = false;
    else if (command === 'cont') this.#running = true;
    if (command in this.#responses) {
      const response = this.#responses[command];
      return typeof response === 'function'
        ? (response as (args?: Record<string, unknown>) => unknown)(args)
        : response;
    }
    // No explicit canned response: answer `query-status` from the run-state.
    if (command === 'query-status') {
      return { status: this.#running ? 'running' : 'paused', running: this.#running };
    }
    throw new Error(`FakeInstanceProcess has no canned response for QMP command "${command}".`);
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
