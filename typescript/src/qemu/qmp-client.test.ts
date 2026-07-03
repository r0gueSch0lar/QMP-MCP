import { mkdtemp, rm } from 'node:fs/promises';
import { createServer, type Server, type Socket } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { QmpClient, QmpCommandError, QmpConnectionError } from './qmp-client.js';

/**
 * A scripted stand-in for the QEMU side of a QMP socket: it sends the greeting
 * on connect and answers commands from a small table, so the client's NDJSON
 * parsing, handshake, id-correlation, error-mapping and event capture are
 * exercised without a real QEMU.
 */
/** Tunes the fake QEMU's misbehaviours for the connection-failure tests. */
interface FakeQmpServerOptions {
  /** Accept the connection but never send the QMP greeting. */
  withholdGreeting?: boolean;
  /** Commands the server silently drops (never answers), to exercise timeouts. */
  mute?: string[];
}

class FakeQmpServer {
  #server: Server;
  #dir!: string;
  #options: FakeQmpServerOptions;
  socketPath!: string;
  lastSocket?: Socket;

  constructor(options: FakeQmpServerOptions = {}) {
    this.#options = options;
    this.#server = createServer((socket) => {
      this.lastSocket = socket;
      socket.setEncoding('utf8');
      if (!this.#options.withholdGreeting) {
        socket.write(
          `${JSON.stringify({ QMP: { version: { qemu: { major: 7 } }, capabilities: [] } })}\n`,
        );
      }
      let buffer = '';
      socket.on('data', (chunk: string) => {
        buffer += chunk;
        let nl = buffer.indexOf('\n');
        while (nl !== -1) {
          const line = buffer.slice(0, nl).trim();
          buffer = buffer.slice(nl + 1);
          if (line) this.#handle(socket, JSON.parse(line));
          nl = buffer.indexOf('\n');
        }
      });
    });
  }

  async start(): Promise<void> {
    this.#dir = await mkdtemp(join(tmpdir(), 'qmp-client-test-'));
    this.socketPath = join(this.#dir, 'qmp.sock');
    await new Promise<void>((resolve) => this.#server.listen(this.socketPath, resolve));
  }

  async stop(): Promise<void> {
    await new Promise<void>((resolve) => this.#server.close(() => resolve()));
    await rm(this.#dir, { recursive: true, force: true });
  }

  /** Push an async (id-less) QMP event to the connected client. */
  emit(event: string, data?: Record<string, unknown>): void {
    this.lastSocket?.write(`${JSON.stringify({ event, data })}\n`);
  }

  #handle(
    socket: Socket,
    message: { execute?: string; id?: number; arguments?: Record<string, unknown> },
  ): void {
    const { execute, id } = message;
    if (execute && this.#options.mute?.includes(execute)) {
      return; // never answer — used to exercise the per-command timeout and close-while-pending
    }
    if (execute === 'qmp_capabilities') {
      socket.write(`${JSON.stringify({ return: {}, id })}\n`);
    } else if (execute === 'query-status') {
      socket.write(`${JSON.stringify({ return: { status: 'running', running: true }, id })}\n`);
    } else if (execute === 'echo') {
      // Echo the args back, optionally after a delay, so two echoes can be made to
      // resolve out of send order while each still carries its own id.
      const args = message.arguments ?? {};
      const delayMs = typeof args.delayMs === 'number' ? args.delayMs : 0;
      setTimeout(() => {
        socket.write(`${JSON.stringify({ return: { tag: args.tag }, id })}\n`);
      }, delayMs);
    } else {
      socket.write(
        `${JSON.stringify({
          error: { class: 'CommandNotFound', desc: `unknown command: ${execute}` },
          id,
        })}\n`,
      );
    }
  }
}

describe('QmpClient', () => {
  let server: FakeQmpServer;

  beforeEach(async () => {
    server = new FakeQmpServer();
    await server.start();
  });

  afterEach(async () => {
    await server.stop();
  });

  it('completes the greeting -> qmp_capabilities handshake', async () => {
    const client = await QmpClient.dial(server.socketPath);
    await client.negotiate();
    expect(client.greeting?.capabilities).toEqual([]);
    await client.close();
  });

  it('correlates a command response by id', async () => {
    const client = await QmpClient.dial(server.socketPath);
    await client.negotiate();
    expect(await client.execute('query-status')).toEqual({ status: 'running', running: true });
    await client.close();
  });

  it('maps a QMP {error} response to a thrown QmpCommandError', async () => {
    const client = await QmpClient.dial(server.socketPath);
    await client.negotiate();
    await expect(client.execute('no-such-command')).rejects.toBeInstanceOf(QmpCommandError);
    await expect(client.execute('no-such-command')).rejects.toMatchObject({
      errorClass: 'CommandNotFound',
      command: 'no-such-command',
    });
    await client.close();
  });

  it('captures asynchronous events', async () => {
    const client = await QmpClient.dial(server.socketPath);
    await client.negotiate();
    const events: string[] = [];
    client.onEvent((e) => events.push(e.event));
    const seen = new Promise<void>((resolve) => client.onEvent(() => resolve()));
    server.emit('STOP');
    await seen;
    expect(events).toContain('STOP');
    await client.close();
  });

  it('rejects commands issued after the connection is closed', async () => {
    const client = await QmpClient.dial(server.socketPath);
    await client.negotiate();
    await client.close();
    await expect(client.execute('query-status')).rejects.toBeInstanceOf(QmpConnectionError);
  });

  it('rejects an in-flight command with QmpConnectionError when the connection closes', async () => {
    const muted = new FakeQmpServer({ mute: ['never-answered'] });
    await muted.start();
    try {
      const client = await QmpClient.dial(muted.socketPath);
      await client.negotiate();
      // Issue a command the server never answers, then tear the socket down while
      // it is still pending: the in-flight promise must reject (not hang).
      const pending = client.execute('never-answered');
      const assertion = expect(pending).rejects.toBeInstanceOf(QmpConnectionError);
      await client.close();
      await assertion;
    } finally {
      await muted.stop();
    }
  });

  it('rejects a command that is never answered after the per-command timeout', async () => {
    const muted = new FakeQmpServer({ mute: ['never-answered'] });
    await muted.start();
    try {
      const client = await QmpClient.dial(muted.socketPath);
      await client.negotiate();
      await expect(client.execute('never-answered', undefined, 50)).rejects.toBeInstanceOf(
        QmpConnectionError,
      );
      await client.close();
    } finally {
      await muted.stop();
    }
  });

  it('correlates out-of-order responses to the right command by id', async () => {
    const client = await QmpClient.dial(server.socketPath);
    await client.negotiate();
    // `first` is sent first but answered last; each promise must still receive the
    // value carrying its own id.
    const first = client.execute('echo', { tag: 'first', delayMs: 40 });
    const second = client.execute('echo', { tag: 'second', delayMs: 0 });
    expect(await second).toEqual({ tag: 'second' });
    expect(await first).toEqual({ tag: 'first' });
    await client.close();
  });

  it('rejects negotiate with QmpConnectionError when the greeting never arrives', async () => {
    const silent = new FakeQmpServer({ withholdGreeting: true });
    await silent.start();
    try {
      const client = await QmpClient.dial(silent.socketPath);
      await expect(client.negotiate(50)).rejects.toBeInstanceOf(QmpConnectionError);
      await client.close();
    } finally {
      await silent.stop();
    }
  });
});
