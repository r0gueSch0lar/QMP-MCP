/**
 * The noVNC browser Viewer (ADR-0010): an in-process, no-external-binary bridge
 * between a browser and the Guest's loopback VNC Display. It owns its OWN HTTP
 * server (independent of the MCP transport, so it works under `TRANSPORT=stdio`)
 * and does three things, all fail-closed behind a single human-facing secret
 * (`QMP_MCP_VIEWER_PASSWORD`):
 *
 *   1. Serves the noVNC static app from the `@novnc/novnc` npm package — static
 *      serving is CONFINED to that package directory (no path traversal).
 *   2. Refuses the page AND the websocket unless the request authenticates
 *      (HTTP Basic with the Viewer password). No password ⇒ it cannot serve.
 *   3. Upgrades an authenticated websocket and proxies its bytes to the SERVER's
 *      loopback VNC TCP port. The proxy target is ALWAYS the server-controlled
 *      loopback endpoint — never taken from the client.
 *
 * Because the page is only served post-auth, the server-generated VNC password is
 * embedded into the page so noVNC auto-authenticates: one human-facing secret, with
 * the internal VNC password still guarding the loopback VNC from other local
 * processes as a second layer.
 */

import { createHash, timingSafeEqual } from 'node:crypto';
import { readFile, realpath } from 'node:fs/promises';
import {
  createServer,
  type Server as HttpServer,
  type IncomingMessage,
  type ServerResponse,
} from 'node:http';
import { createRequire } from 'node:module';
import { connect } from 'node:net';
import { dirname, extname, resolve, sep } from 'node:path';
import type { Duplex } from 'node:stream';
import { type RawData, WebSocket, WebSocketServer } from 'ws';
import { logger } from '../logger.js';

/** The HTTP Basic realm shown to the browser when it prompts for credentials. */
const REALM = 'qmp-mcp viewer';

/** URL prefix the noVNC package assets are served under. */
const ASSET_PREFIX = '/novnc/';

/** URL path the browser opens its VNC websocket on. */
const WEBSOCKET_PATH = '/websockify';

/**
 * Cap on concurrent authenticated Viewer websocket connections. The Viewer fronts
 * a single Instance's Display, so a handful of viewers is plenty; capping bounds
 * the fan-out of relays a single leaked password could open. The N+1th upgrade is
 * refused with 503.
 */
export const MAX_VIEWER_CONNECTIONS = 2;

/**
 * Per-frame ceiling on inbound websocket messages (browser -> VNC). RFB client
 * messages are tiny (key/pointer events, small cut-text), so a modest cap bounds
 * the memory a single frame can pin without impeding normal use.
 */
const MAX_WS_PAYLOAD_BYTES = 4 * 1024 * 1024;

/**
 * Backpressure threshold: once a side's outbound buffer exceeds this, the other
 * side is paused until it drains, so a slow reader cannot make the relay buffer
 * without bound.
 */
const RELAY_HIGH_WATER_MARK = 1024 * 1024;

/**
 * The hosts treated as loopback. A bind to anything else is reachable off-box, so
 * the Viewer's cleartext Basic + interactive VNC would travel in the clear and we
 * warn (F1). Kept deliberately narrow — the exact set ADR-0010 documents as local.
 */
const LOOPBACK_HOSTS = new Set(['127.0.0.1', 'localhost', '::1', '[::1]']);

/** True when the Viewer's bind host is loopback (no cleartext-exposure warning needed). */
function isLoopbackHost(host: string): boolean {
  return LOOPBACK_HOSTS.has(host.trim().toLowerCase());
}

/**
 * Set the anti-clickjacking headers on every Viewer HTTP response (F2). Both say
 * the same thing to old and new browsers: this page may not be framed, so it cannot
 * be embedded by a malicious site to trick a click into the live Guest. Applied via
 * `setHeader` at the top of the handler so it rides along with every `writeHead`.
 */
function setAntiFramingHeaders(res: ServerResponse): void {
  res.setHeader('X-Frame-Options', 'DENY');
  res.setHeader('Content-Security-Policy', "frame-ancestors 'none'");
}

/**
 * Same-origin guard for the websocket upgrade (F2), mirroring the MCP transport's
 * origin allowlist intent. A browser sends `Origin`; a non-browser client (the case
 * the relay is normally driven by) sends none and is allowed. When present, the
 * Origin's authority must equal the request `Host` — i.e. the page must have been
 * served by this same Viewer — otherwise a cross-origin page is driving the upgrade
 * and it is refused.
 */
function originAllowed(req: IncomingMessage): boolean {
  const origin = req.headers.origin;
  // Omitted Origin ⇒ non-browser client; allow (same posture as the MCP CORS guard).
  if (origin === undefined) return true;
  const host = req.headers.host;
  if (host === undefined) return false;
  let originHost: string;
  try {
    originHost = new URL(origin).host;
  } catch {
    // An opaque/`null`/malformed Origin is not same-origin; refuse.
    return false;
  }
  return originHost === host;
}

/** Everything the Viewer needs to serve one Instance's Display. */
export interface ViewerOptions {
  /** Address the Viewer's HTTP server binds to (`QMP_MCP_VIEWER_HOST`). */
  host: string;
  /** TCP port the Viewer's HTTP server listens on (`QMP_MCP_VIEWER_PORT`). */
  port: number;
  /**
   * The human-facing gate (`QMP_MCP_VIEWER_PASSWORD`). The page and the websocket
   * are refused unless the request authenticates with it via HTTP Basic.
   */
  password: string;
  /** Loopback host of the Guest's VNC Display the proxy always dials. */
  vncHost: string;
  /** Loopback TCP port of the Guest's VNC Display the proxy always dials. */
  vncPort: number;
  /**
   * The server-generated VNC password, embedded into the (post-auth) page so noVNC
   * auto-authenticates. It never leaves an authenticated response.
   */
  vncPassword: string;
}

/** A running Viewer. Its lifetime equals the Instance's (ADR-0010). */
export interface Viewer {
  /** The address the HTTP server bound to. */
  readonly host: string;
  /** The port the HTTP server actually bound to (useful when `port` was 0). */
  readonly port: number;
  /** Stop the Viewer: drop websocket clients and close the HTTP server. Idempotent. */
  stop(): Promise<void>;
}

/** Content types for the handful of extensions the noVNC assets use. */
const CONTENT_TYPES: Record<string, string> = {
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.html': 'text/html; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.map': 'application/json; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.wasm': 'application/wasm',
};

function contentTypeFor(path: string): string {
  return CONTENT_TYPES[extname(path).toLowerCase()] ?? 'application/octet-stream';
}

/**
 * Absolute path of the `@novnc/novnc` package directory, memoised. The package's
 * `exports` map points at `core/rfb.js`, so the package root is two levels up. This
 * is the ONLY directory static serving is allowed to read from.
 */
let novncDirCache: string | undefined;
function novncDir(): string {
  if (novncDirCache === undefined) {
    const require = createRequire(import.meta.url);
    novncDirCache = dirname(dirname(require.resolve('@novnc/novnc')));
  }
  return novncDirCache;
}

/** Hash a string to a fixed-length digest so comparisons are constant-time and length-safe. */
function digest(value: string): Buffer {
  return createHash('sha256').update(value).digest();
}

/** Constant-time password comparison (equal-length digests, so no length leak). */
function passwordMatches(expected: string, provided: string): boolean {
  return timingSafeEqual(digest(expected), digest(provided));
}

/**
 * Extract the password from an `Authorization: Basic` header, or undefined when it
 * is absent or malformed. The username half is ignored — the password is the secret.
 */
function basicPassword(req: IncomingMessage): string | undefined {
  const header = req.headers.authorization;
  if (header === undefined) return undefined;
  const [scheme, encoded] = header.split(' ');
  if (scheme === undefined || scheme.toLowerCase() !== 'basic' || encoded === undefined) {
    return undefined;
  }
  const decoded = Buffer.from(encoded, 'base64').toString('utf8');
  const colon = decoded.indexOf(':');
  return colon === -1 ? decoded : decoded.slice(colon + 1);
}

/** True only when the request presents the correct Viewer password via HTTP Basic. */
function isAuthenticated(req: IncomingMessage, password: string): boolean {
  const provided = basicPassword(req);
  return provided !== undefined && passwordMatches(password, provided);
}

/** Build the (post-auth) noVNC page with the server-generated VNC password embedded. */
function renderPage(vncPassword: string): string {
  // JSON.stringify safely embeds the (server-generated, alphanumeric) secret into a
  // script context. The browser-side `${...}` are escaped so Node does not evaluate
  // them here — only the password is interpolated on the server.
  const password = JSON.stringify(vncPassword);
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>qmp-mcp viewer</title>
<style>
  html, body { margin: 0; height: 100%; background: #282828; overflow: hidden; }
  #status { position: fixed; top: 0; left: 0; right: 0; z-index: 10; margin: 0; padding: 4px 10px;
            font: 13px/1.6 system-ui, sans-serif; color: #ddd; background: rgba(0, 0, 0, 0.6); }
  #screen { width: 100%; height: 100%; }
</style>
</head>
<body>
<p id="status">connecting…</p>
<div id="screen"></div>
<script type="module">
  import RFB from '${ASSET_PREFIX}core/rfb.js';
  const password = ${password};
  const status = document.getElementById('status');
  const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
  const url = \`\${scheme}://\${location.host}${WEBSOCKET_PATH}\`;
  const rfb = new RFB(document.getElementById('screen'), url, { credentials: { password } });
  rfb.viewOnly = false;
  rfb.scaleViewport = true;
  rfb.addEventListener('connect', () => { status.style.display = 'none'; });
  rfb.addEventListener('disconnect', (e) => {
    status.style.display = 'block';
    status.textContent = e.detail && e.detail.clean ? 'disconnected' : 'connection lost';
  });
  rfb.addEventListener('securityfailure', () => { status.textContent = 'authentication failed'; });
</script>
</body>
</html>
`;
}

/** Send a 401 that prompts the browser for the Viewer password. */
function sendUnauthorized(res: ServerResponse): void {
  res.writeHead(401, {
    'WWW-Authenticate': `Basic realm="${REALM}"`,
    'Content-Type': 'text/plain; charset=utf-8',
  });
  res.end('Authentication required.\n');
}

/** Outcome of {@link resolveAsset}: a safe canonical path, or the status to reject with. */
type AssetResolution = { ok: true; path: string } | { ok: false; status: 400 | 403 | 404 };

/**
 * Resolve a request-relative asset name to a safe, canonical path inside `dir`, or
 * a rejection status. CONFINED to `dir` by two independent checks:
 *
 *  1. String containment: the decoded name resolved against `dir` must stay inside
 *     it — rejects absolute paths and `..` traversal (403) before touching disk.
 *  2. Realpath containment (defense-in-depth, F4): the target and `dir` are both
 *     canonicalised (symlinks resolved) and the real target must STILL be inside
 *     the real `dir`, so a symlink planted INSIDE the dependency cannot escape it.
 *     A missing target is 404 (ENOENT), mirroring the Image Store's boundary
 *     (`src/instance/store-path.ts`).
 */
export async function resolveAsset(dir: string, rel: string): Promise<AssetResolution> {
  let decoded: string;
  try {
    decoded = decodeURIComponent(rel);
  } catch {
    return { ok: false, status: 400 };
  }
  if (decoded.includes('\0')) return { ok: false, status: 400 };
  const target = resolve(dir, decoded);
  // Fast string containment: the resolved path must stay strictly inside `dir`.
  if (target !== dir && !target.startsWith(dir + sep)) return { ok: false, status: 403 };
  // Canonicalise and re-verify: a symlink inside `dir` must not resolve out of it.
  let real: string;
  try {
    real = await realpath(target);
  } catch {
    // ENOENT (and any other resolve failure) is a plain miss, not a leak.
    return { ok: false, status: 404 };
  }
  let realDir: string;
  try {
    realDir = await realpath(dir);
  } catch {
    return { ok: false, status: 404 };
  }
  if (real !== realDir && !real.startsWith(realDir + sep)) return { ok: false, status: 403 };
  return { ok: true, path: real };
}

/**
 * Serve a static noVNC asset, CONFINED to the package directory via
 * {@link resolveAsset}. Anything that resolves (or symlink-resolves) outside the
 * noVNC package dir is refused, so a client can never read a file outside the assets.
 */
async function serveAsset(rel: string, res: ServerResponse, head: boolean): Promise<void> {
  const resolution = await resolveAsset(novncDir(), rel);
  if (!resolution.ok) {
    if (resolution.status === 400) {
      res.writeHead(400).end();
    } else if (resolution.status === 403) {
      res.writeHead(403, { 'Content-Type': 'text/plain; charset=utf-8' }).end('Forbidden.\n');
    } else {
      res.writeHead(404, { 'Content-Type': 'text/plain; charset=utf-8' }).end('Not found.\n');
    }
    return;
  }
  let data: Buffer;
  try {
    data = await readFile(resolution.path);
  } catch {
    res.writeHead(404, { 'Content-Type': 'text/plain; charset=utf-8' }).end('Not found.\n');
    return;
  }
  res.writeHead(200, {
    'Content-Type': contentTypeFor(resolution.path),
    'Cache-Control': 'no-store',
  });
  res.end(head ? undefined : data);
}

/** Route an authenticated HTTP request: the page, a static asset, or 404. */
function handleRequest(req: IncomingMessage, res: ServerResponse, options: ViewerOptions): void {
  // Anti-clickjacking on EVERY response (F2), set before routing so it rides along
  // with each writeHead below (including the 401 and error responses).
  setAntiFramingHeaders(res);
  if (!isAuthenticated(req, options.password)) {
    sendUnauthorized(res);
    return;
  }
  const method = req.method ?? 'GET';
  if (method !== 'GET' && method !== 'HEAD') {
    res.writeHead(405, { Allow: 'GET, HEAD' }).end();
    return;
  }
  const head = method === 'HEAD';
  const url = new URL(req.url ?? '/', 'http://localhost');
  const pathname = url.pathname;
  if (pathname === '/' || pathname === '/index.html') {
    res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8', 'Cache-Control': 'no-store' });
    res.end(head ? undefined : renderPage(options.vncPassword));
    return;
  }
  if (pathname.startsWith(ASSET_PREFIX)) {
    void serveAsset(pathname.slice(ASSET_PREFIX.length), res, head);
    return;
  }
  res.writeHead(404, { 'Content-Type': 'text/plain; charset=utf-8' }).end('Not found.\n');
}

/**
 * Relay an authenticated websocket to the loopback VNC port. The target is taken
 * from {@link ViewerOptions} (server-controlled) — NEVER from the client's request —
 * and bytes are relayed verbatim in both directions.
 */
function proxyToVnc(ws: WebSocket, vncHost: string, vncPort: number): void {
  const tcp = connect(vncPort, vncHost);
  let closed = false;
  const closeBoth = (): void => {
    if (closed) return;
    closed = true;
    tcp.destroy();
    if (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING) ws.close();
  };

  // VNC -> browser. Honor ws backpressure: if the browser is slow and the ws send
  // buffer fills, stop reading from VNC until it drains, so the relay never buffers
  // an unbounded framebuffer backlog in memory.
  tcp.on('data', (chunk: Buffer) => {
    if (ws.readyState !== WebSocket.OPEN) return;
    ws.send(chunk, () => {
      // Once this frame is flushed and the buffer has drained, resume reading VNC.
      if (tcp.isPaused() && ws.bufferedAmount <= RELAY_HIGH_WATER_MARK) tcp.resume();
    });
    if (ws.bufferedAmount > RELAY_HIGH_WATER_MARK) tcp.pause();
  });
  // browser -> VNC. Binary frames arrive as a Buffer (or Buffer[] when fragmented).
  // Honor TCP backpressure: if the VNC socket's write buffer is full, pause the ws
  // (halting further message events) and resume it once the socket drains.
  ws.on('message', (data: RawData) => {
    const buf = Array.isArray(data)
      ? Buffer.concat(data)
      : Buffer.isBuffer(data)
        ? data
        : Buffer.from(data);
    if (!tcp.write(buf)) ws.pause();
  });
  tcp.on('drain', () => ws.resume());

  tcp.on('close', closeBoth);
  tcp.on('error', closeBoth);
  ws.on('close', closeBoth);
  ws.on('error', closeBoth);
}

/**
 * Start the Viewer's HTTP server and resolve once it is listening. Fail-closed:
 * with no password it refuses to serve. The websocket upgrade is gated by the same
 * password as the page, and every authenticated upgrade is proxied to the
 * server-controlled loopback VNC port.
 */
export function startViewer(options: ViewerOptions): Promise<Viewer> {
  if (options.password === '') {
    return Promise.reject(
      new Error(
        'The Viewer requires a password (QMP_MCP_VIEWER_PASSWORD) but none was configured; ' +
          'refusing to serve.',
      ),
    );
  }

  // Bound the size of any single inbound frame so a malicious client cannot pin
  // unbounded memory with one giant message (F5).
  const wss = new WebSocketServer({ noServer: true, maxPayload: MAX_WS_PAYLOAD_BYTES });
  wss.on('error', (err) => logger.warning(`Viewer websocket server error: ${err.message}`));

  const httpServer = createServer((req, res) => handleRequest(req, res, options));

  // Manual upgrade so the websocket is gated by the SAME password as the page: an
  // unauthenticated upgrade gets a 401 and is dropped before it ever proxies. After
  // auth, the upgrade must also be same-origin (F2), on the expected path (F6), and
  // within the concurrent-connection cap (F5) — else it is refused and the socket closed.
  httpServer.on('upgrade', (req: IncomingMessage, socket: Duplex, head: Buffer) => {
    if (!isAuthenticated(req, options.password)) {
      socket.write(
        `HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm="${REALM}"\r\n` +
          'Connection: close\r\n\r\n',
      );
      socket.destroy();
      return;
    }
    // Same-origin only for browser callers (F2): a cross-origin page must not be
    // able to drive an authenticated upgrade off a cached/leaked credential.
    if (!originAllowed(req)) {
      socket.write('HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n');
      socket.destroy();
      return;
    }
    // Only the path the served page connects on is a valid upgrade target (F6).
    const pathname = new URL(req.url ?? '/', 'http://localhost').pathname;
    if (pathname !== WEBSOCKET_PATH) {
      socket.write('HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n');
      socket.destroy();
      return;
    }
    // Cap concurrent authenticated relays (F5): refuse the N+1th with 503.
    if (wss.clients.size >= MAX_VIEWER_CONNECTIONS) {
      socket.write('HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\n');
      socket.destroy();
      return;
    }
    wss.handleUpgrade(req, socket, head, (ws) => {
      proxyToVnc(ws, options.vncHost, options.vncPort);
    });
  });

  return new Promise<Viewer>((resolvePromise, rejectPromise) => {
    const onError = (err: Error): void => rejectPromise(err);
    httpServer.once('error', onError);
    httpServer.listen(options.port, options.host, () => {
      httpServer.removeListener('error', onError);
      const address = httpServer.address();
      const boundPort =
        typeof address === 'object' && address !== null ? address.port : options.port;
      logger.info(
        `Viewer listening on http://${options.host}:${boundPort} ` +
          `(noVNC over the loopback VNC Display at ${options.vncHost}:${options.vncPort})`,
      );
      // Cleartext-exposure warning (F1): a non-loopback bind serves the HTTP Basic
      // password and the interactive VNC session in the clear. Do not refuse (the
      // container legitimately binds 0.0.0.0) — warn loudly so an operator fronts it.
      if (!isLoopbackHost(options.host)) {
        logger.warning(
          `Viewer is serving cleartext HTTP Basic + interactive VNC on ${options.host}:${boundPort}; ` +
            'put it behind a TLS-terminating reverse proxy on untrusted networks.',
        );
      }
      resolvePromise({
        host: options.host,
        port: boundPort,
        stop: () => stopServer(httpServer, wss),
      });
    });
  });
}

/** Drop all websocket clients and close the HTTP server. Resolves once fully closed. */
function stopServer(httpServer: HttpServer, wss: WebSocketServer): Promise<void> {
  return new Promise<void>((resolvePromise) => {
    for (const client of wss.clients) client.terminate();
    wss.close();
    httpServer.close(() => resolvePromise());
    // Force-close idle keep-alive connections so shutdown is prompt (Node >= 18.2).
    httpServer.closeAllConnections();
  });
}
