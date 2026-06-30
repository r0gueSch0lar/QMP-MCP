import { describe, expect, it } from 'vitest';
import { type Config, ConfigError, loadConfig } from './config.js';

describe('loadConfig', () => {
  it('defaults to stdio transport and info log level when env is empty', () => {
    const config: Config = loadConfig({});
    expect(config).toEqual({ transport: 'stdio', logLevel: 'info' });
  });

  it('reads valid values and normalises case', () => {
    expect(loadConfig({ QMP_MCP_TRANSPORT: 'HTTP', QMP_MCP_LOG_LEVEL: 'Debug' })).toEqual({
      transport: 'http',
      logLevel: 'debug',
    });
  });

  it('treats an empty string as unset and uses the default', () => {
    expect(loadConfig({ QMP_MCP_TRANSPORT: '' })).toEqual({
      transport: 'stdio',
      logLevel: 'info',
    });
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
});
