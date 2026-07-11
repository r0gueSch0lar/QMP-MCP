/**
 * Configuration surface for the server, parsed from `QMP_MCP_*` environment
 * variables. This module is a pure function of its input env: it never reads
 * `process.env` directly, which keeps it trivially testable.
 *
 * Fail-closed: any value that is present but invalid throws a {@link ConfigError}
 * naming the offending variable and the allowed values, rather than silently
 * falling back to a default. The HTTP transport additionally refuses to start
 * without auth (ADR-0005): if it is selected with no credentials configured and
 * no explicit insecure override, {@link loadConfig} throws here, before any
 * server is booted.
 */

import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { DEFAULT_EVENT_BUFFER_SIZE } from './instance/event-buffer.js';

export type TransportMode = 'stdio' | 'http' | 'both';
export type LogLevel = 'debug' | 'info' | 'warning' | 'error';
/** Which auth provider guards the HTTP transport. */
export type AuthMode = 'apikey' | 'jwt';

export const TRANSPORT_MODES: readonly TransportMode[] = ['stdio', 'http', 'both'];
export const LOG_LEVELS: readonly LogLevel[] = ['debug', 'info', 'warning', 'error'];
export const AUTH_MODES: readonly AuthMode[] = ['apikey', 'jwt'];

/**
 * An inclusive `[low, high]` host-port range. A user-mode port-forward's
 * `hostPort` must fall inside it (ADR-0009), so the agent can never bind an
 * arbitrary or privileged host port.
 */
export interface PortRange {
  /** Lowest host port a forward may bind (inclusive). */
  low: number;
  /** Highest host port a forward may bind (inclusive). */
  high: number;
}

/**
 * Default host-port range for user-mode port-forwards: the IANA non-privileged
 * range 1024-65535. Privileged ports (<1024) are excluded so a forward never
 * needs `CAP_NET_BIND_SERVICE`/root (ADR-0008). Shared by {@link loadConfig} and
 * the argv builder so the default lives in exactly one place.
 */
export const DEFAULT_HOSTFWD_PORT_RANGE: PortRange = { low: 1024, high: 65535 };

/**
 * Fallback `qemu-system-*` binary: the arch that `machineArch` degrades an unknown (or
 * x86) `machine` to, i.e. what `qemuBinaryForMachine` returns for anything not mapped to
 * ARM. The Orchestrator normally DERIVES the binary from the Instance's `machine`
 * (ADR-0013) and `QMP_MCP_QEMU_BINARY` overrides it; this constant is just the x86_64
 * historical default the map falls back to.
 */
export const DEFAULT_QEMU_BINARY = 'qemu-system-x86_64';

export interface Config {
  /** Which transport(s) the server should expose. */
  transport: TransportMode;
  /** Minimum severity emitted by the server's own logger. */
  logLevel: LogLevel;
  /** Address the HTTP transport binds to. */
  httpHost: string;
  /** TCP port the HTTP transport listens on. */
  httpPort: number;
  /** Path the MCP endpoint is served from. */
  httpEndpoint: string;
  /**
   * Browser origins permitted by the framework's DNS-rebinding/CORS guard.
   * Requests with no `Origin` header (curl, MCP SDK clients) are always allowed;
   * a browser request whose `Origin` is not in this list is rejected with 403.
   */
  allowedOrigins: string[];
  /** Which provider guards the HTTP transport when auth is enabled. */
  authMode: AuthMode;
  /** Valid API keys for {@link AuthMode} `apikey`, trimmed with empties dropped. */
  apiKeys: string[];
  /** Signing secret for {@link AuthMode} `jwt`, or undefined when unset. */
  jwtSecret: string | undefined;
  /** When true, the HTTP transport runs unauthenticated (local dev only). */
  allowInsecure: boolean;
  /**
   * Absolute path of the read-write Image Store directory (ADR-0006): the single
   * allowlisted directory guest disk images live in and are created into. Disks
   * are referenced by name within it, never by host path.
   */
  imageDir: string;
  /**
   * Absolute path of the read-only ISO Store directory (ADR-0006): the separate
   * allowlisted directory installation/boot ISO media live in. ISOs are
   * referenced by name within it, never by host path, and it is never written to.
   */
  isoDir: string;
  /**
   * Explicit `qemu-system-*` binary override (`QMP_MCP_QEMU_BINARY`), or `undefined`
   * when unset — in which case the Orchestrator DERIVES the binary from each Instance's
   * `machine` (q35/pc → x86_64, virt/raspi* → aarch64; ADR-0013). Set it only to force a
   * specific emulator for every Instance.
   */
  qemuBinaryOverride: string | undefined;
  /**
   * Hard cap, in GiB, on the virtual size of a disk image {@link createImage}
   * may allocate. A larger request is rejected naming this cap.
   */
  maxDiskGb: number;
  /**
   * Hard cap, in MiB, on a Hardware Spec's `memoryMb` (`QMP_MCP_MAX_MEMORY_MB`,
   * default 4096). A spec requesting more guest RAM is rejected before qemu is
   * spawned, naming this cap (issue #9).
   */
  maxMemoryMb: number;
  /**
   * Hard cap on a Hardware Spec's `vcpus` (`QMP_MCP_MAX_VCPUS`, default 2). A
   * spec requesting more virtual CPUs is rejected before qemu is spawned, naming
   * this cap (issue #9).
   */
  maxVcpus: number;
  /**
   * Inclusive host-port range a user-mode port-forward's `hostPort` must fall
   * within (`QMP_MCP_HOSTFWD_PORT_RANGE`, default 1024-65535). A forward to a
   * host port outside it is rejected naming the range (ADR-0009).
   */
  hostfwdPortRange: PortRange;
  /**
   * When true, host-level guest networking (`tap`/`bridge`) is permitted
   * (`QMP_MCP_ALLOW_HOST_NET`). Default false: only user-mode (SLiRP) networking
   * is allowed and a `tap`/`bridge` spec is refused, since host networking needs
   * host privileges incompatible with the non-root posture (ADR-0008/0009).
   */
  allowHostNet: boolean;
  /**
   * Host directory shared into the guest via virtio-9p when a spec sets `share: true`
   * (`QMP_MCP_HOST_SHARE_DIR`, ADR-0014), or undefined when sharing is disabled. An
   * operator path, never agent-supplied.
   */
  hostShareDir: string | undefined;
  /**
   * The INTENDED guest mountpoint for the shared folder (`QMP_MCP_GUEST_SHARE_DIR`),
   * or undefined. Advisory only (QEMU can't mount inside the guest) — reported by
   * `get_share` and used to build the guest's `mount -t 9p` command.
   */
  guestShareDir: string | undefined;
  /**
   * Whether the shared folder is read-WRITE (`QMP_MCP_ALLOW_SHARE_WRITE`, default
   * false ⇒ read-only). The agent can never escalate to writable.
   */
  allowShareWrite: boolean;
  /**
   * When true, `create_instance` auto-starts the Guest: it issues QMP `cont`
   * immediately after launch (`QMP_MCP_AUTO_START`), so the Instance begins
   * executing rather than staying frozen at the `-S` startup pause. Default TRUE
   * (ADR-0016) — set false to load the Guest paused for deterministic inspection,
   * running it only on an explicit `resume_instance` (issue #8/#10).
   */
  autoStart: boolean;
  /**
   * Capacity of the Event Buffer — the bounded ring of recent QMP async events
   * the agent reads via `get_events`/`wait_for_event` (`QMP_MCP_EVENT_BUFFER_SIZE`,
   * default {@link DEFAULT_EVENT_BUFFER_SIZE}). Once full, the oldest event is
   * evicted, so the buffer never grows without bound (issue #12).
   */
  eventBufferSize: number;
  /**
   * Serial Port ring-buffer size in bytes (`QMP_MCP_SERIAL_BUFFER_BYTES`, ADR-0015).
   * Power-of-two; the size QEMU's `ringbuf` chardev is created with when `serial: true`.
   */
  serialBufferBytes: number;
  /**
   * When true, a Hardware Spec's `extraArgs` (raw QEMU arguments) are appended to
   * the generated argv (`QMP_MCP_ALLOW_RAW_ARGS`). Default false: raw args are
   * host-compromise-equivalent, so a spec carrying `extraArgs` is REFUSED unless
   * this is explicitly enabled — the gated escape hatch for trusted single-tenant
   * hosts (ADR-0002).
   */
  allowRawArgs: boolean;
  /**
   * The human-facing password gating the noVNC Viewer (`QMP_MCP_VIEWER_PASSWORD`,
   * ADR-0010), or undefined when unset. The Viewer is FAIL-CLOSED behind it: the
   * page and the websocket are refused unless the request authenticates, and
   * requesting a `display: vnc` Instance while this is unset is rejected. Distinct
   * from the MCP `apiKeys` because its audience is a human in a browser.
   */
  viewerPassword: string | undefined;
  /**
   * Optional username enforced on the noVNC Viewer's HTTP Basic auth
   * (`QMP_MCP_VIEWER_USER`, ADR-0010), or undefined when unset. When set, the browser
   * must supply this username alongside {@link Config.viewerPassword}; when unset the
   * username is ignored (password-only), the historical behavior.
   */
  viewerUser: string | undefined;
  /**
   * Address the noVNC Viewer's HTTP server binds to (`QMP_MCP_VIEWER_HOST`, default
   * `127.0.0.1`; the container image overrides it to `0.0.0.0`). Independent of the
   * MCP transport, so the Viewer works even with `QMP_MCP_TRANSPORT=stdio` (ADR-0010).
   */
  viewerHost: string;
  /** TCP port the noVNC Viewer listens on (`QMP_MCP_VIEWER_PORT`, default 6080). */
  viewerPort: number;
}

/**
 * Raised when an environment variable is present but holds an invalid value,
 * or when the HTTP transport is selected without the auth it requires. The
 * message always names the variable(s) and the remediation.
 */
export class ConfigError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'ConfigError';
  }
}

/**
 * Parse an enum-valued env var. Treats undefined or empty string as unset and
 * returns the fallback; otherwise validates (case-insensitively) against the
 * allowed set and throws an actionable {@link ConfigError} on mismatch.
 */
function parseEnum<T extends string>(
  varName: string,
  raw: string | undefined,
  allowed: readonly T[],
  fallback: T,
): T {
  if (raw === undefined || raw === '') return fallback;
  const value = raw.toLowerCase();
  if ((allowed as readonly string[]).includes(value)) return value as T;
  throw new ConfigError(`${varName} must be one of: ${allowed.join(', ')} (got "${raw}").`);
}

/**
 * Parse a required-non-empty string env var, trimming surrounding whitespace.
 * Treats undefined or blank as unset and returns the fallback.
 */
function parseString(raw: string | undefined, fallback: string): string {
  if (raw === undefined) return fallback;
  const value = raw.trim();
  return value === '' ? fallback : value;
}

/**
 * Parse a TCP port. Treats undefined or empty as unset and returns the fallback;
 * otherwise requires a base-10 integer in 1..65535 and fails closed on anything
 * else (e.g. "abc", "8080x", "0", "70000") rather than silently coercing.
 */
function parsePort(varName: string, raw: string | undefined, fallback: number): number {
  if (raw === undefined || raw.trim() === '') return fallback;
  const value = raw.trim();
  if (!/^\d+$/.test(value)) {
    throw new ConfigError(`${varName} must be an integer port in 1..65535 (got "${raw}").`);
  }
  const port = Number(value);
  if (port < 1 || port > 65535) {
    throw new ConfigError(`${varName} must be an integer port in 1..65535 (got "${raw}").`);
  }
  return port;
}

/**
 * Parse a boolean flag. Accepts `true`/`false` case-insensitively; undefined or
 * empty is the fallback. Fails closed on any other value so a typo never reads
 * as a silent "false".
 */
function parseBoolean(varName: string, raw: string | undefined, fallback: boolean): boolean {
  if (raw === undefined || raw === '') return fallback;
  const value = raw.trim().toLowerCase();
  if (value === 'true') return true;
  if (value === 'false') return false;
  throw new ConfigError(`${varName} must be "true" or "false" (got "${raw}").`);
}

/**
 * Parse a positive-integer env var (e.g. a size cap). Treats undefined or empty
 * as unset and returns the fallback; otherwise requires a base-10 integer >= 1
 * and fails closed on anything else rather than silently coercing.
 */
function parsePositiveInt(varName: string, raw: string | undefined, fallback: number): number {
  if (raw === undefined || raw.trim() === '') return fallback;
  const value = raw.trim();
  if (!/^\d+$/.test(value) || Number(value) < 1) {
    throw new ConfigError(`${varName} must be a positive integer >= 1 (got "${raw}").`);
  }
  return Number(value);
}

/**
 * Parse a byte size that must be a power of two (the QEMU `ringbuf` chardev requirement,
 * ADR-0015). Undefined or empty reads as the fallback; otherwise requires a positive integer
 * that is an exact power of two, failing closed so the server never hands QEMU a size it
 * rejects at launch.
 */
function parsePowerOfTwo(varName: string, raw: string | undefined, fallback: number): number {
  if (raw === undefined || raw.trim() === '') return fallback;
  const value = raw.trim();
  const n = Number(value);
  if (!/^\d+$/.test(value) || n < 1 || (n & (n - 1)) !== 0) {
    throw new ConfigError(
      `${varName} must be a power-of-two number of bytes (e.g. 65536, 1048576) — got "${raw}".`,
    );
  }
  return n;
}

/**
 * Parse a `LOW-HIGH` host-port range. Treats undefined or empty as unset and
 * returns the fallback; otherwise requires two base-10 integers with
 * `1 <= LOW <= HIGH <= 65535` and fails closed on anything else (garbage,
 * reversed bounds, out-of-range, missing dash) rather than silently coercing.
 */
function parsePortRange(varName: string, raw: string | undefined, fallback: PortRange): PortRange {
  if (raw === undefined || raw.trim() === '') return fallback;
  const value = raw.trim();
  const match = /^(\d+)-(\d+)$/.exec(value);
  const message =
    `${varName} must be a host-port range "LOW-HIGH" with ` +
    `1 <= LOW <= HIGH <= 65535 (got "${raw}").`;
  if (!match) throw new ConfigError(message);
  const low = Number(match[1]);
  const high = Number(match[2]);
  if (low < 1 || high > 65535 || low > high) throw new ConfigError(message);
  return { low, high };
}

/**
 * Resolve the Image Store directory (ADR-0006/0007). An explicit
 * `QMP_MCP_IMAGE_DIR` wins; otherwise a host-agnostic default is derived from the
 * XDG/HOME data dirs (and finally the OS temp dir), so the bare-metal default
 * never assumes the Docker filesystem layout — the container image overrides it
 * via env. Exported so the Image Store singleton and {@link loadConfig} share one
 * source of truth.
 */
export function resolveImageDir(env: NodeJS.ProcessEnv): string {
  const explicit = env.QMP_MCP_IMAGE_DIR?.trim();
  if (explicit) return explicit;
  const xdg = env.XDG_DATA_HOME?.trim();
  if (xdg) return join(xdg, 'qmp-mcp', 'images');
  const home = env.HOME?.trim();
  if (home) return join(home, '.local', 'share', 'qmp-mcp', 'images');
  return join(tmpdir(), 'qmp-mcp', 'images');
}

/**
 * Resolve the read-only ISO Store directory (ADR-0006/0007). Mirrors
 * {@link resolveImageDir} but is a SEPARATE directory (`isos`, not `images`) so
 * install media and writable disks have different permissions: an explicit
 * `QMP_MCP_ISO_DIR` wins; otherwise a host-agnostic default is derived from the
 * XDG/HOME data dirs (and finally the OS temp dir), so the bare-metal default
 * never assumes the Docker filesystem layout — the container image overrides it
 * via env. Exported so the ISO Store and {@link loadConfig} share one source of
 * truth.
 */
export function resolveIsoDir(env: NodeJS.ProcessEnv): string {
  const explicit = env.QMP_MCP_ISO_DIR?.trim();
  if (explicit) return explicit;
  const xdg = env.XDG_DATA_HOME?.trim();
  if (xdg) return join(xdg, 'qmp-mcp', 'isos');
  const home = env.HOME?.trim();
  if (home) return join(home, '.local', 'share', 'qmp-mcp', 'isos');
  return join(tmpdir(), 'qmp-mcp', 'isos');
}

/**
 * Resolve and validate an EXPLICIT `qemu-system-*` binary override
 * (`QMP_MCP_QEMU_BINARY`), or `undefined` when unset or blank/whitespace-only. An
 * explicit value is trimmed and must be a **bare command name** (resolved via `PATH`)
 * or an **absolute path**, over the safe charset `[A-Za-z0-9._/+-]`.
 *
 * `undefined` means "no override" — the Orchestrator then DERIVES the binary from the
 * per-instance `machine` (`qemu-system-x86_64` for q35/pc, `qemu-system-aarch64` for
 * virt/raspi*; see `qemuBinaryForMachine`, issue #18), so switching guest
 * architectures no longer needs an env flip + restart. An explicit value overrides
 * that derivation for every Instance (e.g. a custom-built emulator).
 *
 * The value is spawned as argv[0] with NO shell (execFile), but we still fail closed
 * on whitespace, shell metacharacters, and control characters — and on a relative path
 * (`./qemu`, `build/qemu`, `../bin/qemu`) — so a foot-gun never reaches the spawn.
 */
export function resolveQemuBinaryOverride(env: NodeJS.ProcessEnv): string | undefined {
  const raw = env.QMP_MCP_QEMU_BINARY;
  if (raw === undefined) return undefined;
  const value = raw.trim();
  if (value === '') return undefined;
  // Safe charset: ASCII letters/digits plus the path and version punctuation that
  // legitimate binary names and absolute paths use (`^[A-Za-z0-9._/+-]+$`). A '/'
  // is only allowed when the value is an absolute path; a bare command name carries
  // no slash, so this also rejects relative paths, which resolve against an ambient
  // CWD rather than a stable location.
  const charsetOk = /^[A-Za-z0-9._/+-]+$/.test(value);
  const shapeOk = !value.includes('/') || value.startsWith('/');
  if (!charsetOk || !shapeOk) {
    throw new ConfigError(
      `QMP_MCP_QEMU_BINARY must be a bare command name or an absolute path over ` +
        `[A-Za-z0-9._/+-] (no whitespace, shell metacharacters, or control ` +
        `characters) (got "${value}").`,
    );
  }
  return value;
}

/** Resolve the maximum disk size cap in GiB (`QMP_MCP_MAX_DISK_GB`, default 64). */
export function resolveMaxDiskGb(env: NodeJS.ProcessEnv): number {
  return parsePositiveInt('QMP_MCP_MAX_DISK_GB', env.QMP_MCP_MAX_DISK_GB, 64);
}

/**
 * Resolve the maximum guest-memory cap in MiB (`QMP_MCP_MAX_MEMORY_MB`, default
 * 4096). Positive-integer, fail-closed on garbage. Exported so the Orchestrator
 * singleton and {@link loadConfig} share one source of truth (issue #9).
 */
export function resolveMaxMemoryMb(env: NodeJS.ProcessEnv): number {
  return parsePositiveInt('QMP_MCP_MAX_MEMORY_MB', env.QMP_MCP_MAX_MEMORY_MB, 4096);
}

/**
 * Resolve the maximum vCPU cap (`QMP_MCP_MAX_VCPUS`, default 2). Positive-integer,
 * fail-closed on garbage. Exported so the Orchestrator singleton and
 * {@link loadConfig} share one source of truth (issue #9).
 */
export function resolveMaxVcpus(env: NodeJS.ProcessEnv): number {
  return parsePositiveInt('QMP_MCP_MAX_VCPUS', env.QMP_MCP_MAX_VCPUS, 2);
}

/**
 * Resolve the user-mode port-forward host-port range (`QMP_MCP_HOSTFWD_PORT_RANGE`,
 * default {@link DEFAULT_HOSTFWD_PORT_RANGE}). Exported so the Orchestrator
 * singleton and {@link loadConfig} share one source of truth (ADR-0009).
 */
export function resolveHostfwdPortRange(env: NodeJS.ProcessEnv): PortRange {
  return parsePortRange(
    'QMP_MCP_HOSTFWD_PORT_RANGE',
    env.QMP_MCP_HOSTFWD_PORT_RANGE,
    DEFAULT_HOSTFWD_PORT_RANGE,
  );
}

/**
 * Resolve whether host-level (`tap`/`bridge`) networking is permitted
 * (`QMP_MCP_ALLOW_HOST_NET`, default false). Exported so the Orchestrator
 * singleton and {@link loadConfig} share one source of truth (ADR-0009).
 */
export function resolveAllowHostNet(env: NodeJS.ProcessEnv): boolean {
  return parseBoolean('QMP_MCP_ALLOW_HOST_NET', env.QMP_MCP_ALLOW_HOST_NET, false);
}

/**
 * Resolve whether `create_instance` auto-starts the Guest (`QMP_MCP_AUTO_START`,
 * default true — ADR-0016). Exported so the Orchestrator singleton and
 * {@link loadConfig} share one source of truth (issue #8/#10).
 */
export function resolveAutoStart(env: NodeJS.ProcessEnv): boolean {
  return parseBoolean('QMP_MCP_AUTO_START', env.QMP_MCP_AUTO_START, true);
}

/**
 * Default Serial Port ring-buffer size in bytes (`QMP_MCP_SERIAL_BUFFER_BYTES`, ADR-0015):
 * 1 MiB — a power of two large enough for a verbose boot log. Mirrors the Rust constant.
 */
export const DEFAULT_SERIAL_BUFFER_BYTES = 1 << 20;

/**
 * Resolve the Serial Port ring-buffer size (`QMP_MCP_SERIAL_BUFFER_BYTES`, default
 * {@link DEFAULT_SERIAL_BUFFER_BYTES}). Power-of-two, fail-closed. Exported so the Orchestrator
 * singleton and {@link loadConfig} share one source of truth (ADR-0015).
 */
export function resolveSerialBufferBytes(env: NodeJS.ProcessEnv): number {
  return parsePowerOfTwo(
    'QMP_MCP_SERIAL_BUFFER_BYTES',
    env.QMP_MCP_SERIAL_BUFFER_BYTES,
    DEFAULT_SERIAL_BUFFER_BYTES,
  );
}

/**
 * Resolve the Event Buffer capacity (`QMP_MCP_EVENT_BUFFER_SIZE`, default
 * {@link DEFAULT_EVENT_BUFFER_SIZE}). Positive-integer, fail-closed on garbage.
 * Exported so the Orchestrator singleton and {@link loadConfig} share one source
 * of truth (issue #12).
 */
export function resolveEventBufferSize(env: NodeJS.ProcessEnv): number {
  return parsePositiveInt(
    'QMP_MCP_EVENT_BUFFER_SIZE',
    env.QMP_MCP_EVENT_BUFFER_SIZE,
    DEFAULT_EVENT_BUFFER_SIZE,
  );
}

/**
 * Resolve whether the raw-args escape hatch is enabled (`QMP_MCP_ALLOW_RAW_ARGS`,
 * default false). When false a Hardware Spec carrying `extraArgs` is refused;
 * when true those raw QEMU arguments are appended to the generated argv (ADR-0002).
 * Exported so the Orchestrator singleton and {@link loadConfig} share one source
 * of truth.
 */
export function resolveAllowRawArgs(env: NodeJS.ProcessEnv): boolean {
  return parseBoolean('QMP_MCP_ALLOW_RAW_ARGS', env.QMP_MCP_ALLOW_RAW_ARGS, false);
}

/**
 * Resolve the host directory shared into the guest via virtio-9p when a spec opts in
 * with `share: true` (ADR-0014 folder sharing), or undefined when unset (sharing
 * disabled). This is an OPERATOR path, never agent-supplied — the agent only toggles
 * `share`, mirroring how disks/ISOs live in operator-configured Stores. It must be an
 * ABSOLUTE path (a relative one resolves against the process CWD); a blank value reads
 * as unset. Exported so the Orchestrator singleton and {@link loadConfig} share one
 * source of truth.
 */
export function resolveHostShareDir(env: NodeJS.ProcessEnv): string | undefined {
  const raw = env.QMP_MCP_HOST_SHARE_DIR;
  if (raw === undefined) return undefined;
  const value = raw.trim();
  if (value === '') return undefined;
  if (!value.startsWith('/')) {
    throw new ConfigError(
      `QMP_MCP_HOST_SHARE_DIR must be an absolute path (got "${value}"). It is the host directory ` +
        'shared into the guest; a relative path resolves against an ambient CWD.',
    );
  }
  return value;
}

/**
 * Resolve the INTENDED guest mountpoint for the shared folder (`QMP_MCP_GUEST_SHARE_DIR`),
 * or undefined when unset. QEMU cannot mount inside the guest — this is advisory: it is
 * reported by the `get_share` tool and used to build the exact `mount -t 9p` command the
 * guest runs. The 9p mount tag is always the fixed constant `share`.
 */
export function resolveGuestShareDir(env: NodeJS.ProcessEnv): string | undefined {
  const raw = env.QMP_MCP_GUEST_SHARE_DIR;
  return raw !== undefined && raw.trim() !== '' ? raw.trim() : undefined;
}

/**
 * Whether the shared folder is mounted read-WRITE (`QMP_MCP_ALLOW_SHARE_WRITE`, default
 * false). Fail-closed: the share is read-only unless the operator explicitly enables
 * writes — the agent can never escalate to writable (mirrors `QMP_MCP_ALLOW_HOST_NET`).
 */
export function resolveAllowShareWrite(env: NodeJS.ProcessEnv): boolean {
  return parseBoolean('QMP_MCP_ALLOW_SHARE_WRITE', env.QMP_MCP_ALLOW_SHARE_WRITE, false);
}

/**
 * Resolve the noVNC Viewer password (`QMP_MCP_VIEWER_PASSWORD`, ADR-0010), or
 * undefined when unset. A whitespace-only value is treated as unset (mirroring
 * `QMP_MCP_JWT_SECRET`) so the Viewer stays fail-closed rather than serving behind
 * a blank gate. Exported so the Orchestrator singleton and {@link loadConfig} share
 * one source of truth.
 */
export function resolveViewerPassword(env: NodeJS.ProcessEnv): string | undefined {
  const raw = env.QMP_MCP_VIEWER_PASSWORD;
  return raw !== undefined && raw.trim() !== '' ? raw : undefined;
}

/**
 * Resolve the OPTIONAL noVNC Viewer username (`QMP_MCP_VIEWER_USER`, ADR-0010), or
 * undefined when unset. When set, the Viewer's HTTP Basic auth enforces this username
 * alongside the password; when unset (or whitespace-only, treated as unset like the
 * password) the username half is ignored, preserving the password-only default.
 * Exported so the Orchestrator singleton and {@link loadConfig} share one source of
 * truth.
 */
export function resolveViewerUser(env: NodeJS.ProcessEnv): string | undefined {
  const raw = env.QMP_MCP_VIEWER_USER;
  return raw !== undefined && raw.trim() !== '' ? raw : undefined;
}

/**
 * Resolve the noVNC Viewer bind address (`QMP_MCP_VIEWER_HOST`, default
 * `127.0.0.1`; the container image overrides it to `0.0.0.0`). Exported so the
 * Orchestrator singleton and {@link loadConfig} share one source of truth (ADR-0010).
 */
export function resolveViewerHost(env: NodeJS.ProcessEnv): string {
  return parseString(env.QMP_MCP_VIEWER_HOST, '127.0.0.1');
}

/**
 * Resolve the noVNC Viewer TCP port (`QMP_MCP_VIEWER_PORT`, default 6080).
 * Fail-closed on garbage. Exported so the Orchestrator singleton and
 * {@link loadConfig} share one source of truth (ADR-0010).
 */
export function resolveViewerPort(env: NodeJS.ProcessEnv): number {
  return parsePort('QMP_MCP_VIEWER_PORT', env.QMP_MCP_VIEWER_PORT, 6080);
}

/**
 * Split a comma-separated list env var into trimmed, non-empty entries.
 * Undefined yields an empty list.
 */
function parseList(raw: string | undefined): string[] {
  if (raw === undefined) return [];
  return raw
    .split(',')
    .map((entry) => entry.trim())
    .filter((entry) => entry !== '');
}

/**
 * Build a validated {@link Config} from an environment map. Throws a
 * {@link ConfigError} on any invalid value, and — per ADR-0005 — when the HTTP
 * transport is selected without configured auth and without an explicit insecure
 * override.
 */
export function loadConfig(env: NodeJS.ProcessEnv): Config {
  const transport = parseEnum('QMP_MCP_TRANSPORT', env.QMP_MCP_TRANSPORT, TRANSPORT_MODES, 'stdio');
  const logLevel = parseEnum('QMP_MCP_LOG_LEVEL', env.QMP_MCP_LOG_LEVEL, LOG_LEVELS, 'info');
  const httpHost = parseString(env.QMP_MCP_HTTP_HOST, '127.0.0.1');
  const httpPort = parsePort('QMP_MCP_HTTP_PORT', env.QMP_MCP_HTTP_PORT, 8080);
  const httpEndpoint = parseString(env.QMP_MCP_HTTP_ENDPOINT, '/mcp');
  const authMode = parseEnum('QMP_MCP_AUTH', env.QMP_MCP_AUTH, AUTH_MODES, 'apikey');
  const apiKeys = parseList(env.QMP_MCP_API_KEYS);
  const rawJwtSecret = env.QMP_MCP_JWT_SECRET;
  const jwtSecret =
    rawJwtSecret !== undefined && rawJwtSecret.trim() !== '' ? rawJwtSecret : undefined;
  const allowInsecure = parseBoolean('QMP_MCP_ALLOW_INSECURE', env.QMP_MCP_ALLOW_INSECURE, false);

  // Allowed browser origins for the DNS-rebinding/CORS guard. Default to the
  // loopback origins for the configured port; an explicit list (e.g. behind a
  // reverse proxy) overrides it.
  const originOverride = parseList(env.QMP_MCP_HTTP_ALLOWED_ORIGINS);
  const allowedOrigins =
    originOverride.length > 0
      ? originOverride
      : [`http://localhost:${httpPort}`, `http://127.0.0.1:${httpPort}`];

  // ADR-0005 fail-closed: the HTTP transport can build and control VMs, so it
  // refuses to start unauthenticated unless the operator opts in explicitly.
  const httpSelected = transport === 'http' || transport === 'both';
  if (httpSelected && !allowInsecure) {
    if (authMode === 'apikey' && apiKeys.length === 0) {
      throw new ConfigError(
        `HTTP transport requires authentication but none is configured ` +
          `(QMP_MCP_AUTH=apikey, QMP_MCP_API_KEYS is empty). ` +
          `Set QMP_MCP_API_KEYS to a comma-separated list of keys, ` +
          `or switch to JWT with QMP_MCP_AUTH=jwt and QMP_MCP_JWT_SECRET, ` +
          `or set QMP_MCP_ALLOW_INSECURE=true to run unauthenticated (local dev only).`,
      );
    }
    if (authMode === 'jwt' && jwtSecret === undefined) {
      throw new ConfigError(
        `HTTP transport with QMP_MCP_AUTH=jwt requires a signing secret but ` +
          `QMP_MCP_JWT_SECRET is not set. ` +
          `Set QMP_MCP_JWT_SECRET, ` +
          `or set QMP_MCP_ALLOW_INSECURE=true to run unauthenticated (local dev only).`,
      );
    }
  }

  return {
    transport,
    logLevel,
    httpHost,
    httpPort,
    httpEndpoint,
    allowedOrigins,
    authMode,
    apiKeys,
    jwtSecret,
    allowInsecure,
    imageDir: resolveImageDir(env),
    isoDir: resolveIsoDir(env),
    qemuBinaryOverride: resolveQemuBinaryOverride(env),
    maxDiskGb: resolveMaxDiskGb(env),
    maxMemoryMb: resolveMaxMemoryMb(env),
    maxVcpus: resolveMaxVcpus(env),
    hostfwdPortRange: resolveHostfwdPortRange(env),
    allowHostNet: resolveAllowHostNet(env),
    hostShareDir: resolveHostShareDir(env),
    guestShareDir: resolveGuestShareDir(env),
    allowShareWrite: resolveAllowShareWrite(env),
    autoStart: resolveAutoStart(env),
    eventBufferSize: resolveEventBufferSize(env),
    serialBufferBytes: resolveSerialBufferBytes(env),
    allowRawArgs: resolveAllowRawArgs(env),
    viewerPassword: resolveViewerPassword(env),
    viewerUser: resolveViewerUser(env),
    viewerHost: resolveViewerHost(env),
    viewerPort: resolveViewerPort(env),
  };
}
