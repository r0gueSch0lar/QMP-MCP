/**
 * The real {@link QemuDriver}: it actually spawns a `qemu-system-*` child,
 * waits for it to create the QMP UNIX socket, dials it, completes the QMP
 * handshake, and hands back an {@link InstanceProcess} that owns the live QMP
 * Session. Teardown terminates the child (TERM, then KILL) and cleans up the
 * socket file.
 */

import { type ChildProcess, spawn } from 'node:child_process';
import { mkdir, rm } from 'node:fs/promises';
import { dirname } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { logger } from '../logger.js';
import type { InstanceProcess, LaunchRequest, QemuDriver } from './driver.js';
import { QmpClient, type QmpEvent } from './qmp-client.js';

/** How long to wait for QEMU to create and accept on the QMP socket. */
const SOCKET_DIAL_TIMEOUT_MS = 10_000;
/** Delay between connection attempts while QEMU is still starting up. */
const SOCKET_DIAL_INTERVAL_MS = 50;
/** Grace period after SIGTERM before escalating to SIGKILL during teardown. */
const TERMINATE_GRACE_MS = 5_000;
/** How much child stderr to retain for diagnostics on a launch failure. */
const STDERR_CAP = 4_000;

export class RealQemuDriver implements QemuDriver {
  async launch(request: LaunchRequest): Promise<InstanceProcess> {
    const { binary, argv, qmpSocketPath } = request;

    // QEMU creates the socket; its parent directory must already exist.
    await mkdir(dirname(qmpSocketPath), { recursive: true });

    logger.debug(`spawning ${binary} ${argv.join(' ')}`);
    // stdout is unused; ignore it so an undrained pipe can never apply backpressure
    // to the child. stderr is piped and drained below for launch-failure diagnostics.
    const child = spawn(binary, argv, { stdio: ['ignore', 'ignore', 'pipe'] });

    let stderr = '';
    child.stderr?.setEncoding('utf8');
    child.stderr?.on('data', (chunk: string) => {
      stderr = (stderr + chunk).slice(-STDERR_CAP);
    });

    // Track exit so dialing can fail fast and `exited` is observable.
    let exitInfo: { code: number | null; signal: NodeJS.Signals | null } | undefined;
    const exited = new Promise<void>((resolve) => {
      child.once('exit', (code, signal) => {
        exitInfo = { code, signal };
        resolve();
      });
    });

    // A spawn-level failure (e.g. binary not found) surfaces via 'error'.
    let onChildError: ((err: Error) => void) | undefined;
    const spawnError = new Promise<never>((_, reject) => {
      onChildError = (err: Error): void =>
        reject(new Error(`Failed to spawn ${binary}: ${err.message}`));
      child.once('error', onChildError);
    });
    // If dial wins the race, spawnError stays pending; a later 'error' (e.g. during
    // teardown) must not surface as an unhandled rejection that crashes the server.
    spawnError.catch(() => undefined);

    let client: QmpClient | undefined;
    try {
      client = await Promise.race([
        this.#dial(
          qmpSocketPath,
          () => exitInfo,
          () => stderr,
        ),
        spawnError,
      ]);
      await client.negotiate();
      // Dial won: stop treating child 'error' as a spawn failure, but keep a benign
      // listener so a later runtime 'error' can't crash the process (an
      // unhandled 'error' on a ChildProcess throws).
      if (onChildError) child.removeListener('error', onChildError);
      child.on('error', (err: Error) =>
        logger.warning(`qemu child emitted an error after launch: ${err.message}`),
      );
      return new RealInstanceProcess(child, client, exited, qmpSocketPath);
    } catch (err) {
      // Ensure we never leak a half-started child, a live QMP client, or a stale
      // socket. `client?.` guards the case where dial itself failed.
      await client?.close().catch(() => undefined);
      await terminate(child, exited);
      await rm(qmpSocketPath, { force: true }).catch(() => undefined);
      throw err;
    }
  }

  /** Retry-connect to the QMP socket until it accepts, QEMU exits, or timeout. */
  async #dial(
    socketPath: string,
    exitInfo: () => { code: number | null; signal: NodeJS.Signals | null } | undefined,
    stderr: () => string,
  ): Promise<QmpClient> {
    const deadline = Date.now() + SOCKET_DIAL_TIMEOUT_MS;
    for (;;) {
      const exit = exitInfo();
      if (exit) {
        throw new Error(
          `QEMU exited before the QMP socket was ready (code=${exit.code}, signal=${exit.signal}). ` +
            `Stderr: ${stderr().trim() || '(empty)'}`,
        );
      }
      try {
        return await QmpClient.dial(socketPath);
      } catch (err) {
        if (Date.now() >= deadline) {
          throw new Error(
            `Timed out after ${SOCKET_DIAL_TIMEOUT_MS}ms connecting to the QMP socket at ${socketPath}: ` +
              `${(err as Error).message}. Stderr: ${stderr().trim() || '(empty)'}`,
          );
        }
        await delay(SOCKET_DIAL_INTERVAL_MS);
      }
    }
  }
}

/** A real launched Instance backed by a child process and a {@link QmpClient}. */
class RealInstanceProcess implements InstanceProcess {
  readonly exited: Promise<void>;
  #child: ChildProcess;
  #client: QmpClient;
  #qmpSocketPath: string;
  #closing?: Promise<void>;

  constructor(
    child: ChildProcess,
    client: QmpClient,
    exited: Promise<void>,
    qmpSocketPath: string,
  ) {
    this.#child = child;
    this.#client = client;
    this.#qmpSocketPath = qmpSocketPath;
    this.exited = exited;
  }

  execute(command: string, args?: Record<string, unknown>): Promise<unknown> {
    return this.#client.execute(command, args);
  }

  onEvent(listener: (event: QmpEvent) => void): () => void {
    return this.#client.onEvent(listener);
  }

  close(): Promise<void> {
    if (!this.#closing) this.#closing = this.#doClose();
    return this.#closing;
  }

  async #doClose(): Promise<void> {
    await this.#client.close().catch(() => undefined);
    await terminate(this.#child, this.exited);
    // QEMU removes its listening socket on clean exit, but tidy up regardless.
    await rm(this.#qmpSocketPath, { force: true }).catch(() => undefined);
  }
}

/** Terminate a child with SIGTERM, escalating to SIGKILL after a grace period. */
async function terminate(child: ChildProcess, exited: Promise<void>): Promise<void> {
  if (child.exitCode !== null || child.signalCode !== null) return;
  child.kill('SIGTERM');
  // Cancel the grace timer the moment the child exits, so it can't keep the event
  // loop (and the server's shutdown) alive for the full grace period after exit.
  const ac = new AbortController();
  const timedOut = Symbol('timeout');
  const grace = delay(TERMINATE_GRACE_MS, timedOut, { signal: ac.signal });
  // Aborting rejects the timer; swallow that so it never becomes unhandled.
  grace.catch(() => undefined);
  let race: symbol | 'exited';
  try {
    race = await Promise.race([exited.then(() => 'exited' as const), grace]);
  } finally {
    ac.abort();
  }
  if (race === timedOut) {
    child.kill('SIGKILL');
    await exited;
  }
}
