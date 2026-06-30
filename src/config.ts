/**
 * Configuration surface for the server, parsed from `QMP_MCP_*` environment
 * variables. This module is a pure function of its input env: it never reads
 * `process.env` directly, which keeps it trivially testable.
 *
 * Fail-closed: any value that is present but invalid throws a {@link ConfigError}
 * naming the offending variable and the allowed values, rather than silently
 * falling back to a default.
 */

export type TransportMode = 'stdio' | 'http' | 'both';
export type LogLevel = 'debug' | 'info' | 'warning' | 'error';

export const TRANSPORT_MODES: readonly TransportMode[] = ['stdio', 'http', 'both'];
export const LOG_LEVELS: readonly LogLevel[] = ['debug', 'info', 'warning', 'error'];

export interface Config {
  /** Which transport(s) the server should expose. */
  transport: TransportMode;
  /** Minimum severity emitted by the server's own logger. */
  logLevel: LogLevel;
}

/**
 * Raised when an environment variable is present but holds an invalid value.
 * The message always names the variable and the remediation.
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
 * Build a validated {@link Config} from an environment map.
 */
export function loadConfig(env: NodeJS.ProcessEnv): Config {
  return {
    transport: parseEnum('QMP_MCP_TRANSPORT', env.QMP_MCP_TRANSPORT, TRANSPORT_MODES, 'stdio'),
    logLevel: parseEnum('QMP_MCP_LOG_LEVEL', env.QMP_MCP_LOG_LEVEL, LOG_LEVELS, 'info'),
  };
}
