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

export type TransportMode = 'stdio' | 'http' | 'both';
export type LogLevel = 'debug' | 'info' | 'warning' | 'error';
/** Which auth provider guards the HTTP transport. */
export type AuthMode = 'apikey' | 'jwt';

export const TRANSPORT_MODES: readonly TransportMode[] = ['stdio', 'http', 'both'];
export const LOG_LEVELS: readonly LogLevel[] = ['debug', 'info', 'warning', 'error'];
export const AUTH_MODES: readonly AuthMode[] = ['apikey', 'jwt'];

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
  };
}
