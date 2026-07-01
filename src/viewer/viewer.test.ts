import { mkdtemp, rm, symlink, writeFile } from 'node:fs/promises';
import { type AddressInfo, createServer, type Server as NetServer, type Socket } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { WebSocket } from 'ws';
import { logger } from '../logger.js';
import {
  MAX_VIEWER_CONNECTIONS,
  resolveAsset,
  startViewer,
  type Viewer,
  type ViewerOptions,
} from './viewer.js';

const PASSWORD = 'test-viewer-secret';

/** An HTTP Basic header carrying `password` (the username half is ignored). */
function basicAuth(password: string, user = 'viewer'): string {
  return `Basic ${Buffer.from(`${user}:${password}`).toString('base64')}`;
}

// Track every resource so a failing assertion can never leak a listener/socket and
// hang vitest. afterEach tears them all down.
const openViewers: Viewer[] = [];
const openServers: NetServer[] = [];
const openClients: WebSocket[] = [];
const openSockets: Socket[] = [];

afterEach(async () => {
  for (const ws of openClients.splice(0)) ws.close();
  for (const sock of openSockets.splice(0)) sock.destroy();
  for (const viewer of openViewers.splice(0)) await viewer.stop().catch(() => undefined);
  for (const server of openServers.splice(0)) {
    await new Promise<void>((resolve) => server.close(() => resolve()));
  }
});

/** Start a Viewer on an ephemeral port with test defaults, tracked for cleanup. */
async function launchViewer(overrides: Partial<ViewerOptions> = {}): Promise<Viewer> {
  const viewer = await startViewer({
    host: '127.0.0.1',
    port: 0,
    password: PASSWORD,
    // Auth-only tests never proxy, so a dead default port is fine; proxy tests override.
    vncHost: '127.0.0.1',
    vncPort: 1,
    vncPassword: 'vncpw123',
    ...overrides,
  });
  openViewers.push(viewer);
  return viewer;
}

/**
 * A mock TCP server standing in for the loopback VNC port. It records the bytes it
 * receives and lets a test greet each new connection.
 */
function mockVncServer(): {
  server: NetServer;
  received: Buffer[];
  connections: Socket[];
  greet: (bytes: Buffer) => void;
  port: () => number;
} {
  const received: Buffer[] = [];
  const connections: Socket[] = [];
  let greeting: Buffer | undefined;
  const server = createServer((sock) => {
    connections.push(sock);
    openSockets.push(sock);
    sock.on('error', () => undefined);
    sock.on('data', (chunk) => received.push(chunk));
    if (greeting !== undefined) sock.write(greeting);
  });
  server.on('error', () => undefined);
  openServers.push(server);
  return {
    server,
    received,
    connections,
    greet: (bytes) => {
      greeting = bytes;
    },
    port: () => (server.address() as AddressInfo).port,
  };
}

function listen(server: NetServer): Promise<void> {
  return new Promise((resolve) => server.listen(0, '127.0.0.1', () => resolve()));
}

async function waitFor(predicate: () => boolean, timeoutMs = 2000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error('waitFor timed out');
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}

describe('Viewer authentication gate (ADR-0010)', () => {
  it('refuses the page without credentials (401) and prompts with WWW-Authenticate', async () => {
    const viewer = await launchViewer();
    const res = await fetch(`http://127.0.0.1:${viewer.port}/`);
    expect(res.status).toBe(401);
    expect(res.headers.get('www-authenticate')).toMatch(/Basic/i);
  });

  it('refuses the page with a wrong password (401)', async () => {
    const viewer = await launchViewer();
    const res = await fetch(`http://127.0.0.1:${viewer.port}/`, {
      headers: { Authorization: basicAuth('wrong-password') },
    });
    expect(res.status).toBe(401);
  });

  it('serves the noVNC page with the correct password, embedding the VNC password', async () => {
    const viewer = await launchViewer({ vncPassword: 'embedded9' });
    const res = await fetch(`http://127.0.0.1:${viewer.port}/`, {
      headers: { Authorization: basicAuth(PASSWORD) },
    });
    expect(res.status).toBe(200);
    const body = await res.text();
    // The page loads the noVNC library from the confined asset prefix...
    expect(body).toContain('/novnc/core/rfb.js');
    // ...and embeds the server-generated VNC password so noVNC auto-authenticates.
    expect(body).toContain('"embedded9"');
  });
});

describe('Viewer static serving is confined to the noVNC assets (ADR-0010)', () => {
  it('serves the noVNC RFB module only with auth', async () => {
    const viewer = await launchViewer();
    const noauth = await fetch(`http://127.0.0.1:${viewer.port}/novnc/core/rfb.js`);
    expect(noauth.status).toBe(401);

    const ok = await fetch(`http://127.0.0.1:${viewer.port}/novnc/core/rfb.js`, {
      headers: { Authorization: basicAuth(PASSWORD) },
    });
    expect(ok.status).toBe(200);
    expect(ok.headers.get('content-type')).toMatch(/javascript/);
    // rfb.js is the noVNC client module.
    expect(await ok.text()).toContain('RFB');
  });

  it('refuses a path-traversal escape out of the noVNC package dir (403)', async () => {
    const viewer = await launchViewer();
    // Percent-encoded ../../package.json so the client does not normalise it away.
    const res = await fetch(
      `http://127.0.0.1:${viewer.port}/novnc/%2e%2e%2f%2e%2e%2fpackage.json`,
      { headers: { Authorization: basicAuth(PASSWORD) } },
    );
    expect(res.status).toBe(403);
    // It never leaked a file from outside the noVNC assets (e.g. our package.json).
    expect(await res.text()).not.toContain('qmp-mcp');
  });
});

describe('Viewer websocket proxy to the loopback VNC port (ADR-0010)', () => {
  it('relays bytes in BOTH directions between the browser ws and the VNC port', async () => {
    const mock = mockVncServer();
    await listen(mock.server);
    const fromVnc = Buffer.from([0x52, 0x46, 0x42, 0x01]); // "RFB\x01"
    mock.greet(fromVnc);

    const viewer = await launchViewer({ vncPort: mock.port() });
    const ws = new WebSocket(`ws://127.0.0.1:${viewer.port}/websockify`, {
      headers: { Authorization: basicAuth(PASSWORD) },
    });
    openClients.push(ws);

    // VNC -> browser: the mock's greeting arrives at the ws client.
    const received = await new Promise<Buffer>((resolve, reject) => {
      ws.once('message', (data) => resolve(data as Buffer));
      ws.once('error', reject);
    });
    expect(Buffer.from(received)).toEqual(fromVnc);

    // browser -> VNC: bytes the ws client sends reach the mock.
    const fromBrowser = Buffer.from([0x01, 0x02, 0x03, 0x04]);
    ws.send(fromBrowser);
    await waitFor(() => Buffer.concat(mock.received).length >= fromBrowser.length);
    expect(Buffer.concat(mock.received)).toEqual(fromBrowser);
  });

  it('always dials the CONFIGURED loopback port, ignoring any client-supplied target', async () => {
    const target = mockVncServer(); // the server-controlled target
    await listen(target.server);
    target.greet(Buffer.from([0x99]));
    const trap = mockVncServer(); // must NEVER be dialed
    await listen(trap.server);

    const viewer = await launchViewer({ vncPort: target.port() });
    // The client tries to redirect the proxy via the path + query — it must be ignored.
    const ws = new WebSocket(
      `ws://127.0.0.1:${viewer.port}/websockify?host=127.0.0.1&port=${trap.port()}`,
      { headers: { Authorization: basicAuth(PASSWORD) } },
    );
    openClients.push(ws);

    const message = await new Promise<Buffer>((resolve, reject) => {
      ws.once('message', (data) => resolve(data as Buffer));
      ws.once('error', reject);
    });
    // Reached the configured target...
    expect(Buffer.from(message)).toEqual(Buffer.from([0x99]));
    expect(target.connections.length).toBe(1);
    // ...and never the client-supplied one.
    expect(trap.connections.length).toBe(0);
  });

  it('refuses an unauthenticated websocket upgrade and never opens a proxy connection', async () => {
    const mock = mockVncServer();
    await listen(mock.server);
    const viewer = await launchViewer({ vncPort: mock.port() });

    const ws = new WebSocket(`ws://127.0.0.1:${viewer.port}/websockify`); // no auth
    openClients.push(ws);
    const outcome = await new Promise<Error>((resolve) => {
      ws.once('error', resolve);
      ws.once('open', () => resolve(new Error('unexpectedly opened')));
    });
    expect(outcome).toBeInstanceOf(Error);
    // The upgrade was rejected before any dial to the VNC port.
    expect(mock.connections.length).toBe(0);
  });
});

describe('Viewer lifecycle (ADR-0010)', () => {
  it('binds a real HTTP port and stop() closes it', async () => {
    const viewer = await launchViewer();
    const before = await fetch(`http://127.0.0.1:${viewer.port}/`, {
      headers: { Authorization: basicAuth(PASSWORD) },
    });
    expect(before.status).toBe(200);

    await viewer.stop();

    // Once stopped, the port no longer accepts connections.
    await expect(
      fetch(`http://127.0.0.1:${viewer.port}/`, {
        headers: { Authorization: basicAuth(PASSWORD) },
      }),
    ).rejects.toThrow();
  });

  it('refuses to start with an empty password (fail-closed)', async () => {
    await expect(
      startViewer({
        host: '127.0.0.1',
        port: 0,
        password: '',
        vncHost: '127.0.0.1',
        vncPort: 1,
        vncPassword: 'x',
      }),
    ).rejects.toThrow(/QMP_MCP_VIEWER_PASSWORD/);
  });
});

describe('Viewer anti-clickjacking headers (F2)', () => {
  it('sends X-Frame-Options: DENY and CSP frame-ancestors on the page', async () => {
    const viewer = await launchViewer();
    const res = await fetch(`http://127.0.0.1:${viewer.port}/`, {
      headers: { Authorization: basicAuth(PASSWORD) },
    });
    expect(res.headers.get('x-frame-options')).toBe('DENY');
    expect(res.headers.get('content-security-policy')).toBe("frame-ancestors 'none'");
  });

  it('sends the anti-framing headers on a static asset response too', async () => {
    const viewer = await launchViewer();
    const res = await fetch(`http://127.0.0.1:${viewer.port}/novnc/core/rfb.js`, {
      headers: { Authorization: basicAuth(PASSWORD) },
    });
    expect(res.headers.get('x-frame-options')).toBe('DENY');
    expect(res.headers.get('content-security-policy')).toBe("frame-ancestors 'none'");
  });
});

describe('Viewer websocket same-origin guard (F2)', () => {
  it('allows a same-origin upgrade (Origin authority matches Host)', async () => {
    const mock = mockVncServer();
    await listen(mock.server);
    mock.greet(Buffer.from([0x42]));
    const viewer = await launchViewer({ vncPort: mock.port() });
    const ws = new WebSocket(`ws://127.0.0.1:${viewer.port}/websockify`, {
      headers: {
        Authorization: basicAuth(PASSWORD),
        Origin: `http://127.0.0.1:${viewer.port}`,
      },
    });
    openClients.push(ws);
    const message = await new Promise<Buffer>((resolve, reject) => {
      ws.once('message', (data) => resolve(data as Buffer));
      ws.once('error', reject);
    });
    expect(Buffer.from(message)).toEqual(Buffer.from([0x42]));
    expect(mock.connections.length).toBe(1);
  });

  it('allows an upgrade with NO Origin header (non-browser client)', async () => {
    const mock = mockVncServer();
    await listen(mock.server);
    mock.greet(Buffer.from([0x43]));
    const viewer = await launchViewer({ vncPort: mock.port() });
    const ws = new WebSocket(`ws://127.0.0.1:${viewer.port}/websockify`, {
      headers: { Authorization: basicAuth(PASSWORD) },
    });
    openClients.push(ws);
    const message = await new Promise<Buffer>((resolve, reject) => {
      ws.once('message', (data) => resolve(data as Buffer));
      ws.once('error', reject);
    });
    expect(Buffer.from(message)).toEqual(Buffer.from([0x43]));
  });

  it('rejects a cross-origin upgrade (Origin mismatch) with 403 and never dials VNC', async () => {
    const mock = mockVncServer();
    await listen(mock.server);
    const viewer = await launchViewer({ vncPort: mock.port() });
    const ws = new WebSocket(`ws://127.0.0.1:${viewer.port}/websockify`, {
      headers: { Authorization: basicAuth(PASSWORD), Origin: 'http://evil.example' },
    });
    openClients.push(ws);
    const status = await new Promise<number>((resolve) => {
      ws.once('unexpected-response', (_req, res) => {
        res.resume();
        resolve(res.statusCode ?? 0);
      });
      ws.once('open', () => resolve(101));
      ws.once('error', () => resolve(-1));
    });
    expect(status).toBe(403);
    expect(mock.connections.length).toBe(0);
  });
});

describe('Viewer websocket path enforcement (F6)', () => {
  it('rejects an authenticated upgrade on an unexpected path (404) and never dials VNC', async () => {
    const mock = mockVncServer();
    await listen(mock.server);
    const viewer = await launchViewer({ vncPort: mock.port() });
    const ws = new WebSocket(`ws://127.0.0.1:${viewer.port}/not-websockify`, {
      headers: { Authorization: basicAuth(PASSWORD) },
    });
    openClients.push(ws);
    const status = await new Promise<number>((resolve) => {
      ws.once('unexpected-response', (_req, res) => {
        res.resume();
        resolve(res.statusCode ?? 0);
      });
      ws.once('open', () => resolve(101));
      ws.once('error', () => resolve(-1));
    });
    expect(status).toBe(404);
    expect(mock.connections.length).toBe(0);
  });
});

describe('Viewer connection cap (F5)', () => {
  it(`refuses the ${MAX_VIEWER_CONNECTIONS + 1}th authenticated connection with 503`, async () => {
    const mock = mockVncServer();
    await listen(mock.server);
    const viewer = await launchViewer({ vncPort: mock.port() });

    // Fill the cap with open, authenticated connections and keep them alive.
    for (let i = 0; i < MAX_VIEWER_CONNECTIONS; i++) {
      const ws = new WebSocket(`ws://127.0.0.1:${viewer.port}/websockify`, {
        headers: { Authorization: basicAuth(PASSWORD) },
      });
      openClients.push(ws);
      await new Promise<void>((resolve, reject) => {
        ws.once('open', () => resolve());
        ws.once('error', reject);
      });
    }
    // Every held connection reached the loopback VNC port.
    await waitFor(() => mock.connections.length === MAX_VIEWER_CONNECTIONS);

    // The next authenticated upgrade is over the cap: 503, and it never dials VNC.
    const extra = new WebSocket(`ws://127.0.0.1:${viewer.port}/websockify`, {
      headers: { Authorization: basicAuth(PASSWORD) },
    });
    openClients.push(extra);
    const status = await new Promise<number>((resolve) => {
      extra.once('unexpected-response', (_req, res) => {
        res.resume();
        resolve(res.statusCode ?? 0);
      });
      extra.once('open', () => resolve(101));
      extra.once('error', () => resolve(-1));
    });
    expect(status).toBe(503);
    expect(mock.connections.length).toBe(MAX_VIEWER_CONNECTIONS);
  });
});

describe('Viewer asset realpath containment (F4)', () => {
  it('refuses a symlink inside the package dir that escapes it, serves a real file, 404s a miss', async () => {
    const pkgDir = await mkdtemp(join(tmpdir(), 'viewer-pkg-'));
    const outsideDir = await mkdtemp(join(tmpdir(), 'viewer-out-'));
    try {
      const secret = join(outsideDir, 'secret.txt');
      await writeFile(secret, 'top secret outside the package');
      // A symlink INSIDE the package dir whose target escapes it.
      await symlink(secret, join(pkgDir, 'escape.txt'));
      // A legitimate file inside the package dir.
      await writeFile(join(pkgDir, 'ok.txt'), 'hello');

      // The symlink escape is refused by the realpath re-check (string check passes).
      await expect(resolveAsset(pkgDir, 'escape.txt')).resolves.toEqual({
        ok: false,
        status: 403,
      });
      // A real file inside the dir resolves.
      const good = await resolveAsset(pkgDir, 'ok.txt');
      expect(good.ok).toBe(true);
      // A missing file is a 404, not a leak.
      await expect(resolveAsset(pkgDir, 'nope.txt')).resolves.toEqual({ ok: false, status: 404 });
    } finally {
      await rm(pkgDir, { recursive: true, force: true });
      await rm(outsideDir, { recursive: true, force: true });
    }
  });
});

describe('Viewer cleartext-bind warning (F1)', () => {
  it('logs a stderr WARNING when binding a non-loopback host', async () => {
    const spy = vi.spyOn(logger, 'warning').mockImplementation(() => undefined);
    try {
      await launchViewer({ host: '0.0.0.0' });
      expect(spy).toHaveBeenCalledWith(expect.stringContaining('cleartext'));
      expect(spy).toHaveBeenCalledWith(expect.stringContaining('0.0.0.0'));
    } finally {
      spy.mockRestore();
    }
  });

  it('does not warn when binding a loopback host', async () => {
    const spy = vi.spyOn(logger, 'warning').mockImplementation(() => undefined);
    try {
      await launchViewer({ host: '127.0.0.1' });
      expect(spy).not.toHaveBeenCalledWith(expect.stringContaining('cleartext'));
    } finally {
      spy.mockRestore();
    }
  });
});
