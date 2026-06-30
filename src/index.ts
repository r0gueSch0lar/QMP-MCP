#!/usr/bin/env node
import { readFileSync } from 'node:fs';
import { dirname } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import {
  APIKeyAuthProvider,
  type AuthProvider,
  type HttpStreamTransportConfig,
  JWTAuthProvider,
  MCPServer,
  type MCPServerConfig,
  type TransportConfig,
} from 'mcp-framework';
import { type Config, ConfigError, loadConfig } from './config.js';
import { orchestrator } from './instance/orchestrator.js';
import { logger, setLogLevel } from './logger.js';

/** Directory of the compiled entrypoint (i.e. `dist`). */
const here = dirname(fileURLToPath(import.meta.url));

/**
 * Build the auth provider that guards the HTTP transport, or `undefined` when
 * insecure mode is enabled. By the time this runs {@link loadConfig} has already
 * failed closed if a required credential was missing, so the selected mode's
 * secret is guaranteed present here.
 */
export function buildAuthProvider(config: Config): AuthProvider | undefined {
  if (config.allowInsecure) return undefined;
  if (config.authMode === 'jwt') {
    // loadConfig guarantees jwtSecret is set for http+jwt without insecure.
    if (config.jwtSecret === undefined) {
      throw new ConfigError('internal: QMP_MCP_JWT_SECRET missing after validation');
    }
    // Pin to HS256 (also the provider default) so a token cannot request a
    // weaker or 'none' algorithm via its header.
    return new JWTAuthProvider({ secret: config.jwtSecret, algorithms: ['HS256'] });
  }
  // Default: API key in the X-API-Key header (the provider's default).
  return new APIKeyAuthProvider({ keys: config.apiKeys });
}

/**
 * Build the http-stream transport config from {@link Config}: bind host/port,
 * the MCP endpoint path, the DNS-rebinding/CORS origin allowlist, and the auth
 * provider (omitted entirely in insecure mode).
 */
export function buildHttpTransport(config: Config): TransportConfig {
  const options: HttpStreamTransportConfig = {
    host: config.httpHost,
    port: config.httpPort,
    endpoint: config.httpEndpoint,
    // Restrict browser origins so a malicious page cannot drive this port via a
    // DNS-rebinding attack: the framework rejects (403) any Origin not in this
    // allowlist before a handler runs. This allowlist — not the response's
    // Access-Control-Allow-Origin header — is the control; do not relax it.
    // Non-browser clients send no Origin and are allowed through.
    cors: { allowedOrigins: config.allowedOrigins },
  };
  const auth = buildAuthProvider(config);
  if (auth !== undefined) {
    options.auth = { provider: auth };
  } else {
    logger.warning(
      'QMP_MCP_ALLOW_INSECURE=true: serving the HTTP transport WITHOUT authentication. ' +
        'This is for local development only — never expose this port on an untrusted network.',
    );
  }
  return { type: 'http-stream', options };
}

/** Identity passed to the framework: server name, version, and tool basePath. */
interface ServerIdentity {
  name: string;
  version: string;
  basePath: string;
}

/**
 * Map a validated {@link Config} onto an {@link MCPServerConfig}: stdio uses a
 * single stdio transport; http uses a single http-stream transport; both runs
 * stdio and http-stream concurrently via the `transports` array. For http/both,
 * {@link loadConfig} has already failed closed if required auth was missing.
 */
export function buildServerConfig(config: Config, identity: ServerIdentity): MCPServerConfig {
  const baseConfig: MCPServerConfig = { ...identity, logging: true };
  if (config.transport === 'stdio') {
    return { ...baseConfig, transport: { type: 'stdio' } };
  }
  if (config.transport === 'http') {
    return { ...baseConfig, transport: buildHttpTransport(config) };
  }
  // 'both': run stdio and http-stream concurrently via the transports array.
  return { ...baseConfig, transports: [{ type: 'stdio' }, buildHttpTransport(config)] };
}

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

  // Pin discovery to the compiled directory so tools load from `dist/tools`
  // regardless of cwd (the default basePath is `cwd/dist`, wrong under npx).
  const server = new MCPServer(
    buildServerConfig(config, { name: 'qmp-mcp', version: readVersion(), basePath: here }),
  );

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

  const where =
    config.transport === 'stdio'
      ? 'transport=stdio'
      : `transport=${config.transport} on http://${config.httpHost}:${config.httpPort}${config.httpEndpoint}`;
  logger.info(`starting qmp-mcp (${where})`);
  // start() resolves when the server stops (signal, or stdin/transport close),
  // so this also covers the stdin end hook. Tear down the Instance before exit.
  await server.start();
  await shutdown('server stopped');
}

/**
 * Only boot when this module is the process entrypoint (i.e. `node dist/index.js`
 * or the `qmp-mcp` bin). When imported — e.g. by the wiring unit tests — the
 * exported builders are usable without starting a server or binding a port.
 */
const isEntrypoint =
  process.argv[1] !== undefined && import.meta.url === pathToFileURL(process.argv[1]).href;

if (isEntrypoint) {
  main().catch((err: unknown) => {
    logger.error(`fatal: ${err instanceof Error ? (err.stack ?? err.message) : String(err)}`);
    process.exitCode = 1;
  });
}
