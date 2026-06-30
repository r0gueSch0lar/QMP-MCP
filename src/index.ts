#!/usr/bin/env node
import { readFileSync } from 'node:fs';
import { dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { MCPServer } from 'mcp-framework';
import { type Config, ConfigError, loadConfig } from './config.js';
import { orchestrator } from './instance/orchestrator.js';
import { logger, setLogLevel } from './logger.js';

/** Directory of the compiled entrypoint (i.e. `dist`). */
const here = dirname(fileURLToPath(import.meta.url));

/**
 * Read the package version from the package root, resolved relative to this
 * compiled file so it works no matter the current working directory (e.g. when
 * launched via `npx` from an unrelated directory).
 */
function readVersion(): string {
  try {
    const pkg = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8'));
    return typeof pkg.version === 'string' ? pkg.version : '0.0.0';
  } catch {
    return '0.0.0';
  }
}

async function main(): Promise<void> {
  let config: Config;
  try {
    config = loadConfig(process.env);
  } catch (err) {
    if (err instanceof ConfigError) {
      logger.error(err.message);
      // Set exit code and return rather than process.exit(): exiting on the same
      // tick can drop the buffered stderr write (the actionable message) when
      // launched over a pipe. Nothing else runs, so the process drains and exits 1.
      process.exitCode = 1;
      return;
    }
    throw err;
  }
  setLogLevel(config.logLevel);

  // This slice ships only the stdio transport. Refuse, rather than silently
  // running stdio, when another transport is requested — the HTTP transport and
  // its fail-closed auth arrive in a later slice.
  if (config.transport !== 'stdio') {
    logger.error(
      `QMP_MCP_TRANSPORT=${config.transport} is not available in this build yet. ` +
        'The HTTP transport ships in a later slice; set QMP_MCP_TRANSPORT=stdio for now.',
    );
    process.exitCode = 1;
    return;
  }

  const server = new MCPServer({
    name: 'qmp-mcp',
    version: readVersion(),
    // Pin discovery to the compiled directory so tools load from `dist/tools`
    // regardless of cwd (the default basePath is `cwd/dist`, wrong under npx).
    basePath: here,
    transport: { type: 'stdio' },
    logging: true,
  });

  // ADR-0004: the Instance's lifetime is the server's lifetime. On shutdown we
  // must tear down qemu, or it is orphaned (or lingers holding the QMP socket).
  // One-shot and idempotent: every trigger awaits the same teardown so the
  // process never exits while qemu is still being stopped.
  let shutdownPromise: Promise<void> | undefined;
  const shutdown = (reason: string): Promise<void> => {
    if (!shutdownPromise) {
      shutdownPromise = (async () => {
        if (orchestrator.getInstance().state === 'NONE') return;
        logger.info(`shutting down (${reason}): destroying the running Instance`);
        try {
          await orchestrator.destroyInstance();
        } catch (err) {
          logger.error(
            `failed to destroy the Instance during shutdown: ${
              err instanceof Error ? err.message : String(err)
            }`,
          );
        }
      })();
    }
    return shutdownPromise;
  };
  // SIGINT/SIGTERM stop the server (the framework also handles these to unwind
  // its transports); our handler guarantees qemu is destroyed first.
  process.once('SIGINT', () => void shutdown('SIGINT'));
  process.once('SIGTERM', () => void shutdown('SIGTERM'));

  logger.info('starting qmp-mcp (transport=stdio)');
  // start() resolves when the server stops (signal, or stdin/transport close),
  // so this also covers the stdin end hook. Tear down the Instance before exit.
  await server.start();
  await shutdown('server stopped');
}

main().catch((err: unknown) => {
  logger.error(`fatal: ${err instanceof Error ? (err.stack ?? err.message) : String(err)}`);
  process.exitCode = 1;
});
