import { existsSync } from 'node:fs';
import { mkdtemp, rm } from 'node:fs/promises';
import { createServer, type Server, type Socket } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { serialSocketPath } from './hardware-spec.js';
import {
  attachSerialBridge,
  pushBounded,
  SerialBridge,
  SerialWriteTimeoutError,
  teardownSerialBridge,
} from './serial-bridge.js';

const wait = (ms: number): Promise<void> => new Promise((r) => setTimeout(r, ms));

/** Poll `fn` until it returns a truthy value or the deadline passes. */
async function waitFor<T>(fn: () => T | undefined, timeoutMs = 1000): Promise<T | undefined> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const v = fn();
    if (v) return v;
    if (Date.now() >= deadline) return undefined;
    await wait(5);
  }
}

describe('pushBounded — the overwrite-oldest ring (ADR-0015)', () => {
  it('accumulates below the cap and evicts the oldest past it', () => {
    let ring = Buffer.alloc(0);
    ring = pushBounded(ring, Buffer.from('abc'), 10);
    expect(ring.toString()).toBe('abc');
    ring = pushBounded(ring, Buffer.from('def'), 10);
    expect(ring.toString()).toBe('abcdef');
    // Overflow: keep only the last `cap` bytes (oldest dropped).
    ring = pushBounded(ring, Buffer.from('0123456789'), 4);
    expect(ring.toString()).toBe('6789');
    // A single chunk larger than the cap keeps only its tail.
    expect(pushBounded(Buffer.alloc(0), Buffer.from('LONGCHUNK'), 4).toString()).toBe('HUNK');
  });
});

describe('SerialBridge over a real UNIX socket (ADR-0015)', () => {
  let dir: string;
  let qmpSock: string;
  let serialSock: string;
  let server: Server | undefined;
  let bridge: SerialBridge | undefined;
  let serverConn: Socket | undefined;
  let received: Buffer[];

  beforeEach(async () => {
    dir = await mkdtemp(join(tmpdir(), 'qmp-serialsock-'));
    qmpSock = join(dir, 'qmp.sock');
    serialSock = serialSocketPath(qmpSock);
    received = [];
  });

  afterEach(async () => {
    if (bridge) await teardownSerialBridge(bridge);
    bridge = undefined;
    await new Promise<void>((resolve) => (server ? server.close(() => resolve()) : resolve()));
    server = undefined;
    await rm(dir, { recursive: true, force: true });
  });

  /** Start a fake-QEMU UNIX server on `serialSock`; `greeting`, if set, is written on connect. */
  async function startServer(greeting?: string): Promise<void> {
    server = createServer((conn) => {
      serverConn = conn;
      conn.on('data', (c: Buffer) => received.push(c));
      if (greeting !== undefined) conn.write(Buffer.from(greeting));
    });
    await new Promise<void>((resolve) => server?.listen(serialSock, resolve));
  }

  it('dials the serial socket, captures output, and drains it destructively', async () => {
    await startServer('BOOTLOG');
    bridge = await attachSerialBridge(qmpSock, 1 << 20);
    const captured = await waitFor(() => {
      const out = bridge?.drain(1024);
      return out && out.length > 0 ? out : undefined;
    });
    expect(captured?.toString()).toBe('BOOTLOG');
    // Drain is destructive: nothing left.
    expect(bridge?.drain(1024).length).toBe(0);
  });

  it('drains only up to the requested size, from the oldest end', async () => {
    await startServer('0123456789');
    bridge = await attachSerialBridge(qmpSock, 1 << 20);
    await waitFor(() => (serverConn ? true : undefined));
    await wait(20); // let the greeting land
    expect(bridge?.drain(4).toString()).toBe('0123');
    expect(bridge?.drain(1024).toString()).toBe('456789');
  });

  it('writes input onto the connected serial line (reaches the guest)', async () => {
    await startServer();
    bridge = await attachSerialBridge(qmpSock, 1 << 20);
    await bridge.write(Buffer.from('login\n'));
    const got = await waitFor(() =>
      Buffer.concat(received).toString() === 'login\n' ? true : undefined,
    );
    expect(got).toBe(true);
  });

  it('unlinks the server-owned socket on teardown', async () => {
    await startServer();
    bridge = await attachSerialBridge(qmpSock, 1 << 20);
    expect(existsSync(serialSock)).toBe(true);
    await teardownSerialBridge(bridge);
    bridge = undefined;
    expect(existsSync(serialSock)).toBe(false);
  });

  it('fails closed with an actionable message when nothing is listening', async () => {
    // No server → dial never succeeds; a short timeout keeps the test fast.
    await expect(
      attachSerialBridge(qmpSock, 1 << 20, { dialTimeoutMs: 80, dialIntervalMs: 10 }),
    ).rejects.toThrow(/Failed to connect to the Serial Port socket.*within 80ms/s);
  });
});

describe('SerialBridge.write timeout (ADR-0015)', () => {
  it('rejects with a SerialWriteTimeoutError when the write never flushes', async () => {
    // A stub socket whose write callback never fires (the guest is not draining its UART RX) —
    // deterministic, no dependence on OS buffer sizes: the timer must win. It exposes only the
    // methods SerialBridge touches (on/once/off/write/destroy).
    const stuck = {
      on() {},
      once() {},
      off() {},
      write() {
        return false;
      },
      destroy() {},
    } as unknown as Socket;
    const bridge = new SerialBridge(stuck, 1 << 20, '/tmp/never.sock');
    await expect(bridge.write(Buffer.from('data'), 30)).rejects.toBeInstanceOf(
      SerialWriteTimeoutError,
    );
  });
});
