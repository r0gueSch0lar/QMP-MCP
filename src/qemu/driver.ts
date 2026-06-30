/**
 * The QEMU driver port — the single primary test seam of this server.
 *
 * It abstracts the two things only a real machine can do: spawn a
 * `qemu-system-*` process with a given argv, and own the live QMP Session to it.
 * The Orchestrator depends on this interface (constructor injection), so the
 * whole lifecycle is exercisable against the in-memory {@link FakeQemuDriver}
 * with no real process or socket, while production wires in the real driver.
 */

import type { QmpEvent } from './qmp-client.js';

export type { QmpEvent } from './qmp-client.js';

/** Everything the driver needs to launch and connect to one Instance. */
export interface LaunchRequest {
  /** The `qemu-system-*` binary to exec (e.g. `qemu-system-x86_64`). */
  binary: string;
  /** The full argv (excluding the program name), already including `-qmp`. */
  argv: string[];
  /** Path of the QMP UNIX socket the launched process will create. */
  qmpSocketPath: string;
}

/**
 * A launched Instance with an established QMP Session. The handle owns the QMP
 * channel: callers drive the Guest exclusively through {@link execute} and
 * observe async {@link onEvent} notifications, and tear everything down with
 * {@link close}.
 */
export interface InstanceProcess {
  /** Execute a QMP command, resolving with its `return` value. */
  execute(command: string, args?: Record<string, unknown>): Promise<unknown>;
  /** Subscribe to async QMP events. Returns an unsubscribe function. */
  onEvent(listener: (event: QmpEvent) => void): () => void;
  /** Terminate the process and close the QMP Session. Idempotent. */
  close(): Promise<void>;
  /** Resolves when the underlying process has exited (for any reason). */
  readonly exited: Promise<void>;
}

/**
 * The driver port. A single method launches an Instance and hands back a live
 * {@link InstanceProcess}; everything else flows through that handle.
 */
export interface QemuDriver {
  launch(request: LaunchRequest): Promise<InstanceProcess>;
}
