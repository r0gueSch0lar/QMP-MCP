import { APIKeyAuthProvider, type HttpStreamTransportConfig, JWTAuthProvider } from 'mcp-framework';
import { describe, expect, it } from 'vitest';
import type { Config } from './config.js';
import { buildAuthProvider, buildHttpTransport, buildServerConfig } from './index.js';

/** A valid http+apikey Config; override per test. */
function makeConfig(overrides: Partial<Config> = {}): Config {
  return {
    transport: 'http',
    logLevel: 'info',
    httpHost: '127.0.0.1',
    httpPort: 8080,
    httpEndpoint: '/mcp',
    allowedOrigins: ['http://localhost:8080', 'http://127.0.0.1:8080'],
    authMode: 'apikey',
    apiKeys: ['k1', 'k2'],
    jwtSecret: undefined,
    allowInsecure: false,
    ...overrides,
  };
}

const identity = { name: 'qmp-mcp', version: '0.0.0', basePath: '/tmp' };

describe('buildAuthProvider', () => {
  it('builds an APIKeyAuthProvider with the configured keys on the X-API-Key header', () => {
    const provider = buildAuthProvider(makeConfig({ apiKeys: ['k1', 'k2', 'k3'] }));
    expect(provider).toBeInstanceOf(APIKeyAuthProvider);
    expect((provider as APIKeyAuthProvider).getKeyCount()).toBe(3);
    expect((provider as APIKeyAuthProvider).getHeaderName()).toBe('X-API-Key');
  });

  it('builds a JWTAuthProvider when authMode is jwt', () => {
    const provider = buildAuthProvider(makeConfig({ authMode: 'jwt', jwtSecret: 's3cr3t' }));
    expect(provider).toBeInstanceOf(JWTAuthProvider);
  });

  it('returns no provider when insecure mode is enabled', () => {
    expect(buildAuthProvider(makeConfig({ allowInsecure: true, apiKeys: [] }))).toBeUndefined();
  });
});

describe('buildHttpTransport', () => {
  it('wires host/port/endpoint and the DNS-rebinding origin allowlist', () => {
    const transport = buildHttpTransport(
      makeConfig({
        httpHost: '0.0.0.0',
        httpPort: 9000,
        httpEndpoint: '/rpc',
        allowedOrigins: ['https://app.example.com'],
      }),
    );
    expect(transport.type).toBe('http-stream');
    const options = transport.options as HttpStreamTransportConfig;
    expect(options.host).toBe('0.0.0.0');
    expect(options.port).toBe(9000);
    expect(options.endpoint).toBe('/rpc');
    expect(options.cors?.allowedOrigins).toEqual(['https://app.example.com']);
  });

  it('attaches the API-key provider as the transport auth', () => {
    const options = buildHttpTransport(makeConfig()).options as HttpStreamTransportConfig;
    expect(options.auth?.provider).toBeInstanceOf(APIKeyAuthProvider);
  });

  it('omits auth entirely in insecure mode', () => {
    const options = buildHttpTransport(makeConfig({ allowInsecure: true, apiKeys: [] }))
      .options as HttpStreamTransportConfig;
    expect(options.auth).toBeUndefined();
  });
});

describe('buildServerConfig', () => {
  it('uses a single stdio transport for transport=stdio', () => {
    const result = buildServerConfig(makeConfig({ transport: 'stdio' }), identity);
    expect(result.transport).toEqual({ type: 'stdio' });
    expect(result.transports).toBeUndefined();
    expect(result.basePath).toBe('/tmp');
  });

  it('uses a single http-stream transport for transport=http', () => {
    const result = buildServerConfig(makeConfig({ transport: 'http' }), identity);
    expect(result.transport?.type).toBe('http-stream');
    expect(result.transports).toBeUndefined();
  });

  it('runs stdio and http-stream concurrently for transport=both', () => {
    const result = buildServerConfig(makeConfig({ transport: 'both' }), identity);
    expect(result.transport).toBeUndefined();
    expect(result.transports?.map((t) => t.type)).toEqual(['stdio', 'http-stream']);
  });
});
