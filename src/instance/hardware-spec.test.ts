import { describe, expect, it } from 'vitest';
import {
  AccelError,
  buildArgv,
  type HardwareSpec,
  HardwareSpecError,
  parseHardwareSpec,
  resolveAccel,
} from './hardware-spec.js';

const SOCK = '/run/qmp-mcp/qmp.sock';

/** A fully-resolved spec with all defaults applied, for argv tests. */
function spec(overrides: Partial<HardwareSpec> = {}): HardwareSpec {
  return parseHardwareSpec(overrides);
}

describe('parseHardwareSpec', () => {
  it('fills every field with a default for an empty spec', () => {
    expect(parseHardwareSpec({})).toEqual({
      machine: 'q35',
      cpu: 'max',
      vcpus: 1,
      memoryMb: 256,
      accel: 'auto',
    });
  });

  it('rejects an unknown field, failing closed', () => {
    expect(() => parseHardwareSpec({ disk: 'foo.qcow2' })).toThrow(HardwareSpecError);
  });

  it('names the offending field on a bad value', () => {
    expect(() => parseHardwareSpec({ vcpus: 0 })).toThrowError(/vcpus/);
    expect(() => parseHardwareSpec({ accel: 'xen' })).toThrowError(/accel/);
  });

  it('coerces nothing: a non-integer vcpu count is rejected', () => {
    expect(() => parseHardwareSpec({ vcpus: 1.5 })).toThrowError(/vcpus/);
  });
});

describe('buildArgv', () => {
  it('maps machine, cpu, smp and memory from the spec', () => {
    const argv = buildArgv(spec({ machine: 'pc', cpu: 'host', vcpus: 4, memoryMb: 2048 }), {
      accel: 'tcg',
      qmpSocketPath: SOCK,
    });
    expect(argv).toContain('-cpu');
    expect(argv[argv.indexOf('-cpu') + 1]).toBe('host');
    expect(argv[argv.indexOf('-smp') + 1]).toBe('4');
    expect(argv[argv.indexOf('-m') + 1]).toBe('2048');
    expect(argv[argv.indexOf('-machine') + 1]).toBe('pc,accel=tcg');
  });

  it('encodes the accelerator into the -machine value (tcg)', () => {
    const argv = buildArgv(spec(), { accel: 'tcg', qmpSocketPath: SOCK });
    expect(argv[argv.indexOf('-machine') + 1]).toBe('q35,accel=tcg');
  });

  it('encodes the accelerator into the -machine value (kvm)', () => {
    const argv = buildArgv(spec(), { accel: 'kvm', qmpSocketPath: SOCK });
    expect(argv[argv.indexOf('-machine') + 1]).toBe('q35,accel=kvm');
  });

  it('is headless and frozen at startup, and wires the QMP unix socket', () => {
    const argv = buildArgv(spec(), { accel: 'tcg', qmpSocketPath: SOCK });
    expect(argv).toContain('-nodefaults');
    expect(argv).toContain('-nographic');
    expect(argv).toContain('-S');
    expect(argv[argv.indexOf('-qmp') + 1]).toBe(`unix:${SOCK},server=on,wait=off`);
  });

  it('is pure: same inputs yield an equal argv', () => {
    const opts = { accel: 'tcg', qmpSocketPath: SOCK } as const;
    expect(buildArgv(spec(), opts)).toEqual(buildArgv(spec(), opts));
  });
});

describe('resolveAccel', () => {
  it('auto chooses KVM when /dev/kvm is available and reports it', () => {
    const r = resolveAccel('auto', () => true);
    expect(r.accel).toBe('kvm');
    expect(r.reason).toMatch(/KVM/);
  });

  it('auto falls back to TCG when /dev/kvm is unavailable and reports it', () => {
    const r = resolveAccel('auto', () => false);
    expect(r.accel).toBe('tcg');
    expect(r.reason).toMatch(/TCG/);
  });

  it('tcg is always TCG, regardless of the probe', () => {
    expect(resolveAccel('tcg', () => true).accel).toBe('tcg');
  });

  it('kvm hard-fails with an actionable error when /dev/kvm is inaccessible', () => {
    expect(() => resolveAccel('kvm', () => false)).toThrow(AccelError);
    expect(() => resolveAccel('kvm', () => false)).toThrowError(/dev\/kvm/);
  });

  it('kvm succeeds when /dev/kvm is accessible', () => {
    expect(resolveAccel('kvm', () => true).accel).toBe('kvm');
  });
});
