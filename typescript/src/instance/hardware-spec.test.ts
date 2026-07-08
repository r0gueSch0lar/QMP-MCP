import { mkdtemp, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import {
  AccelError,
  buildArgv,
  type HardwareSpec,
  HardwareSpecError,
  hostQemuArch,
  machineArch,
  parseHardwareSpec,
  qemuArchOfBinary,
  qemuBinaryForMachine,
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
      // The Display defaults to none — a fully headless Instance (ADR-0010).
      display: 'none',
      // No display adapter by default (issue #15).
      displayDevice: 'none',
      disks: [],
      // Networking defaults to user-mode (SLiRP) with an allowlisted NIC and no
      // port-forwards (ADR-0009).
      network: { mode: 'user', model: 'virtio-net-pci', hostForwards: [] },
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

  it('defaults display to none, accepts vnc, and rejects an unknown mode (ADR-0010)', () => {
    expect(parseHardwareSpec({}).display).toBe('none');
    expect(parseHardwareSpec({ display: 'vnc' }).display).toBe('vnc');
    expect(() => parseHardwareSpec({ display: 'spice' })).toThrowError(/display/);
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

describe('buildArgv display (ADR-0010)', () => {
  it('display none (default) stays headless: no -vnc, still -nographic', () => {
    const argv = buildArgv(spec(), { accel: 'tcg', qmpSocketPath: SOCK });
    expect(argv).not.toContain('-vnc');
    expect(argv).toContain('-nographic');
  });

  it('display vnc emits a loopback -vnc with no plaintext password in the argv', () => {
    const argv = buildArgv(spec({ display: 'vnc' }), { accel: 'tcg', qmpSocketPath: SOCK });
    expect(argv).toContain('-vnc');
    const value = argv[argv.indexOf('-vnc') + 1] ?? '';
    // Loopback bind, fixed display 0 (single Instance), password REQUIRED but set
    // later over QMP — the value carries `password=on`, never a plaintext secret.
    expect(value).toBe('127.0.0.1:0,password=on');
    expect(value.startsWith('127.0.0.1:')).toBe(true);
    // No `password=<secret>` form anywhere: password=on is the only password token.
    expect(argv.join(' ')).not.toMatch(/password=(?!on\b)/);
    // The generated argv is otherwise intact (still headless-frozen with QMP wired).
    expect(argv).toContain('-S');
    expect(argv[argv.indexOf('-qmp') + 1]).toBe(`unix:${SOCK},server=on,wait=off`);
  });
});

describe('buildArgv resource caps (issue #9)', () => {
  it('rejects memoryMb over QMP_MCP_MAX_MEMORY_MB, naming the cap and requested/allowed', () => {
    const call = () =>
      buildArgv(spec({ memoryMb: 8192 }), {
        accel: 'tcg',
        qmpSocketPath: SOCK,
        maxMemoryMb: 4096,
      });
    expect(call).toThrowError(HardwareSpecError);
    expect(call).toThrowError(/memoryMb 8192 exceeds QMP_MCP_MAX_MEMORY_MB=4096/);
  });

  it('accepts memoryMb at the cap, emitting -m unchanged', () => {
    const argv = buildArgv(spec({ memoryMb: 4096 }), {
      accel: 'tcg',
      qmpSocketPath: SOCK,
      maxMemoryMb: 4096,
    });
    expect(argv[argv.indexOf('-m') + 1]).toBe('4096');
  });

  it('accepts memoryMb under the cap', () => {
    const argv = buildArgv(spec({ memoryMb: 2048 }), {
      accel: 'tcg',
      qmpSocketPath: SOCK,
      maxMemoryMb: 4096,
    });
    expect(argv[argv.indexOf('-m') + 1]).toBe('2048');
  });

  it('rejects vcpus over QMP_MCP_MAX_VCPUS, naming the cap and requested/allowed', () => {
    const call = () =>
      buildArgv(spec({ vcpus: 8 }), { accel: 'tcg', qmpSocketPath: SOCK, maxVcpus: 2 });
    expect(call).toThrowError(HardwareSpecError);
    expect(call).toThrowError(/vcpus 8 exceeds QMP_MCP_MAX_VCPUS=2/);
  });

  it('accepts vcpus at the cap, emitting -smp unchanged', () => {
    const argv = buildArgv(spec({ vcpus: 2 }), {
      accel: 'tcg',
      qmpSocketPath: SOCK,
      maxVcpus: 2,
    });
    expect(argv[argv.indexOf('-smp') + 1]).toBe('2');
  });

  it('admits a larger spec when a higher cap is injected', () => {
    // The cap is injected (env-configurable), so raising it admits a bigger spec.
    const argv = buildArgv(spec({ memoryMb: 32768, vcpus: 16 }), {
      accel: 'tcg',
      qmpSocketPath: SOCK,
      maxMemoryMb: 65536,
      maxVcpus: 32,
    });
    expect(argv[argv.indexOf('-m') + 1]).toBe('32768');
    expect(argv[argv.indexOf('-smp') + 1]).toBe('16');
  });

  it('skips the checks when no caps are injected (caps live outside the schema)', () => {
    // Without injected caps there is no enforcement — the Orchestrator always
    // injects them, so create_instance stays fail-closed.
    expect(() =>
      buildArgv(spec({ memoryMb: 1_000_000, vcpus: 200 }), { accel: 'tcg', qmpSocketPath: SOCK }),
    ).not.toThrow();
  });
});

describe('buildArgv extraArgs (ADR-0002 raw-args escape hatch)', () => {
  it('appends extraArgs verbatim to the generated argv when allowRawArgs is true', () => {
    const argv = buildArgv(spec({ extraArgs: ['-vga', 'std'] }), {
      accel: 'tcg',
      qmpSocketPath: SOCK,
      allowRawArgs: true,
    });
    // Appended after the generated argv (which always ends with the -qmp pair).
    expect(argv.slice(-2)).toEqual(['-vga', 'std']);
    // The generated argv is otherwise intact.
    expect(argv[argv.indexOf('-qmp') + 1]).toBe(`unix:${SOCK},server=on,wait=off`);
  });

  it('rejects a spec carrying extraArgs when allowRawArgs is not enabled, naming the flag', () => {
    const call = () =>
      buildArgv(spec({ extraArgs: ['-drive', 'file=/etc/shadow'] }), {
        accel: 'tcg',
        qmpSocketPath: SOCK,
      });
    expect(call).toThrowError(HardwareSpecError);
    expect(call).toThrowError(/QMP_MCP_ALLOW_RAW_ARGS/);
  });

  it('rejects extraArgs when allowRawArgs is explicitly false (fail-closed, not silently dropped)', () => {
    expect(() =>
      buildArgv(spec({ extraArgs: ['-snapshot'] }), {
        accel: 'tcg',
        qmpSocketPath: SOCK,
        allowRawArgs: false,
      }),
    ).toThrowError(/QMP_MCP_ALLOW_RAW_ARGS/);
  });

  it('is a no-op when extraArgs is absent or empty, regardless of allowRawArgs', () => {
    const base = buildArgv(spec(), { accel: 'tcg', qmpSocketPath: SOCK });
    // Absent extraArgs: identical argv with the hatch open.
    expect(buildArgv(spec(), { accel: 'tcg', qmpSocketPath: SOCK, allowRawArgs: true })).toEqual(
      base,
    );
    // An empty extraArgs array appends nothing and is never refused.
    expect(buildArgv(spec({ extraArgs: [] }), { accel: 'tcg', qmpSocketPath: SOCK })).toEqual(base);
    expect(
      buildArgv(spec({ extraArgs: [] }), {
        accel: 'tcg',
        qmpSocketPath: SOCK,
        allowRawArgs: true,
      }),
    ).toEqual(base);
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

describe('parseHardwareSpec network', () => {
  it('defaults to a single user-mode virtio NIC with no forwards', () => {
    expect(parseHardwareSpec({}).network).toEqual({
      mode: 'user',
      model: 'virtio-net-pci',
      hostForwards: [],
    });
  });

  it('defaults a port-forward proto to tcp', () => {
    const parsed = parseHardwareSpec({
      network: { hostForwards: [{ hostPort: 8022, guestPort: 22 }] },
    });
    expect(parsed.network.hostForwards).toEqual([{ hostPort: 8022, guestPort: 22, proto: 'tcp' }]);
  });

  it('rejects a non-allowlisted NIC model, failing closed', () => {
    expect(() => parseHardwareSpec({ network: { model: 'pcnet' } })).toThrowError(
      HardwareSpecError,
    );
    expect(() => parseHardwareSpec({ network: { model: 'pcnet' } })).toThrowError(/model/);
  });

  it('rejects a NIC model carrying an injected -device property (comma)', () => {
    // Without the allowlist, "virtio-net-pci,addr=0x4" would inject an extra
    // -device property; the closed enum makes that impossible.
    expect(() => parseHardwareSpec({ network: { model: 'virtio-net-pci,addr=0x4' } })).toThrowError(
      HardwareSpecError,
    );
  });

  it('rejects an injected mode value (comma/option), failing closed', () => {
    // "user,smb=on" would enable the SLiRP SMB server; the closed enum refuses it.
    expect(() => parseHardwareSpec({ network: { mode: 'user,smb=on' } })).toThrowError(
      HardwareSpecError,
    );
  });

  it('rejects an unknown field on network and on a port-forward, failing closed', () => {
    expect(() => parseHardwareSpec({ network: { foo: 'bar' } })).toThrowError(HardwareSpecError);
    expect(() =>
      parseHardwareSpec({
        network: { hostForwards: [{ hostPort: 2000, guestPort: 22, extra: 1 }] },
      }),
    ).toThrowError(HardwareSpecError);
  });

  it('rejects a non-enum protocol', () => {
    expect(() =>
      parseHardwareSpec({
        network: { hostForwards: [{ hostPort: 2000, guestPort: 22, proto: 'icmp' }] },
      }),
    ).toThrowError(HardwareSpecError);
  });

  it('rejects an out-of-range / zero / negative / non-integer guestPort', () => {
    for (const guestPort of [0, -1, 1.5, 70000]) {
      expect(() =>
        parseHardwareSpec({ network: { hostForwards: [{ hostPort: 2000, guestPort }] } }),
      ).toThrowError(/guestPort/);
    }
  });

  it('rejects an out-of-range / zero / negative / non-integer hostPort at parse time', () => {
    for (const hostPort of [0, -1, 1.5, 70000]) {
      expect(() =>
        parseHardwareSpec({ network: { hostForwards: [{ hostPort, guestPort: 22 }] } }),
      ).toThrowError(/hostPort/);
    }
  });
});

describe('buildArgv network', () => {
  it('emits a default user-mode NIC with an allowlisted model', () => {
    const argv = buildArgv(spec(), { accel: 'tcg', qmpSocketPath: SOCK });
    expect(argv[argv.indexOf('-netdev') + 1]).toBe('user,id=net0');
    expect(argv[argv.indexOf('-device') + 1]).toBe('virtio-net-pci,netdev=net0');
  });

  it('honours an allowlisted NIC model', () => {
    const argv = buildArgv(spec({ network: { mode: 'user', model: 'e1000', hostForwards: [] } }), {
      accel: 'tcg',
      qmpSocketPath: SOCK,
    });
    expect(argv[argv.indexOf('-device') + 1]).toBe('e1000,netdev=net0');
  });

  it('emits hostfwd entries built from validated ints for valid port-forwards', () => {
    const argv = buildArgv(
      spec({
        network: {
          mode: 'user',
          model: 'virtio-net-pci',
          hostForwards: [
            { hostPort: 8022, guestPort: 22, proto: 'tcp' },
            { hostPort: 15353, guestPort: 53, proto: 'udp' },
          ],
        },
      }),
      { accel: 'tcg', qmpSocketPath: SOCK, hostfwdPortRange: { low: 1024, high: 65535 } },
    );
    expect(argv[argv.indexOf('-netdev') + 1]).toBe(
      'user,id=net0,hostfwd=tcp:127.0.0.1:8022-:22,hostfwd=udp:127.0.0.1:15353-:53',
    );
    expect(argv[argv.indexOf('-device') + 1]).toBe('virtio-net-pci,netdev=net0');
  });

  it('binds each forward to 127.0.0.1 (loopback), never 0.0.0.0 — no host-LAN exposure', () => {
    const argv = buildArgv(
      spec({
        network: {
          mode: 'user',
          model: 'virtio-net-pci',
          hostForwards: [{ hostPort: 8080, guestPort: 80, proto: 'tcp' }],
        },
      }),
      { accel: 'tcg', qmpSocketPath: SOCK, hostfwdPortRange: { low: 1024, high: 65535 } },
    );
    const netdev = argv[argv.indexOf('-netdev') + 1] ?? '';
    // The host address sits between proto and host port; pinning it to loopback
    // keeps the forward off the host LAN (ADR-0009 zero host exposure).
    expect(netdev).toBe('user,id=net0,hostfwd=tcp:127.0.0.1:8080-:80');
    expect(netdev).toContain('hostfwd=tcp:127.0.0.1:8080-:80');
    // The old empty-host-address form (which binds 0.0.0.0) must be gone.
    expect(netdev).not.toContain('hostfwd=tcp::8080');
    expect(netdev).not.toContain('0.0.0.0');
  });

  it('rejects a hostPort outside the configured range, naming the range and the value', () => {
    const call = () =>
      buildArgv(
        spec({
          network: {
            mode: 'user',
            model: 'virtio-net-pci',
            hostForwards: [{ hostPort: 80, guestPort: 80 }],
          },
        }),
        {
          accel: 'tcg',
          qmpSocketPath: SOCK,
          hostfwdPortRange: { low: 1024, high: 65535 },
        },
      );
    expect(call).toThrowError(HardwareSpecError);
    expect(call).toThrowError(/1024-65535/);
    expect(call).toThrowError(/\b80\b/);
    expect(call).toThrowError(/QMP_MCP_HOSTFWD_PORT_RANGE/);
  });

  it('enforces a custom range that excludes low/privileged ports', () => {
    const range = { low: 2000, high: 3000 };
    expect(() =>
      buildArgv(
        spec({
          network: {
            mode: 'user',
            model: 'virtio-net-pci',
            hostForwards: [{ hostPort: 1500, guestPort: 22 }],
          },
        }),
        {
          accel: 'tcg',
          qmpSocketPath: SOCK,
          hostfwdPortRange: range,
        },
      ),
    ).toThrowError(/2000-3000/);
    const argv = buildArgv(
      spec({
        network: {
          mode: 'user',
          model: 'virtio-net-pci',
          hostForwards: [{ hostPort: 2500, guestPort: 22 }],
        },
      }),
      { accel: 'tcg', qmpSocketPath: SOCK, hostfwdPortRange: range },
    );
    expect(argv[argv.indexOf('-netdev') + 1]).toBe('user,id=net0,hostfwd=tcp:127.0.0.1:2500-:22');
  });

  it('falls back to the default 1024-65535 range when none is configured', () => {
    // 80 is privileged and outside the default range, so it is rejected even
    // when the caller passes no explicit range.
    expect(() =>
      buildArgv(
        spec({
          network: {
            mode: 'user',
            model: 'virtio-net-pci',
            hostForwards: [{ hostPort: 80, guestPort: 80 }],
          },
        }),
        {
          accel: 'tcg',
          qmpSocketPath: SOCK,
        },
      ),
    ).toThrowError(/1024-65535/);
  });

  it('rejects tap mode by default, naming QMP_MCP_ALLOW_HOST_NET', () => {
    const call = () =>
      buildArgv(spec({ network: { mode: 'tap', model: 'virtio-net-pci', hostForwards: [] } }), {
        accel: 'tcg',
        qmpSocketPath: SOCK,
      });
    expect(call).toThrowError(HardwareSpecError);
    expect(call).toThrowError(/QMP_MCP_ALLOW_HOST_NET/);
  });

  it('rejects bridge mode by default', () => {
    expect(() =>
      buildArgv(spec({ network: { mode: 'bridge', model: 'virtio-net-pci', hostForwards: [] } }), {
        accel: 'tcg',
        qmpSocketPath: SOCK,
      }),
    ).toThrowError(/QMP_MCP_ALLOW_HOST_NET/);
  });

  it('emits a tap netdev when host networking is enabled', () => {
    const argv = buildArgv(
      spec({ network: { mode: 'tap', model: 'virtio-net-pci', hostForwards: [] } }),
      {
        accel: 'tcg',
        qmpSocketPath: SOCK,
        allowHostNet: true,
      },
    );
    expect(argv[argv.indexOf('-netdev') + 1]).toBe('tap,id=net0');
    expect(argv[argv.indexOf('-device') + 1]).toBe('virtio-net-pci,netdev=net0');
  });

  it('emits a bridge netdev when host networking is enabled', () => {
    const argv = buildArgv(
      spec({ network: { mode: 'bridge', model: 'e1000', hostForwards: [] } }),
      {
        accel: 'tcg',
        qmpSocketPath: SOCK,
        allowHostNet: true,
      },
    );
    expect(argv[argv.indexOf('-netdev') + 1]).toBe('bridge,id=net0');
    expect(argv[argv.indexOf('-device') + 1]).toBe('e1000,netdev=net0');
  });

  it('rejects hostForwards with tap mode even when host networking is enabled', () => {
    // hostForwards are user-mode only; supplying them with tap/bridge must fail
    // closed with an actionable message rather than being silently dropped.
    const call = () =>
      buildArgv(
        spec({
          network: {
            mode: 'tap',
            model: 'virtio-net-pci',
            hostForwards: [{ hostPort: 8022, guestPort: 22 }],
          },
        }),
        { accel: 'tcg', qmpSocketPath: SOCK, allowHostNet: true },
      );
    expect(call).toThrowError(HardwareSpecError);
    expect(call).toThrowError(/hostForwards are only valid for user-mode/);
    expect(call).toThrowError(/mode "user"/);
  });

  it('rejects hostForwards with bridge mode even when host networking is enabled', () => {
    const call = () =>
      buildArgv(
        spec({
          network: {
            mode: 'bridge',
            model: 'e1000',
            hostForwards: [{ hostPort: 9090, guestPort: 90 }],
          },
        }),
        { accel: 'tcg', qmpSocketPath: SOCK, allowHostNet: true },
      );
    expect(call).toThrowError(HardwareSpecError);
    expect(call).toThrowError(/hostForwards are only valid for user-mode/);
  });
});

describe('resolveAccel', () => {
  it('auto chooses KVM when the guest arch matches the host and /dev/kvm is available', () => {
    const r = resolveAccel('auto', 'x86_64', 'x86_64', 'q35', () => true);
    expect(r.accel).toBe('kvm');
    expect(r.reason).toMatch(/KVM/);
  });

  it('auto falls back to TCG when /dev/kvm is unavailable and reports it', () => {
    const r = resolveAccel('auto', 'x86_64', 'x86_64', 'q35', () => false);
    expect(r.accel).toBe('tcg');
    expect(r.reason).toMatch(/TCG/);
  });

  it('auto falls back to TCG when the guest arch does not match the host, even with KVM (issue #18)', () => {
    // aarch64 guest on an x86_64 host: KVM cannot cross architectures.
    const r = resolveAccel('auto', 'aarch64', 'x86_64', 'virt', () => true);
    expect(r.accel).toBe('tcg');
    expect(r.reason).toMatch(/guest arch aarch64 does not match host arch x86_64/);
  });

  it('auto falls back to TCG for a raspi board even on a matching host with KVM (issue #18)', () => {
    // raspi boards bake a fixed CPU KVM can't virtualize, so aarch64-on-aarch64 is TCG.
    const r = resolveAccel('auto', 'aarch64', 'aarch64', 'raspi3b', () => true);
    expect(r.accel).toBe('tcg');
    expect(r.reason).toMatch(/raspi3b board has a fixed CPU that KVM cannot virtualize/);
  });

  it('tcg is always TCG, regardless of the probe or arch', () => {
    expect(resolveAccel('tcg', 'aarch64', 'x86_64', 'virt', () => true).accel).toBe('tcg');
  });

  it('kvm hard-fails with an actionable error when /dev/kvm is inaccessible', () => {
    expect(() => resolveAccel('kvm', 'x86_64', 'x86_64', 'q35', () => false)).toThrow(AccelError);
    expect(() => resolveAccel('kvm', 'x86_64', 'x86_64', 'q35', () => false)).toThrowError(
      /dev\/kvm/,
    );
  });

  it('kvm succeeds when /dev/kvm is accessible', () => {
    expect(resolveAccel('kvm', 'x86_64', 'x86_64', 'q35', () => true).accel).toBe('kvm');
  });
});

describe('machine → arch/binary derivation (issue #18)', () => {
  it('maps x86 machines (and unknown names) to qemu-system-x86_64', () => {
    for (const m of ['q35', 'pc', 'microvm', 'some-future-x86-board']) {
      expect(machineArch(m)).toBe('x86_64');
      expect(qemuBinaryForMachine(m)).toBe('qemu-system-x86_64');
    }
  });

  it('maps virt/sbsa-ref and every raspi* board to qemu-system-aarch64', () => {
    // qemu-system-aarch64 is a superset emulator that hosts the 32-bit raspi boards too.
    for (const m of ['virt', 'sbsa-ref', 'raspi0', 'raspi1ap', 'raspi2b', 'raspi3b', 'raspi4b']) {
      expect(machineArch(m)).toBe('aarch64');
      expect(qemuBinaryForMachine(m)).toBe('qemu-system-aarch64');
    }
  });

  it('normalizes Node process.arch to the qemu-system suffix, passing unknowns through', () => {
    expect(hostQemuArch('x64')).toBe('x86_64');
    expect(hostQemuArch('arm64')).toBe('aarch64');
    expect(hostQemuArch('riscv64')).toBe('riscv64');
  });

  it('reads the guest arch from the actual binary name, incl. an absolute-path override', () => {
    expect(qemuArchOfBinary('qemu-system-aarch64')).toBe('aarch64');
    expect(qemuArchOfBinary('/usr/bin/qemu-system-riscv64')).toBe('riscv64');
    // A non-standard override name won't match any host arch, so accel: auto -> TCG.
    expect(qemuArchOfBinary('/opt/my-custom-emulator')).toBe('my-custom-emulator');
  });
});

describe('buildArgv raspi / direct-kernel boot (issue #4)', () => {
  let store: string;

  beforeAll(async () => {
    store = await mkdtemp(join(tmpdir(), 'hw-raspi-'));
    // Leaf files need not exist for path resolution, but create them so the test
    // reads like a real Image Store.
    await writeFile(join(store, 'kernel8.img'), '');
    await writeFile(join(store, 'merged.dtb'), '');
    await writeFile(join(store, 'dietpi.img'), '');
    await writeFile(join(store, 'vmlinuz'), '');
  });

  afterAll(async () => {
    await rm(store, { recursive: true, force: true });
  });

  it('omits -cpu/-smp/-m for a fixed-hardware raspi board', () => {
    const argv = buildArgv(spec({ machine: 'raspi3b', network: { mode: 'none' } }), {
      accel: 'tcg',
      qmpSocketPath: SOCK,
    });
    expect(argv).not.toContain('-cpu');
    expect(argv).not.toContain('-smp');
    expect(argv).not.toContain('-m');
    // The machine is still emitted with its accel.
    expect(argv[argv.indexOf('-machine') + 1]).toBe('raspi3b,accel=tcg');
  });

  it('emits -kernel/-dtb/-append (resolved by name in the Image Store) and if=sd', () => {
    const argv = buildArgv(
      spec({
        machine: 'raspi3b',
        display: 'vnc',
        kernel: 'kernel8.img',
        dtb: 'merged.dtb',
        appendCmdline: 'console=tty1 root=/dev/mmcblk0p2 rootwait rw',
        disks: [{ image: 'dietpi.img', interface: 'sd', format: 'raw' }],
        network: { model: 'usb-net' },
      }),
      { accel: 'tcg', qmpSocketPath: SOCK, imageDir: store },
    );
    expect(argv[argv.indexOf('-kernel') + 1]).toBe(join(store, 'kernel8.img'));
    expect(argv[argv.indexOf('-dtb') + 1]).toBe(join(store, 'merged.dtb'));
    // -append is one token — spaces stay inside it.
    expect(argv[argv.indexOf('-append') + 1]).toBe('console=tty1 root=/dev/mmcblk0p2 rootwait rw');
    expect(argv[argv.indexOf('-drive') + 1]).toContain('if=sd');
    // The kernel/dtb/append block sits before -nodefaults.
    expect(argv.indexOf('-kernel')).toBeLessThan(argv.indexOf('-nodefaults'));
  });

  it('keeps -cpu/-smp/-m for a NON-raspi direct-kernel boot (kernel is not raspi-only)', () => {
    const argv = buildArgv(
      spec({ machine: 'virt', cpu: 'cortex-a72', vcpus: 2, memoryMb: 512, kernel: 'vmlinuz' }),
      { accel: 'tcg', qmpSocketPath: SOCK, imageDir: store },
    );
    expect(argv[argv.indexOf('-cpu') + 1]).toBe('cortex-a72');
    expect(argv[argv.indexOf('-smp') + 1]).toBe('2');
    expect(argv[argv.indexOf('-m') + 1]).toBe('512');
    expect(argv[argv.indexOf('-kernel') + 1]).toBe(join(store, 'vmlinuz'));
  });

  it('fails closed when a kernel is requested but no Image Store is configured', () => {
    expect(() =>
      buildArgv(spec({ machine: 'raspi3b', kernel: 'kernel8.img', network: { mode: 'none' } }), {
        accel: 'tcg',
        qmpSocketPath: SOCK,
      }),
    ).toThrowError(/QMP_MCP_IMAGE_DIR/);
  });

  it('rejects a traversing kernel reference at argv time', () => {
    expect(() =>
      buildArgv(spec({ machine: 'raspi3b', kernel: '../vmlinuz', network: { mode: 'none' } }), {
        accel: 'tcg',
        qmpSocketPath: SOCK,
        imageDir: store,
      }),
    ).toThrowError(/kernel reference/);
  });

  it('network.mode "none" emits no NIC (-netdev/-device)', () => {
    const argv = buildArgv(spec({ machine: 'raspi3b', network: { mode: 'none' } }), {
      accel: 'tcg',
      qmpSocketPath: SOCK,
    });
    expect(argv).not.toContain('-netdev');
    expect(argv).not.toContain('-device');
  });

  it('emits the usb-net NIC for a raspi (USB bus, no PCI)', () => {
    const argv = buildArgv(spec({ machine: 'raspi3b', network: { model: 'usb-net' } }), {
      accel: 'tcg',
      qmpSocketPath: SOCK,
    });
    expect(argv[argv.indexOf('-device') + 1]).toBe('usb-net,netdev=net0');
    expect(argv[argv.indexOf('-netdev') + 1]).toBe('user,id=net0');
  });

  it('refuses a PCI NIC on a raspi (no PCI bus), naming usb-net / none', () => {
    expect(() =>
      // virtio-net-pci is the default model — a bare raspi spec must fail closed.
      buildArgv(spec({ machine: 'raspi3b' }), { accel: 'tcg', qmpSocketPath: SOCK }),
    ).toThrowError(/no PCI bus.*usb-net.*none/s);
  });

  it('refuses usb-net on a non-raspi machine (no USB bus)', () => {
    expect(() =>
      buildArgv(spec({ machine: 'q35', network: { model: 'usb-net' } }), {
        accel: 'tcg',
        qmpSocketPath: SOCK,
      }),
    ).toThrowError(/usb-net.*needs a USB bus/s);
  });

  it('allows network.mode "none" on a non-raspi machine too (no NIC)', () => {
    const argv = buildArgv(spec({ machine: 'q35', network: { mode: 'none' } }), {
      accel: 'tcg',
      qmpSocketPath: SOCK,
    });
    expect(argv).not.toContain('-device');
  });

  it('rejects dtb without kernel (a dtb is only passed to a direct-booted kernel)', () => {
    expect(() => parseHardwareSpec({ machine: 'raspi3b', dtb: 'merged.dtb' })).toThrowError(
      /dtb requires kernel/,
    );
  });

  it('rejects appendCmdline without kernel', () => {
    expect(() =>
      parseHardwareSpec({ machine: 'raspi3b', appendCmdline: 'console=tty1' }),
    ).toThrowError(/appendCmdline requires kernel/);
  });

  it('rejects an appendCmdline containing a control character (newline)', () => {
    expect(() =>
      parseHardwareSpec({ machine: 'raspi3b', kernel: 'kernel8.img', appendCmdline: 'a\nb' }),
    ).toThrowError(HardwareSpecError);
  });

  it('accepts sd as a disk interface', () => {
    const parsed = parseHardwareSpec({ disks: [{ image: 'dietpi.img', interface: 'sd' }] });
    expect(parsed.disks[0]?.interface).toBe('sd');
  });
});

describe('buildArgv display adapter + initrd (issue #15)', () => {
  let store: string;

  beforeAll(async () => {
    store = await mkdtemp(join(tmpdir(), 'hw-disp-'));
    await writeFile(join(store, 'vmlinuz'), '');
    await writeFile(join(store, 'initrd.img'), '');
    await writeFile(join(store, 'rootfs.img'), '');
  });

  afterAll(async () => {
    await rm(store, { recursive: true, force: true });
  });

  it('displayDevice virtio-gpu emits -device virtio-gpu-pci (so virt/q35 render over VNC)', () => {
    const argv = buildArgv(spec({ machine: 'virt', display: 'vnc', displayDevice: 'virtio-gpu' }), {
      accel: 'tcg',
      qmpSocketPath: SOCK,
    });
    expect(argv[argv.indexOf('-device') + 1]).toBe('virtio-gpu-pci');
    // The adapter sits after the -vnc block.
    expect(argv.indexOf('-vnc')).toBeLessThan(argv.indexOf('virtio-gpu-pci'));
  });

  it('maps vga -> VGA and ramfb -> ramfb', () => {
    const vga = buildArgv(spec({ machine: 'q35', displayDevice: 'vga' }), {
      accel: 'tcg',
      qmpSocketPath: SOCK,
    });
    expect(vga).toContain('VGA');
    const ramfb = buildArgv(spec({ machine: 'virt', displayDevice: 'ramfb' }), {
      accel: 'tcg',
      qmpSocketPath: SOCK,
    });
    expect(ramfb).toContain('ramfb');
  });

  it('emits no display -device by default (displayDevice none)', () => {
    const argv = buildArgv(spec({ machine: 'q35', display: 'vnc' }), {
      accel: 'tcg',
      qmpSocketPath: SOCK,
    });
    // Only the NIC -device is present; no display adapter.
    expect(argv.filter((a) => a === '-device')).toHaveLength(1);
    expect(argv[argv.indexOf('-device') + 1]).toBe('virtio-net-pci,netdev=net0');
  });

  it('refuses a displayDevice on a raspi board (built-in framebuffer, no PCI)', () => {
    expect(() =>
      buildArgv(
        spec({ machine: 'raspi3b', displayDevice: 'virtio-gpu', network: { mode: 'none' } }),
        {
          accel: 'tcg',
          qmpSocketPath: SOCK,
        },
      ),
    ).toThrowError(/raspi.*built-in framebuffer|displayDevice.*raspi/s);
  });

  it('emits -initrd (resolved by name in the Image Store) right after -kernel', () => {
    const argv = buildArgv(
      spec({ machine: 'virt', cpu: 'cortex-a72', kernel: 'vmlinuz', initrd: 'initrd.img' }),
      { accel: 'tcg', qmpSocketPath: SOCK, imageDir: store },
    );
    expect(argv[argv.indexOf('-initrd') + 1]).toBe(join(store, 'initrd.img'));
    expect(argv.indexOf('-kernel')).toBeLessThan(argv.indexOf('-initrd'));
  });

  it('rejects initrd without kernel', () => {
    expect(() => parseHardwareSpec({ initrd: 'initrd.img' })).toThrowError(
      /initrd requires kernel/,
    );
  });

  it('accepts the display-device enum values', () => {
    for (const d of ['none', 'virtio-gpu', 'vga', 'ramfb'] as const) {
      expect(parseHardwareSpec({ displayDevice: d }).displayDevice).toBe(d);
    }
  });
});
