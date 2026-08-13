import { openSync, statSync, writeSync } from 'node:fs';
import { dirname } from 'node:path';
import type { LogLevel } from './config.js';

/**
 * The fixed vocabulary of logging subsystems, shared verbatim with the Rust variant (which maps
 * its module paths onto the same names): the keys `QMP_MCP_LOG_FILTER` may override per-subsystem.
 * Recording and the Serial Port bridge log under `orchestrator` — they are Instance-lifecycle
 * machinery on both sides (in Rust they live inside the orchestrator module).
 */
export const LOG_SUBSYSTEMS = [
  'server',
  'orchestrator',
  'qmp',
  'viewer',
  'download',
  'policy',
] as const;
export type LogSubsystem = (typeof LOG_SUBSYSTEMS)[number];

/**
 * Severity ordering for level filtering. Higher numbers are more severe.
 */
const SEVERITY: Record<LogLevel, number> = {
  debug: 0,
  info: 1,
  warning: 2,
  error: 3,
};

let threshold: LogLevel = 'info';
let filters: Partial<Record<LogSubsystem, LogLevel>> = {};
let logFileFd: number | undefined;

/**
 * Set the minimum level that will be emitted. Messages below it are dropped
 * unless a per-subsystem override from {@link setLogFilter} says otherwise.
 */
export function setLogLevel(level: LogLevel): void {
  threshold = level;
}

/**
 * Install the per-subsystem level overrides (`QMP_MCP_LOG_FILTER`, parsed and validated by the
 * config loader). A subsystem present here uses its own minimum level; the rest follow the
 * global threshold.
 */
export function setLogFilter(overrides: Partial<Record<LogSubsystem, LogLevel>>): void {
  filters = overrides;
}

/**
 * Open `QMP_MCP_LOG_FILE` for appending, fail-closed: the file's directory must already exist
 * (the server never creates log directories), and the path must not be a directory. On success
 * every emitted line is written to the file as well as stderr.
 */
export function openLogFile(path: string): void {
  const dir = dirname(path);
  let dirStat: ReturnType<typeof statSync>;
  try {
    dirStat = statSync(dir);
  } catch {
    throw new Error(
      `QMP_MCP_LOG_FILE: directory "${dir}" does not exist (create it first; the server does not create log directories).`,
    );
  }
  if (!dirStat.isDirectory()) {
    throw new Error(`QMP_MCP_LOG_FILE: "${dir}" is not a directory.`);
  }
  try {
    if (statSync(path).isDirectory()) {
      throw new Error(`QMP_MCP_LOG_FILE: "${path}" is a directory, not a file.`);
    }
  } catch (err) {
    if (err instanceof Error && err.message.startsWith('QMP_MCP_LOG_FILE')) throw err;
    // Path does not exist yet — fine, the open below creates it.
  }
  logFileFd = openSync(path, 'a');
}

/** Close the log file sink (used by tests; the OS closes it on process exit anyway). */
export function closeLogFile(): void {
  logFileFd = undefined;
}

function emit(subsystem: LogSubsystem, level: LogLevel, message: string): void {
  const min = filters[subsystem] ?? threshold;
  if (SEVERITY[level] < SEVERITY[min]) return;
  // IMPORTANT: always write to stderr. In stdio transport mode, stdout carries
  // the MCP JSON-RPC stream and must never be polluted by log output.
  const line = `[qmp-mcp] ${level} ${subsystem}: ${message}\n`;
  process.stderr.write(line);
  if (logFileFd !== undefined) {
    try {
      writeSync(logFileFd, line);
    } catch {
      // The sink went bad (deleted directory, full disk, …): disable it rather than
      // fail every subsequent log call, and say so once on stderr.
      logFileFd = undefined;
      process.stderr.write(
        '[qmp-mcp] warning server: log file write failed; disabling QMP_MCP_LOG_FILE sink.\n',
      );
    }
  }
}

/** A logger bound to one subsystem. */
export interface Logger {
  debug: (message: string) => void;
  info: (message: string) => void;
  warning: (message: string) => void;
  error: (message: string) => void;
}

const loggers = new Map<LogSubsystem, Logger>();

/**
 * Minimal stderr (plus optional file) logger, scoped to a subsystem so
 * `QMP_MCP_LOG_FILTER` can dial verbosity per area. Used for the server's own
 * lifecycle/diagnostic output; tool-level logging to the client goes through
 * mcp-framework's MCP logging. One memoized instance per subsystem, so a test
 * spying on `getLogger('viewer').warning` intercepts the viewer's own logger.
 */
export function getLogger(subsystem: LogSubsystem): Logger {
  let logger = loggers.get(subsystem);
  if (!logger) {
    logger = {
      debug: (message: string): void => emit(subsystem, 'debug', message),
      info: (message: string): void => emit(subsystem, 'info', message),
      warning: (message: string): void => emit(subsystem, 'warning', message),
      error: (message: string): void => emit(subsystem, 'error', message),
    };
    loggers.set(subsystem, logger);
  }
  return logger;
}
