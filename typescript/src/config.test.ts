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
  qemuBinary: 'qemu-system-x86_64',
  maxDiskGb: 64,
  maxMemoryMb: 4096,
  maxVcpus: 2,
  hostfwdPortRange: { low: 1024, high: 65535 },
  allowHostNet: false,
  autoStart: false,
  eventBufferSize: 256,
  allowRawArgs: false,
  viewerPassword: undefined,
  viewerHost: '127.0.0.1',
  viewerPort: 6080,
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

  it('reads QMP_MCP_AUTO_START (default false, opt-in true) — issue #8', () => {
    expect(loadConfig({}).autoStart).toBe(false);
    expect(loadConfig({ QMP_MCP_AUTO_START: 'true' }).autoStart).toBe(true);
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

  describe('resource caps (issue #9)', () => {
    it('defaults the memory cap to 4096 MiB and the vCPU cap to 2', () => {
      const config = loadConfig({});
      expect(config.maxMemoryMb).toBe(4096);
      expect(config.maxVcpus).toBe(2);
    });

    it('reads QMP_MCP_MAX_MEMORY_MB and fails closed on a non-positive-integer value', () => {
      // A higher env cap admits a larger spec.
      expect(loadConfig({ QMP_MCP_MAX_MEMORY_MB: '32768' }).maxMemoryMb).toBe(32768);
      expect(() => loadConfig({ QMP_MCP_MAX_MEMORY_MB: 'lots' })).toThrowError(
        /QMP_MCP_MAX_MEMORY_MB must be a positive integer/,
      );
      expect(() => loadConfig({ QMP_MCP_MAX_MEMORY_MB: '0' })).toThrowError(
        /QMP_MCP_MAX_MEMORY_MB/,
      );
    });

    it('reads QMP_MCP_MAX_VCPUS and fails closed on a non-positive-integer value', () => {
      // A higher env cap admits a larger spec.
      expect(loadConfig({ QMP_MCP_MAX_VCPUS: '16' }).maxVcpus).toBe(16);
      expect(() => loadConfig({ QMP_MCP_MAX_VCPUS: 'many' })).toThrowError(
        /QMP_MCP_MAX_VCPUS must be a positive integer/,
      );
      expect(() => loadConfig({ QMP_MCP_MAX_VCPUS: '0' })).toThrowError(/QMP_MCP_MAX_VCPUS/);
    });

    it('reads QMP_MCP_EVENT_BUFFER_SIZE and fails closed on a non-positive-integer value', () => {
      expect(loadConfig({}).eventBufferSize).toBe(256);
      expect(loadConfig({ QMP_MCP_EVENT_BUFFER_SIZE: '1024' }).eventBufferSize).toBe(1024);
      expect(() => loadConfig({ QMP_MCP_EVENT_BUFFER_SIZE: 'big' })).toThrowError(
        /QMP_MCP_EVENT_BUFFER_SIZE must be a positive integer/,
      );
      expect(() => loadConfig({ QMP_MCP_EVENT_BUFFER_SIZE: '0' })).toThrowError(
        /QMP_MCP_EVENT_BUFFER_SIZE/,
      );
    });

    it('reads QMP_MCP_ALLOW_RAW_ARGS, defaulting closed and failing closed on garbage', () => {
      expect(loadConfig({}).allowRawArgs).toBe(false);
      expect(loadConfig({ QMP_MCP_ALLOW_RAW_ARGS: 'true' }).allowRawArgs).toBe(true);
      expect(loadConfig({ QMP_MCP_ALLOW_RAW_ARGS: 'false' }).allowRawArgs).toBe(false);
      expect(() => loadConfig({ QMP_MCP_ALLOW_RAW_ARGS: 'yes' })).toThrowError(
        /QMP_MCP_ALLOW_RAW_ARGS must be "true" or "false"/,
      );
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

  describe('QEMU binary (issue #15)', () => {
    it('defaults to qemu-system-x86_64 when unset', () => {
      expect(loadConfig({}).qemuBinary).toBe('qemu-system-x86_64');
    });

    it('honors an explicit override, selecting the guest architecture', () => {
      // A non-x86 emulator selects the guest architecture; a bare name and an
      // absolute path are both accepted, and the value is trimmed.
      expect(loadConfig({ QMP_MCP_QEMU_BINARY: 'qemu-system-aarch64' }).qemuBinary).toBe(
        'qemu-system-aarch64',
      );
      expect(loadConfig({ QMP_MCP_QEMU_BINARY: ' /usr/bin/qemu-system-riscv64 ' }).qemuBinary).toBe(
        '/usr/bin/qemu-system-riscv64',
      );
    });

    it('treats blank/whitespace-only as unset (falls back to the default)', () => {
      expect(loadConfig({ QMP_MCP_QEMU_BINARY: '' }).qemuBinary).toBe('qemu-system-x86_64');
      expect(loadConfig({ QMP_MCP_QEMU_BINARY: '   ' }).qemuBinary).toBe('qemu-system-x86_64');
    });

    it.each([
      'qemu; rm -rf',
      'qemu-system-x86_64 --enable-kvm',
      'qemu\tsystem',
      '$(rm -rf /)',
      'qemu|nc',
      '../bin/qemu-system-aarch64',
      './qemu',
      'build/qemu-system-aarch64',
    ])('fails closed on the unsafe value %p, naming the variable', (value) => {
      let thrown: unknown;
      try {
        loadConfig({ QMP_MCP_QEMU_BINARY: value });
      } catch (err) {
        thrown = err;
      }
      expect(thrown).toBeInstanceOf(ConfigError);
      expect((thrown as Error).message).toContain('QMP_MCP_QEMU_BINARY');
    });
  });

  describe('guest networking (ADR-0009)', () => {
    it('defaults the host-forward range to the non-privileged 1024-65535 and host net off', () => {
      const config = loadConfig({});
      expect(config.hostfwdPortRange).toEqual({ low: 1024, high: 65535 });
      expect(config.allowHostNet).toBe(false);
    });

    it('reads a valid QMP_MCP_HOSTFWD_PORT_RANGE', () => {
      expect(loadConfig({ QMP_MCP_HOSTFWD_PORT_RANGE: '2000-3000' }).hostfwdPortRange).toEqual({
        low: 2000,
        high: 3000,
      });
      // a single-port range (low == high) is allowed
      expect(loadConfig({ QMP_MCP_HOSTFWD_PORT_RANGE: '8080-8080' }).hostfwdPortRange).toEqual({
        low: 8080,
        high: 8080,
      });
    });

    it.each([
      'abc',
      '1024',
      '1024-',
      '-65535',
      '0-65535',
      '1024-70000',
      '3000-2000',
      '1024-65535-2',
      ' 1024 - 2048 ',
    ])('fails closed on the invalid range %p, naming the variable', (range) => {
      let thrown: unknown;
      try {
        loadConfig({ QMP_MCP_HOSTFWD_PORT_RANGE: range });
      } catch (err) {
        thrown = err;
      }
      expect(thrown).toBeInstanceOf(ConfigError);
      expect((thrown as Error).message).toContain('QMP_MCP_HOSTFWD_PORT_RANGE');
    });

    it('reads QMP_MCP_ALLOW_HOST_NET as a boolean and fails closed on garbage', () => {
      expect(loadConfig({ QMP_MCP_ALLOW_HOST_NET: 'true' }).allowHostNet).toBe(true);
      expect(loadConfig({ QMP_MCP_ALLOW_HOST_NET: 'False' }).allowHostNet).toBe(false);
      expect(() => loadConfig({ QMP_MCP_ALLOW_HOST_NET: 'yes' })).toThrowError(
        /QMP_MCP_ALLOW_HOST_NET must be "true" or "false"/,
      );
    });
  });

  describe('noVNC Viewer (ADR-0010)', () => {
    it('defaults the Viewer host/port and leaves the password unset', () => {
      const config = loadConfig({});
      expect(config.viewerHost).toBe('127.0.0.1');
      expect(config.viewerPort).toBe(6080);
      expect(config.viewerPassword).toBeUndefined();
    });

    it('reads the Viewer password, host and port from env', () => {
      const config = loadConfig({
        QMP_MCP_VIEWER_PASSWORD: 'view-secret',
        QMP_MCP_VIEWER_HOST: '0.0.0.0',
        QMP_MCP_VIEWER_PORT: '7000',
      });
      expect(config.viewerPassword).toBe('view-secret');
      expect(config.viewerHost).toBe('0.0.0.0');
      expect(config.viewerPort).toBe(7000);
    });

    it('treats a whitespace-only Viewer password as unset (fail-closed)', () => {
      expect(loadConfig({ QMP_MCP_VIEWER_PASSWORD: '   ' }).viewerPassword).toBeUndefined();
    });

    it('fails closed on a non-integer Viewer port, naming the variable', () => {
      expect(() => loadConfig({ QMP_MCP_VIEWER_PORT: 'abc' })).toThrowError(
        /QMP_MCP_VIEWER_PORT must be an integer port in 1\.\.65535/,
      );
    });
  });

  it('does not require http auth when the transport is stdio', () => {
    // stdio never exposes a network port, so missing http auth must not throw.
    expect(() => loadConfig({ QMP_MCP_TRANSPORT: 'stdio' })).not.toThrow();
    expect(loadConfig({ QMP_MCP_AUTH: 'jwt' }).authMode).toBe('jwt');
  });
});
