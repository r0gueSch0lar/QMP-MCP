import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';
import { type Config, ConfigError, loadConfig } from './config.js';

/** The host-agnostic default Image Store dir for an empty environment. */
const DEFAULT_IMAGE_DIR = join(tmpdir(), 'qmp-mcp', 'images');

/** The host-agnostic default ISO Store dir for an empty environment. */
const DEFAULT_ISO_DIR = join(tmpdir(), 'qmp-mcp', 'isos');

/** The full default config when the environment is empty (stdio, no auth). */
const DEFAULTS: Config = {
  transport: 'stdio',
  logLevel: 'info',
  httpHost: '127.0.0.1',
  httpPort: 8080,
  httpEndpoint: '/mcp',
  allowedOrigins: ['http://localhost:8080', 'http://127.0.0.1:8080'],
  authMode: 'apikey',
  apiKeys: [],
  jwtSecret: undefined,
  allowInsecure: false,
  imageDir: DEFAULT_IMAGE_DIR,
  isoDir: DEFAULT_ISO_DIR,
  maxDiskGb: 64,
};

describe('loadConfig', () => {
  it('defaults to a stdio, no-auth config when env is empty', () => {
    expect(loadConfig({})).toEqual(DEFAULTS);
  });

  it('reads valid values and normalises case', () => {
    const config = loadConfig({
      QMP_MCP_TRANSPORT: 'HTTP',
      QMP_MCP_LOG_LEVEL: 'Debug',
      QMP_MCP_AUTH: 'ApiKey',
      QMP_MCP_API_KEYS: 'k1',
    });
    expect(config.transport).toBe('http');
    expect(config.logLevel).toBe('debug');
    expect(config.authMode).toBe('apikey');
  });

  it('treats an empty string as unset and uses the default', () => {
    expect(loadConfig({ QMP_MCP_TRANSPORT: '' })).toEqual(DEFAULTS);
  });

  it('rejects an invalid transport, naming the variable and the allowed values', () => {
    expect(() => loadConfig({ QMP_MCP_TRANSPORT: 'ftp' })).toThrowError(
      /QMP_MCP_TRANSPORT must be one of: stdio, http, both/,
    );
  });

  it('rejects an invalid log level with a ConfigError that names the variable', () => {
    let thrown: unknown;
    try {
      loadConfig({ QMP_MCP_LOG_LEVEL: 'verbose' });
    } catch (err) {
      thrown = err;
    }
    expect(thrown).toBeInstanceOf(ConfigError);
    expect((thrown as Error).message).toContain('QMP_MCP_LOG_LEVEL');
    expect((thrown as Error).message).toContain('debug, info, warning, error');
  });

  describe('HTTP host/port/endpoint', () => {
    it('uses safe defaults (127.0.0.1:8080 /mcp) when unset', () => {
      const config = loadConfig({ QMP_MCP_TRANSPORT: 'http', QMP_MCP_API_KEYS: 'k' });
      expect(config.httpHost).toBe('127.0.0.1');
      expect(config.httpPort).toBe(8080);
      expect(config.httpEndpoint).toBe('/mcp');
    });

    it('reads host/port/endpoint from env and trims them', () => {
      const config = loadConfig({
        QMP_MCP_TRANSPORT: 'http',
        QMP_MCP_API_KEYS: 'k',
        QMP_MCP_HTTP_HOST: ' 0.0.0.0 ',
        QMP_MCP_HTTP_PORT: '9000',
        QMP_MCP_HTTP_ENDPOINT: ' /rpc ',
      });
      expect(config.httpHost).toBe('0.0.0.0');
      expect(config.httpPort).toBe(9000);
      expect(config.httpEndpoint).toBe('/rpc');
    });

    it('derives the default allowed origins from the configured port', () => {
      const config = loadConfig({
        QMP_MCP_TRANSPORT: 'http',
        QMP_MCP_API_KEYS: 'k',
        QMP_MCP_HTTP_PORT: '9000',
      });
      expect(config.allowedOrigins).toEqual(['http://localhost:9000', 'http://127.0.0.1:9000']);
    });

    it('lets an explicit allowed-origins list override the default', () => {
      const config = loadConfig({
        QMP_MCP_TRANSPORT: 'http',
        QMP_MCP_API_KEYS: 'k',
        QMP_MCP_HTTP_ALLOWED_ORIGINS: 'https://app.example.com, https://admin.example.com ',
      });
      expect(config.allowedOrigins).toEqual([
        'https://app.example.com',
        'https://admin.example.com',
      ]);
    });

    it.each([
      'abc',
      '8080x',
      '0',
      '70000',
      '-1',
      '80.5',
    ])('fails closed on the invalid port %p', (port) => {
      expect(() =>
        loadConfig({ QMP_MCP_TRANSPORT: 'http', QMP_MCP_API_KEYS: 'k', QMP_MCP_HTTP_PORT: port }),
      ).toThrowError(/QMP_MCP_HTTP_PORT must be an integer port in 1\.\.65535/);
    });
  });

  describe('API-key fail-closed (ADR-0005)', () => {
    it('throws naming QMP_MCP_API_KEYS and QMP_MCP_ALLOW_INSECURE when http has no keys', () => {
      let thrown: unknown;
      try {
        loadConfig({ QMP_MCP_TRANSPORT: 'http' });
      } catch (err) {
        thrown = err;
      }
      expect(thrown).toBeInstanceOf(ConfigError);
      expect((thrown as Error).message).toContain('QMP_MCP_API_KEYS');
      expect((thrown as Error).message).toContain('QMP_MCP_ALLOW_INSECURE');
    });

    it('also fails closed for transport "both" with no keys', () => {
      expect(() => loadConfig({ QMP_MCP_TRANSPORT: 'both' })).toThrowError(ConfigError);
    });

    it('treats a keys value of only commas/whitespace as empty and fails closed', () => {
      expect(() =>
        loadConfig({ QMP_MCP_TRANSPORT: 'http', QMP_MCP_API_KEYS: ' , ,, ' }),
      ).toThrowError(ConfigError);
    });

    it('accepts http when keys are configured, parsing and trimming them', () => {
      const config = loadConfig({
        QMP_MCP_TRANSPORT: 'http',
        QMP_MCP_API_KEYS: 'k1, k2 ,, k3 ',
      });
      expect(config.apiKeys).toEqual(['k1', 'k2', 'k3']);
      expect(config.authMode).toBe('apikey');
    });

    it('permits unauthenticated http when QMP_MCP_ALLOW_INSECURE=true with no keys', () => {
      const config = loadConfig({ QMP_MCP_TRANSPORT: 'http', QMP_MCP_ALLOW_INSECURE: 'true' });
      expect(config.allowInsecure).toBe(true);
      expect(config.apiKeys).toEqual([]);
    });
  });

  describe('JWT fail-closed (ADR-0005)', () => {
    it('throws naming QMP_MCP_JWT_SECRET when http+jwt has no secret', () => {
      let thrown: unknown;
      try {
        loadConfig({ QMP_MCP_TRANSPORT: 'http', QMP_MCP_AUTH: 'jwt' });
      } catch (err) {
        thrown = err;
      }
      expect(thrown).toBeInstanceOf(ConfigError);
      expect((thrown as Error).message).toContain('QMP_MCP_JWT_SECRET');
    });

    it('treats a whitespace-only secret as unset and fails closed', () => {
      expect(() =>
        loadConfig({ QMP_MCP_TRANSPORT: 'http', QMP_MCP_AUTH: 'jwt', QMP_MCP_JWT_SECRET: '   ' }),
      ).toThrowError(/QMP_MCP_JWT_SECRET/);
    });

    it('accepts http+jwt when the secret is set', () => {
      const config = loadConfig({
        QMP_MCP_TRANSPORT: 'http',
        QMP_MCP_AUTH: 'jwt',
        QMP_MCP_JWT_SECRET: 's3cr3t',
      });
      expect(config.authMode).toBe('jwt');
      expect(config.jwtSecret).toBe('s3cr3t');
    });
  });

  describe('insecure override', () => {
    it('rejects a non-boolean QMP_MCP_ALLOW_INSECURE', () => {
      expect(() =>
        loadConfig({ QMP_MCP_TRANSPORT: 'http', QMP_MCP_ALLOW_INSECURE: 'yes' }),
      ).toThrowError(/QMP_MCP_ALLOW_INSECURE must be "true" or "false"/);
    });
  });

  describe('Image Store (ADR-0006)', () => {
    it('defaults the Image Store dir host-agnostically and the size cap to 64 GiB', () => {
      const config = loadConfig({});
      expect(config.imageDir).toBe(DEFAULT_IMAGE_DIR);
      expect(config.maxDiskGb).toBe(64);
    });

    it('takes an explicit QMP_MCP_IMAGE_DIR verbatim, trimmed', () => {
      expect(loadConfig({ QMP_MCP_IMAGE_DIR: ' /srv/images ' }).imageDir).toBe('/srv/images');
    });

    it('derives the default dir from XDG_DATA_HOME, then HOME', () => {
      expect(loadConfig({ XDG_DATA_HOME: '/x/data' }).imageDir).toBe('/x/data/qmp-mcp/images');
      expect(loadConfig({ HOME: '/home/u' }).imageDir).toBe('/home/u/.local/share/qmp-mcp/images');
    });

    it('reads QMP_MCP_MAX_DISK_GB and fails closed on a non-positive-integer value', () => {
      expect(loadConfig({ QMP_MCP_MAX_DISK_GB: '128' }).maxDiskGb).toBe(128);
      expect(() => loadConfig({ QMP_MCP_MAX_DISK_GB: 'big' })).toThrowError(
        /QMP_MCP_MAX_DISK_GB must be a positive integer/,
      );
      expect(() => loadConfig({ QMP_MCP_MAX_DISK_GB: '0' })).toThrowError(/QMP_MCP_MAX_DISK_GB/);
    });
  });

  describe('ISO Store (ADR-0006)', () => {
    it('defaults the ISO Store dir host-agnostically, separate from the Image Store', () => {
      const config = loadConfig({});
      expect(config.isoDir).toBe(DEFAULT_ISO_DIR);
      // The two stores are SEPARATE directories (read-write vs read-only).
      expect(config.isoDir).not.toBe(config.imageDir);
    });

    it('takes an explicit QMP_MCP_ISO_DIR verbatim, trimmed', () => {
      expect(loadConfig({ QMP_MCP_ISO_DIR: ' /srv/isos ' }).isoDir).toBe('/srv/isos');
    });

    it('derives the default dir from XDG_DATA_HOME, then HOME', () => {
      expect(loadConfig({ XDG_DATA_HOME: '/x/data' }).isoDir).toBe('/x/data/qmp-mcp/isos');
      expect(loadConfig({ HOME: '/home/u' }).isoDir).toBe('/home/u/.local/share/qmp-mcp/isos');
    });
  });

  it('does not require http auth when the transport is stdio', () => {
    // stdio never exposes a network port, so missing http auth must not throw.
    expect(() => loadConfig({ QMP_MCP_TRANSPORT: 'stdio' })).not.toThrow();
    expect(loadConfig({ QMP_MCP_AUTH: 'jwt' }).authMode).toBe('jwt');
  });
});
