/**
 * A minimal QMP (QEMU Machine Protocol) client. QMP is QEMU's own JSON protocol
 * spoken over a UNIX socket — newline-delimited JSON objects in both directions.
 * It is NOT part of mcp-framework; this is a hand-rolled client.
 *
 * Protocol shape:
 *   - On connect the server sends a greeting:  {"QMP": {"version": ..., ...}}
 *   - The client leaves negotiation mode by sending `qmp_capabilities`.
 *   - Commands carry an `id`; responses echo it as {"return": ...} or
 *     {"error": {"class", "desc"}}, letting us correlate concurrent requests.
 *   - Asynchronous {"event": ...} messages arrive at any time, unsolicited.
 *
 * The client is transport-injectable: it wraps any duplex stream (a real
 * `net.Socket`, or an in-memory pair in tests), so the request/response and
 * event-parsing logic is exercisable without a real QEMU.
 */

import { EventEmitter } from 'node:events';
import { connect, type Socket } from 'node:net';

/** An asynchronous QMP event (e.g. SHUTDOWN, STOP, RESUME). */
export interface QmpEvent {
  event: string;
  data?: Record<string, unknown>;
  timestamp?: { seconds: number; microseconds: number };
}

/** The greeting object QEMU sends immediately on connection. */
export interface QmpGreeting {
  version: unknown;
  capabilities: unknown[];
}

/**
 * Raised when QEMU answers a command with `{"error": {...}}`. Carries the QMP
 * error class and the command name so callers can react or surface it.
 */
export class QmpCommandError extends Error {
  readonly errorClass: string;
  readonly command: string;
  constructor(command: string, errorClass: string, desc: string) {
    super(`QMP command "${command}" failed [${errorClass}]: ${desc}`);
    this.name = 'QmpCommandError';
    this.errorClass = errorClass;
    this.command = command;
  }
}

/** Raised when the connection closes (or times out) with requests outstanding. */
export class QmpConnectionError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'QmpConnectionError';
  }
}

interface Pending {
  command: string;
  resolve: (value: unknown) => void;
  reject: (reason: Error) => void;
  /** Per-command timeout; cleared once the command settles or the socket fails. */
  timer?: ReturnType<typeof setTimeout>;
}

const DEFAULT_NEGOTIATE_TIMEOUT_MS = 5_000;
/**
 * Per-command timeout. Bounds {@link QmpClient.execute} (and thus
 * `qmp_capabilities` during negotiation and `query-status`) so a QEMU that
 * greets then goes silent fails closed instead of hanging forever.
 */
const DEFAULT_COMMAND_TIMEOUT_MS = 10_000;

/**
 * Wraps a connected duplex socket and speaks QMP over it. Emits `'event'` for
 * each async QMP event and `'close'` when the underlying socket ends.
 */
export class QmpClient extends EventEmitter {
  #socket: Socket;
  #buffer = '';
  #nextId = 1;
  #pending = new Map<number, Pending>();
  #greeting?: QmpGreeting;
  #onGreeting?: (greeting: QmpGreeting) => void;
  #closed = false;
  #closeError?: Error;

  constructor(socket: Socket) {
    super();
    this.#socket = socket;
    socket.setEncoding('utf8');
    socket.on('data', (chunk: string) => this.#onData(chunk));
    socket.on('error', (err: Error) => this.#fail(err));
    socket.on('close', () => this.#fail());
  }

  /**
   * Dial a QMP UNIX socket and return a client wrapping the live connection.
   * The caller still drives {@link negotiate} to complete the handshake.
   */
  static async dial(socketPath: string): Promise<QmpClient> {
    const socket = await new Promise<Socket>((resolve, reject) => {
      const s = connect(socketPath);
      const onError = (err: Error): void => {
        s.removeListener('connect', onConnect);
        reject(err);
      };
      const onConnect = (): void => {
        s.removeListener('error', onError);
        resolve(s);
      };
      s.once('error', onError);
      s.once('connect', onConnect);
    });
    return new QmpClient(socket);
  }

  /** The greeting QEMU sent, available after {@link negotiate} resolves. */
  get greeting(): QmpGreeting | undefined {
    return this.#greeting;
  }

  /**
   * Complete the QMP handshake: wait for the greeting, then send
   * `qmp_capabilities` to leave negotiation mode and establish the QMP Session.
   */
  async negotiate(timeoutMs: number = DEFAULT_NEGOTIATE_TIMEOUT_MS): Promise<void> {
    await this.#waitForGreeting(timeoutMs);
    await this.execute('qmp_capabilities');
  }

  /**
   * Execute a QMP command, correlating the response by `id`. Resolves with the
   * command's `return` value, rejects with {@link QmpCommandError} when QEMU
   * reports an error, or rejects with {@link QmpConnectionError} when no response
   * arrives within `timeoutMs` (so a silent QEMU fails closed, not hangs).
   */
  execute(
    command: string,
    args?: Record<string, unknown>,
    timeoutMs: number = DEFAULT_COMMAND_TIMEOUT_MS,
  ): Promise<unknown> {
    if (this.#closed) {
      return Promise.reject(
        this.#closeError ?? new QmpConnectionError('QMP connection is closed.'),
      );
    }
    const id = this.#nextId++;
    const message: Record<string, unknown> = { execute: command, id };
    if (args !== undefined) message.arguments = args;
    return new Promise<unknown>((resolve, reject) => {
      const pending: Pending = { command, resolve, reject };
      if (Number.isFinite(timeoutMs) && timeoutMs > 0) {
        pending.timer = setTimeout(() => {
          // Only act if this command is still outstanding.
          if (this.#pending.get(id) !== pending) return;
          this.#pending.delete(id);
          reject(
            new QmpConnectionError(
              `QMP command "${command}" timed out after ${timeoutMs}ms with no response from QEMU.`,
            ),
          );
        }, timeoutMs);
        // Don't let the timeout itself keep the event loop alive; the open socket
        // already does while we genuinely wait, and #fail clears it on close.
        pending.timer.unref();
      }
      this.#pending.set(id, pending);
      this.#socket.write(`${JSON.stringify(message)}\n`, (err) => {
        if (err) {
          if (this.#pending.get(id) === pending) this.#pending.delete(id);
          if (pending.timer) clearTimeout(pending.timer);
          reject(err);
        }
      });
    });
  }

  /** Subscribe to async QMP events. Returns an unsubscribe function. */
  onEvent(listener: (event: QmpEvent) => void): () => void {
    this.on('event', listener);
    return () => this.off('event', listener);
  }

  /** Close the underlying socket. Outstanding requests reject. */
  async close(): Promise<void> {
    if (!this.#socket.destroyed) {
      await new Promise<void>((resolve) => {
        this.#socket.end(() => resolve());
        // end() may not fire its callback if the peer already vanished.
        this.#socket.once('close', () => resolve());
      });
    }
    this.#fail();
  }

  #waitForGreeting(timeoutMs: number): Promise<QmpGreeting> {
    if (this.#greeting) return Promise.resolve(this.#greeting);
    if (this.#closed) {
      return Promise.reject(
        this.#closeError ??
          new QmpConnectionError('QMP connection closed before the greeting arrived.'),
      );
    }
    return new Promise<QmpGreeting>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.#onGreeting = undefined;
        reject(
          new QmpConnectionError(`Timed out after ${timeoutMs}ms waiting for the QMP greeting.`),
        );
      }, timeoutMs);
      this.#onGreeting = (greeting) => {
        clearTimeout(timer);
        resolve(greeting);
      };
    });
  }

  #onData(chunk: string): void {
    this.#buffer += chunk;
    let newline = this.#buffer.indexOf('\n');
    while (newline !== -1) {
      const line = this.#buffer.slice(0, newline).trim();
      this.#buffer = this.#buffer.slice(newline + 1);
      if (line.length > 0) this.#dispatch(line);
      newline = this.#buffer.indexOf('\n');
    }
  }

  #dispatch(line: string): void {
    let message: Record<string, unknown>;
    try {
      message = JSON.parse(line) as Record<string, unknown>;
    } catch {
      // A malformed line is unexpected from QEMU; surface it but keep going.
      this.emit('parseError', new QmpConnectionError(`Could not parse QMP message: ${line}`));
      return;
    }

    if ('QMP' in message) {
      const qmp = message.QMP as { version?: unknown; capabilities?: unknown[] };
      this.#greeting = { version: qmp.version, capabilities: qmp.capabilities ?? [] };
      this.#onGreeting?.(this.#greeting);
      this.#onGreeting = undefined;
      return;
    }

    if ('event' in message) {
      this.emit('event', message as unknown as QmpEvent);
      return;
    }

    const id = typeof message.id === 'number' ? message.id : undefined;
    if (id === undefined) return;
    const pending = this.#pending.get(id);
    if (!pending) return;
    this.#pending.delete(id);
    if (pending.timer) clearTimeout(pending.timer);

    if ('error' in message) {
      const error = message.error as { class?: string; desc?: string };
      pending.reject(
        new QmpCommandError(pending.command, error.class ?? 'GenericError', error.desc ?? ''),
      );
      return;
    }
    pending.resolve(message.return);
  }

  /** Tear down: mark closed, reject all outstanding requests, emit `'close'`. */
  #fail(error?: Error): void {
    if (this.#closed) return;
    this.#closed = true;
    this.#closeError = error ?? new QmpConnectionError('QMP connection closed.');
    for (const pending of this.#pending.values()) {
      if (pending.timer) clearTimeout(pending.timer);
      pending.reject(this.#closeError);
    }
    this.#pending.clear();
    this.emit('close', error);
  }
}
