import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { closeLogFile, getLogger, openLogFile, setLogFilter, setLogLevel } from './logger.js';

/** Capture stderr lines emitted during `fn` without letting them reach the terminal. */
function captureStderr(fn: () => void): string[] {
  const lines: string[] = [];
  const spy = vi.spyOn(process.stderr, 'write').mockImplementation((chunk) => {
    lines.push(String(chunk));
    return true;
  });
  try {
    fn();
  } finally {
    spy.mockRestore();
  }
  return lines;
}

afterEach(() => {
  setLogLevel('info');
  setLogFilter({});
  closeLogFile();
});

describe('per-subsystem filtering (QMP_MCP_LOG_FILTER)', () => {
  it('an override quiets one subsystem without touching the rest', () => {
    setLogLevel('info');
    setLogFilter({ qmp: 'error' });
    const lines = captureStderr(() => {
      getLogger('qmp').info('wire chatter');
      getLogger('qmp').error('wire failure');
      getLogger('server').info('startup');
    });
    expect(lines).toEqual([
      '[qmp-mcp] error qmp: wire failure\n',
      '[qmp-mcp] info server: startup\n',
    ]);
  });

  it('an override can also make one subsystem more verbose than the global level', () => {
    setLogLevel('warning');
    setLogFilter({ download: 'debug' });
    const lines = captureStderr(() => {
      getLogger('download').debug('mirror probe');
      getLogger('server').info('suppressed by the global level');
    });
    expect(lines).toEqual(['[qmp-mcp] debug download: mirror probe\n']);
  });
});

describe('log file sink (QMP_MCP_LOG_FILE)', () => {
  it('appends every emitted line to the file, and only emitted ones', () => {
    const dir = mkdtempSync(join(tmpdir(), 'qmp-log-'));
    const path = join(dir, 'server.log');
    try {
      setLogLevel('info');
      openLogFile(path);
      captureStderr(() => {
        getLogger('server').info('first');
        getLogger('server').debug('filtered out');
      });
      closeLogFile();
      openLogFile(path);
      captureStderr(() => {
        getLogger('server').info('second');
      });
      closeLogFile();
      expect(readFileSync(path, 'utf8')).toBe(
        '[qmp-mcp] info server: first\n[qmp-mcp] info server: second\n',
      );
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it('fails closed, naming the variable, when the directory does not exist', () => {
    const missing = join(tmpdir(), `qmp-log-missing-${process.pid}-${Date.now()}`, 'x.log');
    expect(() => openLogFile(missing)).toThrowError(/QMP_MCP_LOG_FILE.*does not exist/);
  });
});
