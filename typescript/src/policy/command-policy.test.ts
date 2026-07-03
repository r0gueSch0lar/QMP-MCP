import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import {
  buildPolicy,
  CommandPolicyError,
  DEFAULT_ALLOWLIST,
  decideCommand,
  HARD_DENYLIST,
  normalizeCommandName,
  PolicyError,
  type ResolvedPolicy,
  resolveCommandPolicy,
} from './command-policy.js';

/** The default policy (built-in allowlist, no overrides). */
const defaults = (): ResolvedPolicy => buildPolicy();

describe('Command Policy invariants', () => {
  it('the hard denylist and the default allowlist are disjoint', () => {
    for (const cmd of DEFAULT_ALLOWLIST) {
      expect(HARD_DENYLIST.has(cmd)).toBe(false);
    }
  });

  it('every hard-denylist entry is already normalised (trimmed + lower-case)', () => {
    for (const cmd of HARD_DENYLIST) {
      expect(cmd).toBe(normalizeCommandName(cmd));
    }
  });

  it('includes every command the issue requires in the hard set', () => {
    // The original required 15 — these must always remain hard-denied.
    for (const cmd of [
      'human-monitor-command',
      'migrate',
      'migrate-incoming',
      'migrate-set-parameters',
      'dump-guest-memory',
      'pmemsave',
      'memsave',
      'object-add',
      'blockdev-add',
      'device_add',
      'netdev_add',
      'chardev-add',
      'chardev-change',
      'getfd',
      'add-fd',
    ]) {
      expect(HARD_DENYLIST.has(cmd)).toBe(true);
    }
  });

  it('also hard-denies the widened host-file/host-state backstop set', () => {
    // Added so an operator who broadens the allowlist still cannot enable any
    // command with the same host-file/host-state capability.
    for (const cmd of [
      'xen-save-devices-state',
      'xen-load-devices-state',
      'qom-set',
      'block-commit',
      'block-stream',
      'block_resize',
      'blockdev-snapshot',
      'blockdev-snapshot-sync',
      'blockdev-snapshot-internal-sync',
      'migrate-recover',
      'migrate-continue',
      'migrate-pause',
      'migrate-start-postcopy',
    ]) {
      expect(HARD_DENYLIST.has(cmd)).toBe(true);
    }
  });
});

describe('decideCommand — hard denylist (immutable)', () => {
  it('refuses every hard-denied command under the default policy', () => {
    for (const cmd of HARD_DENYLIST) {
      const verdict = decideCommand(defaults(), cmd);
      expect(verdict.allowed).toBe(false);
      if (!verdict.allowed) expect(verdict.hardDenied).toBe(true);
    }
  });

  it('STILL refuses every hard-denied command when an allow override tries to enable it', () => {
    for (const cmd of HARD_DENYLIST) {
      // Allowed via the override list — must remain denied (hard denylist wins).
      const policy = buildPolicy({ allow: [cmd] });
      const verdict = decideCommand(policy, cmd);
      expect(verdict.allowed).toBe(false);
      if (!verdict.allowed) {
        expect(verdict.hardDenied).toBe(true);
        expect(verdict.reason).toMatch(/hard denylist/i);
      }
    }
  });

  it('STILL refuses a hard-denied command when QMP_MCP_ALLOW tries to enable it', () => {
    const policy = resolveCommandPolicy({ QMP_MCP_ALLOW: 'human-monitor-command, migrate' });
    for (const cmd of ['human-monitor-command', 'migrate']) {
      const verdict = decideCommand(policy, cmd);
      expect(verdict.allowed).toBe(false);
      if (!verdict.allowed) expect(verdict.hardDenied).toBe(true);
    }
  });
});

describe('decideCommand — default allowlist & default-deny', () => {
  it('allows a default-allowlisted command (query-status)', () => {
    expect(decideCommand(defaults(), 'query-status')).toEqual({
      allowed: true,
      command: 'query-status',
    });
  });

  it('denies an unknown command by default, not as a hard denial', () => {
    const verdict = decideCommand(defaults(), 'totally-made-up-command');
    expect(verdict.allowed).toBe(false);
    if (!verdict.allowed) {
      expect(verdict.hardDenied).toBe(false);
      expect(verdict.reason).toMatch(/not in the Command Policy allowlist/i);
    }
  });
});

describe('decideCommand — screendump is NOT generically executable (#11)', () => {
  it('screendump is absent from the default allowlist', () => {
    // It writes an arbitrary host file via its `filename` arg, and the policy
    // gates command NAMES not arguments — so it must NOT be generically runnable.
    expect(DEFAULT_ALLOWLIST.has('screendump')).toBe(false);
  });

  it('denies screendump under the default policy (default-deny, not hard-denied)', () => {
    const verdict = decideCommand(defaults(), 'screendump');
    expect(verdict.allowed).toBe(false);
    if (!verdict.allowed) {
      // The dedicated screendump tool still serves it with a server-chosen path,
      // so this is an ordinary default-deny, not a hard denial.
      expect(verdict.hardDenied).toBe(false);
      expect(verdict.reason).toMatch(/not in the Command Policy allowlist/i);
    }
  });
});

describe('decideCommand — case/whitespace evasion is blocked', () => {
  it.each([
    ' migrate ',
    'MIGRATE',
    'Migrate',
    '  Human-Monitor-Command  ',
    'HUMAN-MONITOR-COMMAND',
    'Device_Add',
  ])('treats %j as its hard-denied canonical command', (evasion) => {
    const verdict = decideCommand(defaults(), evasion);
    expect(verdict.allowed).toBe(false);
    if (!verdict.allowed) expect(verdict.hardDenied).toBe(true);
  });

  it('normalises an allowlisted command with stray case/space and forwards the canonical name', () => {
    const verdict = decideCommand(defaults(), '  Query-Status  ');
    expect(verdict).toEqual({ allowed: true, command: 'query-status' });
  });
});

describe('non-string command is fail-closed (not a thrown TypeError)', () => {
  it.each([
    undefined,
    null,
    42,
    {},
    [],
    true,
  ])('decideCommand denies a non-string command (%j) without throwing', (bad) => {
    const verdict = decideCommand(defaults(), bad as unknown as string);
    expect(verdict.allowed).toBe(false);
    if (!verdict.allowed) {
      expect(verdict.hardDenied).toBe(false);
      expect(verdict.reason).toMatch(/must be a string/i);
    }
  });

  it('normalizeCommandName returns the empty string for a non-string input', () => {
    expect(normalizeCommandName(undefined as unknown as string)).toBe('');
    expect(normalizeCommandName(42 as unknown as string)).toBe('');
  });
});

describe('overrides — env QMP_MCP_ALLOW / QMP_MCP_DENY', () => {
  it('QMP_MCP_ALLOW adds a safe command that was previously default-denied', () => {
    // query-rocker is a real read-only QMP command, not in the built-in defaults.
    expect(decideCommand(defaults(), 'query-rocker').allowed).toBe(false);
    const policy = resolveCommandPolicy({ QMP_MCP_ALLOW: 'query-rocker' });
    expect(decideCommand(policy, 'query-rocker')).toEqual({
      allowed: true,
      command: 'query-rocker',
    });
  });

  it('QMP_MCP_DENY removes a command from the allowlist', () => {
    expect(decideCommand(defaults(), 'system_reset').allowed).toBe(true);
    const policy = resolveCommandPolicy({ QMP_MCP_DENY: 'system_reset' });
    const verdict = decideCommand(policy, 'system_reset');
    expect(verdict.allowed).toBe(false);
    if (!verdict.allowed) {
      expect(verdict.hardDenied).toBe(false);
      expect(verdict.reason).toMatch(/denied by the Command Policy/i);
    }
  });

  it('deny wins over allow when a command is in both (fail-closed precedence)', () => {
    const policy = resolveCommandPolicy({
      QMP_MCP_ALLOW: 'query-rocker',
      QMP_MCP_DENY: 'query-rocker',
    });
    expect(decideCommand(policy, 'query-rocker').allowed).toBe(false);
  });
});

describe('overrides — YAML policy file', () => {
  let dir: string;
  beforeEach(async () => {
    dir = await mkdtemp(join(tmpdir(), 'policy-'));
  });
  afterEach(async () => {
    await rm(dir, { recursive: true, force: true });
  });

  const write = async (name: string, body: string): Promise<string> => {
    const path = join(dir, name);
    await writeFile(path, body);
    return path;
  };

  it('honours the file allow and deny lists', async () => {
    const path = await write('policy.yaml', 'allow:\n  - query-rocker\ndeny:\n  - system_reset\n');
    const policy = resolveCommandPolicy({ QMP_MCP_POLICY_FILE: path });
    expect(decideCommand(policy, 'query-rocker').allowed).toBe(true);
    expect(decideCommand(policy, 'system_reset').allowed).toBe(false);
  });

  it('keeps a hard-denied command denied even when the file allows it (the key AC)', async () => {
    const path = await write('policy.yaml', 'allow:\n  - migrate\n  - human-monitor-command\n');
    const policy = resolveCommandPolicy({ QMP_MCP_POLICY_FILE: path });
    for (const cmd of ['migrate', 'human-monitor-command']) {
      const verdict = decideCommand(policy, cmd);
      expect(verdict.allowed).toBe(false);
      if (!verdict.allowed) expect(verdict.hardDenied).toBe(true);
    }
  });

  it('merges file and env overrides (deny still wins)', async () => {
    const path = await write('policy.yaml', 'allow:\n  - query-rocker\n');
    const policy = resolveCommandPolicy({
      QMP_MCP_POLICY_FILE: path,
      QMP_MCP_DENY: 'query-rocker',
    });
    expect(decideCommand(policy, 'query-rocker').allowed).toBe(false);
  });

  it('treats an empty file as an empty (defaults-only) policy', async () => {
    const path = await write('empty.yaml', '');
    const policy = resolveCommandPolicy({ QMP_MCP_POLICY_FILE: path });
    expect(decideCommand(policy, 'query-status').allowed).toBe(true);
    expect(decideCommand(policy, 'query-rocker').allowed).toBe(false);
  });

  it('fails closed on a MISSING policy file, naming QMP_MCP_POLICY_FILE', () => {
    const missing = join(dir, 'does-not-exist.yaml');
    expect(() => resolveCommandPolicy({ QMP_MCP_POLICY_FILE: missing })).toThrow(PolicyError);
    expect(() => resolveCommandPolicy({ QMP_MCP_POLICY_FILE: missing })).toThrow(
      /QMP_MCP_POLICY_FILE could not be read/,
    );
  });

  it('fails closed on MALFORMED YAML, naming QMP_MCP_POLICY_FILE', async () => {
    const path = await write('bad.yaml', 'allow: "unterminated\n');
    expect(() => resolveCommandPolicy({ QMP_MCP_POLICY_FILE: path })).toThrow(PolicyError);
    expect(() => resolveCommandPolicy({ QMP_MCP_POLICY_FILE: path })).toThrow(
      /QMP_MCP_POLICY_FILE is not valid YAML/,
    );
  });

  it('fails closed on a wrong-shaped file (allow is not a list)', async () => {
    const path = await write('shape.yaml', 'allow: query-status\n');
    expect(() => resolveCommandPolicy({ QMP_MCP_POLICY_FILE: path })).toThrow(
      /QMP_MCP_POLICY_FILE has the wrong shape/,
    );
  });

  it('fails closed on an unknown top-level key (typo guard)', async () => {
    const path = await write('typo.yaml', 'allows:\n  - query-pci\n');
    expect(() => resolveCommandPolicy({ QMP_MCP_POLICY_FILE: path })).toThrow(
      /QMP_MCP_POLICY_FILE has the wrong shape/,
    );
  });
});

describe('CommandPolicyError', () => {
  it('carries the hardDenied flag', () => {
    const err = new CommandPolicyError('nope', true);
    expect(err).toBeInstanceOf(Error);
    expect(err.name).toBe('CommandPolicyError');
    expect(err.hardDenied).toBe(true);
  });
});
