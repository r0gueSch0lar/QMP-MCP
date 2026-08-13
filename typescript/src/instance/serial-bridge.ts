/**
 * The socket-backend Serial Port bridge (ADR-0015): a live UNIX-socket connection to QEMU's
 * server-owned serial `socket` chardev. QEMU listens (`-chardev socket,...,server=on,wait=off`);
 * the Orchestrator dials it here and pumps bytes both ways —
 *
 *   - guest → server: the reader drains the socket into a bounded ring (overwrite-oldest, like
 *     QEMU's own `ringbuf`), which `read_serial` drains destructively;
 *   - server → guest: `write_serial` writes raw bytes straight onto the connected serial line, so
 *     unlike `ringbuf-write` the input actually reaches the guest console.
 *
 * The socket path is a `serial.sock` sibling of the managed QMP socket — server-derived, never
 * agent/operator input. Mirrors the Rust `SerialBridge` + `attach_serial_bridge` +
 * `teardown_serial_bridge` in `../../../rust/src/instance/orchestrator.rs`.
 */

import { rm } from 'node:fs/promises';
import { createConnection, type Socket } from 'node:net';
import { getLogger } from '../logger.js';
import { serialSocketPath } from './hardware-spec.js';

// The Serial Port bridge is Instance-lifecycle machinery, so it logs under
// `orchestrator` (in Rust it lives inside the orchestrator module).
const logger = getLogger('orchestrator');

/** How long {@link attachSerialBridge} retries dialing QEMU's serial socket before failing. */
export const SERIAL_DIAL_TIMEOUT_MS = 2_000;
/** Delay between dial attempts while QEMU is still opening its listening socket. */
export const SERIAL_DIAL_INTERVAL_MS = 25;
/** How long a single {@link SerialBridge.write} waits before declaring the guest not draining. */
export const SERIAL_WRITE_TIMEOUT_MS = 2_000;

/**
 * Marker error for a `write` that exceeded {@link SERIAL_WRITE_TIMEOUT_MS} — the guest is not
 * draining its UART RX (paused/panicked/no console reader), so the socket buffer filled. The
 * Orchestrator turns it into the actionable `write_serial` timeout message.
 */
export class SerialWriteTimeoutError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'SerialWriteTimeoutError';
  }
}

/**
 * Append `chunk` to a bounded serial ring, evicting the oldest bytes once it exceeds `cap` — the
 * same overwrite-oldest behaviour QEMU's `ringbuf` chardev has, so a slow reader never blocks the
 * guest's serial writes (it just loses the oldest unread output). Pure, so it is unit-tested
 * directly. Mirrors the Rust `ring_push_bounded`.
 */
export function pushBounded(ring: Buffer, chunk: Buffer, cap: number): Buffer {
  const combined = ring.length === 0 ? Buffer.from(chunk) : Buffer.concat([ring, chunk]);
  return combined.length > cap ? Buffer.from(combined.subarray(combined.length - cap)) : combined;
}

/**
 * A live serial-socket connection with a bounded capture ring. Constructed by
 * {@link attachSerialBridge}; the Orchestrator holds one for a `serial: true` socket-backend
 * Instance and tears it down with the Instance.
 */
export class SerialBridge {
  /** The server-owned serial socket path, unlinked on teardown. */
  readonly socketPath: string;
  readonly #socket: Socket;
  readonly #cap: number;
  /** Bounded ring of captured guest serial output; `drain` empties it destructively. */
  #ring: Buffer = Buffer.alloc(0);

  constructor(socket: Socket, cap: number, socketPath: string) {
    this.#socket = socket;
    this.#cap = cap;
    this.socketPath = socketPath;
    // Always drain the socket into the bounded ring so the guest never stalls writing to its
    // serial port; on overflow, evict the oldest bytes (QEMU `ringbuf`'s overwrite-oldest).
    socket.on('data', (chunk: Buffer) => {
      this.#ring = pushBounded(this.#ring, chunk, this.#cap);
    });
    socket.on('end', () => logger.debug('serial socket closed by peer'));
    // A mid-session socket error must never surface as an unhandled 'error' that crashes the
    // server: teardown owns the connection, and read/write report their own failures.
    socket.on('error', (err: Error) => logger.debug(`serial socket read error: ${err.message}`));
  }

  /**
   * Drain up to `size` bytes from the oldest end of the ring (destructive), matching the ringbuf
   * backend's drain-on-read semantics. Returns a copy, so the retained ring shares no memory.
   */
  drain(size: number): Buffer {
    const take = Math.min(Math.max(0, size), this.#ring.length);
    const out = Buffer.from(this.#ring.subarray(0, take));
    this.#ring = Buffer.from(this.#ring.subarray(take));
    return out;
  }

  /**
   * Write raw bytes onto the connected serial line, BOUNDED by `timeoutMs`: this runs holding the
   * single Orchestrator lock, and QEMU applies real serial flow control, so an unbounded write
   * would block forever (wedging every other tool) if the guest is not draining its UART RX. On
   * overrun it rejects with a {@link SerialWriteTimeoutError}.
   */
  write(raw: Buffer, timeoutMs = SERIAL_WRITE_TIMEOUT_MS): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      let settled = false;
      const done = (err?: Error): void => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        this.#socket.off('error', onError);
        if (err) reject(err);
        else resolve();
      };
      const onError = (err: Error): void => done(err);
      const timer = setTimeout(
        () => done(new SerialWriteTimeoutError('serial write timed out')),
        timeoutMs,
      );
      this.#socket.once('error', onError);
      // The write callback fires once the bytes are flushed to the kernel (our "done"); if the
      // kernel buffer is full because the guest is not reading, it never fires and the timer wins.
      this.#socket.write(raw, (err) => done(err ?? undefined));
    });
  }

  /** Close the connection (aborting the reader) and unlink the server-owned socket file. */
  async teardown(): Promise<void> {
    this.#socket.destroy();
    await rm(this.socketPath, { force: true }).catch(() => {});
    logger.debug(`serial bridge torn down (${this.socketPath})`);
  }
}

/** Options for {@link attachSerialBridge}; the `connect` seam lets tests inject a fake socket. */
export interface AttachSerialBridgeOptions {
  dialTimeoutMs?: number;
  dialIntervalMs?: number;
  connect?: (path: string) => Promise<Socket>;
}

/** Default connector: open a UNIX-socket client connection, resolving on `connect`. */
function defaultConnect(path: string): Promise<Socket> {
  return new Promise<Socket>((resolve, reject) => {
    const socket = createConnection(path);
    const onError = (err: Error): void => {
      socket.destroy();
      reject(err);
    };
    socket.once('error', onError);
    socket.once('connect', () => {
      socket.off('error', onError);
      resolve(socket);
    });
  });
}

const delay = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * Dial QEMU's serial `socket` chardev (the `serial.sock` sibling of `qmpSocketPath`), retrying
 * briefly until it accepts, then return a live {@link SerialBridge} that is already capturing.
 * Called during launch BEFORE `cont`, so the connection is up before the guest emits a byte and no
 * boot output is lost. Throws (fail-closed) if the socket never accepts within the dial timeout.
 * Mirrors the Rust `attach_serial_bridge`.
 */
export async function attachSerialBridge(
  qmpSocketPath: string,
  bufferBytes: number,
  options: AttachSerialBridgeOptions = {},
): Promise<SerialBridge> {
  const socketPath = serialSocketPath(qmpSocketPath);
  const timeoutMs = options.dialTimeoutMs ?? SERIAL_DIAL_TIMEOUT_MS;
  const intervalMs = options.dialIntervalMs ?? SERIAL_DIAL_INTERVAL_MS;
  const connect = options.connect ?? defaultConnect;
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    try {
      const socket = await connect(socketPath);
      logger.debug(`serial bridge attached (${socketPath})`);
      return new SerialBridge(socket, bufferBytes, socketPath);
    } catch (err) {
      if (Date.now() >= deadline) {
        const message =
          `Failed to connect to the Serial Port socket at ${socketPath} within ${timeoutMs}ms: ` +
          `${err instanceof Error ? err.message : String(err)}. The socket backend needs QEMU's ` +
          'server chardev to be listening.';
        logger.warning(message);
        throw new Error(message);
      }
      await delay(intervalMs);
    }
  }
}

/** Tear a bridge down (mirrors the Rust free function): close the connection and unlink the socket. */
export async function teardownSerialBridge(bridge: SerialBridge): Promise<void> {
  await bridge.teardown();
}
