import { mkdtemp, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
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
      disks: [],
    });
  });

  it('defaults a disk entry (interface=virtio, format=qcow2, readonly=false)', () => {
    const parsed = parseHardwareSpec({ disks: [{ image: 'root.qcow2' }] });
    expect(parsed.disks).toEqual([
      { image: 'root.qcow2', interface: 'virtio', format: 'qcow2', readonly: false },
    ]);
  });

  it('rejects an unknown field inside a disk, failing closed', () => {
    expect(() => parseHardwareSpec({ disks: [{ image: 'd', path: '/x' }] })).toThrow(
      HardwareSpecError,
    );
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

  it('rejects a machine value with an injected QemuOpts property (comma)', () => {
    // "q35,accel=tcg" would override accel and inject -machine properties.
    expect(() => parseHardwareSpec({ machine: 'q35,accel=tcg' })).toThrowError(HardwareSpecError);
    expect(() => parseHardwareSpec({ machine: 'q35,accel=tcg' })).toThrowError(/machine/);
  });

  it('rejects a cpu value with an injected property (comma) and accepts a plain model', () => {
    expect(() => parseHardwareSpec({ cpu: 'host,+vmx' })).toThrowError(/cpu/);
    expect(() => parseHardwareSpec({ machine: 'q35', cpu: 'max' })).not.toThrow();
    expect(parseHardwareSpec({ machine: 'q35', cpu: 'max' }).machine).toBe('q35');
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

describe('buildArgv disks', () => {
  let store: string;

  beforeAll(async () => {
    store = await mkdtemp(join(tmpdir(), 'hw-disks-'));
    await writeFile(join(store, 'root.qcow2'), '');
  });

  afterAll(async () => {
    await rm(store, { recursive: true, force: true });
  });

  it('emits a -drive with an explicit format for a valid in-store disk', () => {
    const argv = buildArgv(spec({ disks: [{ image: 'root.qcow2' }] }), {
      accel: 'tcg',
      qmpSocketPath: SOCK,
      imageDir: store,
    });
    const drive = argv[argv.indexOf('-drive') + 1] ?? '';
    expect(drive).toContain(`file=${join(store, 'root.qcow2')}`);
    // Format is pinned explicitly — never left to QEMU's auto-probing.
    expect(drive).toContain('format=qcow2');
    expect(drive).toContain('if=virtio');
    expect(drive).not.toContain('readonly=on');
  });

  it('marks a readonly disk and honours the interface', () => {
    const argv = buildArgv(
      spec({ disks: [{ image: 'root.qcow2', interface: 'ide', readonly: true }] }),
      { accel: 'tcg', qmpSocketPath: SOCK, imageDir: store },
    );
    const drive = argv[argv.indexOf('-drive') + 1] ?? '';
    expect(drive).toContain('if=ide');
    expect(drive).toContain('readonly=on');
  });

  it('rejects an absolute disk reference at argv time', () => {
    expect(() =>
      buildArgv(spec({ disks: [{ image: '/etc/passwd' }] }), {
        accel: 'tcg',
        qmpSocketPath: SOCK,
        imageDir: store,
      }),
    ).toThrow(HardwareSpecError);
  });

  it('rejects a `..` traversal disk reference at argv time', () => {
    expect(() =>
      buildArgv(spec({ disks: [{ image: '../escape.qcow2' }] }), {
        accel: 'tcg',
        qmpSocketPath: SOCK,
        imageDir: store,
      }),
    ).toThrowError(/separator|valid file name/);
  });

  it('rejects an out-of-store (non-existent name) reference only via containment, and fails closed without an Image Store dir', () => {
    expect(() =>
      buildArgv(spec({ disks: [{ image: 'root.qcow2' }] }), {
        accel: 'tcg',
        qmpSocketPath: SOCK,
      }),
    ).toThrowError(/QMP_MCP_IMAGE_DIR/);
  });

  it('rejects a symlink that escapes the Store at argv time', async () => {
    await symlink('/etc/passwd', join(store, 'evil.qcow2'));
    expect(() =>
      buildArgv(spec({ disks: [{ image: 'evil.qcow2' }] }), {
        accel: 'tcg',
        qmpSocketPath: SOCK,
        imageDir: store,
      }),
    ).toThrowError(/symlink escape/);
  });
});

describe('buildArgv -drive option injection', () => {
  // The Store dir path deliberately contains a comma so we exercise the
  // comma-escaping of the host-derived file path (defense in depth).
  let store: string;

  beforeAll(async () => {
    store = await mkdtemp(join(tmpdir(), 'hw,disks-'));
    await writeFile(join(store, 'root.qcow2'), '');
  });

  afterAll(async () => {
    await rm(store, { recursive: true, force: true });
  });

  it('rejects a disk name carrying an extra property, so -drive cannot gain one', () => {
    // Without the allowlist, "root.qcow2,readonly=on" would inject readonly=on.
    expect(() =>
      buildArgv(spec({ disks: [{ image: 'root.qcow2,readonly=on' }] }), {
        accel: 'tcg',
        qmpSocketPath: SOCK,
        imageDir: store,
      }),
    ).toThrowError(HardwareSpecError);
  });

  it('comma-escapes the resolved file path so the path cannot split into extra props', () => {
    const path = join(store, 'root.qcow2');
    expect(path).toContain(','); // sanity: the Store path really has a comma
    const argv = buildArgv(spec({ disks: [{ image: 'root.qcow2' }] }), {
      accel: 'tcg',
      qmpSocketPath: SOCK,
      imageDir: store,
    });
    const drive = argv[argv.indexOf('-drive') + 1] ?? '';
    // The literal comma in the path is doubled; the property list is exactly the
    // four intended properties — no extra property is introduced by the path.
    expect(drive).toBe(`file=${path.replaceAll(',', ',,')},format=qcow2,if=virtio,media=disk`);
    // A QemuOpts parser splits on a *single* comma (a doubled comma is a literal
    // comma); doing so must yield exactly the four intended key=value tokens.
    expect(drive.replaceAll(',,', ' ').split(',')).toHaveLength(4);
  });

  it('comma-escapes the -machine value so machine cannot gain a property', () => {
    // machine itself is allowlisted, but verify the interpolation is escaped too:
    // a hypothetical comma in the value is doubled rather than splitting.
    const argv = buildArgv(spec({ machine: 'q35' }), { accel: 'tcg', qmpSocketPath: SOCK });
    expect(argv[argv.indexOf('-machine') + 1]).toBe('q35,accel=tcg');
  });
});

describe('parseHardwareSpec cdrom & boot', () => {
  it('leaves cdrom and boot absent for an empty spec', () => {
    const parsed = parseHardwareSpec({});
    expect(parsed.cdrom).toBeUndefined();
    expect(parsed.boot).toBeUndefined();
  });

  it('accepts a cdrom referencing an ISO by name', () => {
    expect(parseHardwareSpec({ cdrom: { iso: 'debian.iso' } }).cdrom).toEqual({
      iso: 'debian.iso',
    });
  });

  it('rejects an unknown field inside cdrom, failing closed', () => {
    expect(() => parseHardwareSpec({ cdrom: { iso: 'd.iso', file: '/x' } })).toThrow(
      HardwareSpecError,
    );
  });

  it('accepts a valid boot order of drive letters', () => {
    expect(parseHardwareSpec({ boot: 'd' }).boot).toBe('d');
    expect(parseHardwareSpec({ boot: 'dc' }).boot).toBe('dc');
    expect(parseHardwareSpec({ boot: 'cdn' }).boot).toBe('cdn');
  });

  it('rejects a boot value carrying an extra -boot option (comma/space/=)', () => {
    expect(() => parseHardwareSpec({ boot: 'c,menu=on' })).toThrowError(/boot/);
    expect(() => parseHardwareSpec({ boot: 'd order=c' })).toThrowError(/boot/);
    expect(() => parseHardwareSpec({ boot: 'c,reboot-timeout=-1' })).toThrowError(
      HardwareSpecError,
    );
  });

  it('rejects boot drive letters outside the allowlist', () => {
    expect(() => parseHardwareSpec({ boot: 'z' })).toThrowError(/boot/);
    expect(() => parseHardwareSpec({ boot: '' })).toThrowError(/boot/);
  });
});

describe('buildArgv cdrom (read-only ISO Store)', () => {
  let isoDir: string;

  beforeAll(async () => {
    isoDir = await mkdtemp(join(tmpdir(), 'hw-iso-'));
    await writeFile(join(isoDir, 'debian.iso'), '');
  });

  afterAll(async () => {
    await rm(isoDir, { recursive: true, force: true });
  });

  it('emits a read-only cdrom -drive with an explicit raw format', () => {
    const argv = buildArgv(spec({ cdrom: { iso: 'debian.iso' } }), {
      accel: 'tcg',
      qmpSocketPath: SOCK,
      isoDir,
    });
    const drive = argv[argv.indexOf('-drive') + 1] ?? '';
    expect(drive).toContain(`file=${join(isoDir, 'debian.iso')}`);
    expect(drive).toContain('media=cdrom');
    expect(drive).toContain('readonly=on');
    // ISO format is pinned explicitly — never left to QEMU's auto-probing.
    expect(drive).toContain('format=raw');
  });

  it('fails closed naming QMP_MCP_ISO_DIR when no ISO Store is configured', () => {
    expect(() =>
      buildArgv(spec({ cdrom: { iso: 'debian.iso' } }), { accel: 'tcg', qmpSocketPath: SOCK }),
    ).toThrowError(/QMP_MCP_ISO_DIR/);
  });

  it('rejects an absolute / `..` / out-of-store ISO reference at argv time', () => {
    expect(() =>
      buildArgv(spec({ cdrom: { iso: '/etc/passwd' } }), {
        accel: 'tcg',
        qmpSocketPath: SOCK,
        isoDir,
      }),
    ).toThrowError(HardwareSpecError);
    expect(() =>
      buildArgv(spec({ cdrom: { iso: '../escape.iso' } }), {
        accel: 'tcg',
        qmpSocketPath: SOCK,
        isoDir,
      }),
    ).toThrowError(/separator|valid file name/);
  });

  it('rejects an ISO name carrying an extra -drive property (comma)', () => {
    // The cdrom.iso is validated by the same allowlist as disks; "x.iso,media=disk"
    // could otherwise turn the read-only cdrom into a writable disk.
    expect(() =>
      buildArgv(spec({ cdrom: { iso: 'debian.iso,media=disk' } }), {
        accel: 'tcg',
        qmpSocketPath: SOCK,
        isoDir,
      }),
    ).toThrowError(HardwareSpecError);
  });
});

describe('buildArgv boot order', () => {
  it('emits a valid boot order as a single -boot order= token', () => {
    const argv = buildArgv(spec({ boot: 'dc' }), { accel: 'tcg', qmpSocketPath: SOCK });
    expect(argv).toContain('-boot');
    expect(argv[argv.indexOf('-boot') + 1]).toBe('order=dc');
  });

  it('omits -boot entirely when no boot order is requested', () => {
    const argv = buildArgv(spec(), { accel: 'tcg', qmpSocketPath: SOCK });
    expect(argv).not.toContain('-boot');
  });

  it('an injected boot value never reaches argv (rejected at parse)', () => {
    // The schema is the choke point: a comma/extra-option value cannot be turned
    // into a HardwareSpec, so buildArgv never sees it.
    expect(() => spec({ boot: 'c,menu=on' })).toThrowError(HardwareSpecError);
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
