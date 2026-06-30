import type { LogLevel } from './config.js';

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

/**
 * Set the minimum level that will be emitted. Messages below it are dropped.
 */
export function setLogLevel(level: LogLevel): void {
  threshold = level;
}

function emit(level: LogLevel, message: string): void {
  if (SEVERITY[level] < SEVERITY[threshold]) return;
  // IMPORTANT: always write to stderr. In stdio transport mode, stdout carries
  // the MCP JSON-RPC stream and must never be polluted by log output.
  process.stderr.write(`[qmp-mcp] ${level}: ${message}\n`);
}

/**
 * Minimal stderr logger. Used for the server's own lifecycle/diagnostic output;
 * tool-level logging to the client goes through mcp-framework's MCP logging.
 */
export const logger = {
  debug: (message: string): void => emit('debug', message),
  info: (message: string): void => emit('info', message),
  warning: (message: string): void => emit('warning', message),
  error: (message: string): void => emit('error', message),
};
