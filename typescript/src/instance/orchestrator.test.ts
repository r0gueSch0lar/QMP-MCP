import { mkdtemp, rm, stat, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';
import { buildPolicy, CommandPolicyError } from '../policy/command-policy.js';
import { FakeQemuDriver } from '../qemu/fake-driver.js';
import { HardwareSpecError } from './hardware-spec.js';
import {
  defaultSocketOccupied,
  LifecycleError,
  Orchestrator,
  type OrchestratorOptions,
} from './orchestrator.js';

/** Yield once so queued microtasks (e.g. the process-exit reconciliation) run. */
const tick = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));

const SOCK = '/run/qmp-mcp/test.sock';

function makeOrchestrator(
  driver: FakeQemuDriver,
  options: Partial<OrchestratorOptions> = {},
): Orchestrator {
  return new Orchestrator(driver, {
    qemuBinaryOverride: 'qemu-system-x86_64',
    // Pin the host arch so the accel=auto guest/host match is deterministic across
    // CI runners (issue #18); the default q35 machine is x86_64, so guest == host.
    hostArch: 'x86_64',
    qmpSocketPath: SOCK,
    kvmAvailable: () => false,
    socketOccupied: async () => false,
    ...options,
  });
}

describe('Orchestrator lifecycle (fake driver)', () => {
  it('starts in NONE', () => {
    expect(makeOrchestrator(new FakeQemuDriver()).getInstance().state).toBe('NONE');
  });

  it('create loads the Instance PAUSED by default (frozen at -S) and launches via the driver port (issue #10)', async () => {
    const driver = new FakeQemuDriver();
    const orch = makeOrchestrator(driver);

    const result = await orch.createInstance({ machine: 'q35', memoryMb: 128 });

    // Default (no auto-start): the Guest is loaded but frozen at -S, so the honest
    // lifecycle state is PAUSED — and get_status agrees (query-status: paused).
    expect(result.state).toBe('PAUSED');
    expect(orch.getInstance().state).toBe('PAUSED');
    expect(await orch.getStatus()).toMatchObject({ running: false });
    expect(driver.launches).toHaveLength(1);
    expect(driver.launches[0]?.binary).toBe('qemu-system-x86_64');
    expect(driver.launches[0]?.qmpSocketPath).toBe(SOCK);
    // The driver was handed a generated argv that wires the QMP socket.
    expect(driver.launches[0]?.argv).toContain('-qmp');
  });

  it('does NOT issue cont on create by default — the Guest stays paused (issue #8)', async () => {
    const driver = new FakeQemuDriver();
    const orch = makeOrchestrator(driver);
    await orch.createInstance({});
    expect(driver.lastProcess?.executed.some((e) => e.command === 'cont')).toBe(false);
  });

  it('auto-starts the Guest with a QMP cont on create when autoStart is set (issues #8, #10)', async () => {
    const driver = new FakeQemuDriver();
    const orch = makeOrchestrator(driver, { autoStart: true });
    const result = await orch.createInstance({});
    expect(result.state).toBe('RUNNING');
    expect(orch.getInstance().state).toBe('RUNNING');
    expect(driver.lastProcess?.executed.some((e) => e.command === 'cont')).toBe(true);
    // get_status agrees: the Guest is actually executing.
    expect(await orch.getStatus()).toMatchObject({ running: true });
  });

  it('reports the chosen accelerator (auto -> tcg when no KVM)', async () => {
    const orch = makeOrchestrator(new FakeQemuDriver(), { kvmAvailable: () => false });
    const result = await orch.createInstance({});
    expect(result.accel).toBe('tcg');
    expect(result.accelReason).toMatch(/TCG/);
    expect(orch.getInstance().accel).toBe('tcg');
  });

  it('reports the chosen accelerator (auto -> kvm when KVM available)', async () => {
    const driver = new FakeQemuDriver();
    const orch = makeOrchestrator(driver, { kvmAvailable: () => true });
    const result = await orch.createInstance({});
    expect(result.accel).toBe('kvm');
    expect(driver.launches[0]?.argv[driver.launches[0].argv.indexOf('-machine') + 1]).toMatch(
      /accel=kvm/,
    );
  });

  it('derives the qemu binary from the machine when no override is set (issue #18)', async () => {
    const driver = new FakeQemuDriver();
    const orch = makeOrchestrator(driver, { qemuBinaryOverride: undefined });
    await orch.createInstance({ machine: 'virt', accel: 'tcg' });
    expect(driver.launches[0]?.binary).toBe('qemu-system-aarch64');
  });

  it('an explicit binary override wins over machine derivation (issue #18)', async () => {
    const driver = new FakeQemuDriver();
    // Override set to x86_64 by makeOrchestrator; a virt (aarch64) machine must NOT
    // flip the binary — the operator's explicit choice is honored for every Instance.
    const orch = makeOrchestrator(driver);
    await orch.createInstance({ machine: 'virt', accel: 'tcg' });
    expect(driver.launches[0]?.binary).toBe('qemu-system-x86_64');
  });

  it('accel=auto falls back to TCG when the guest arch does not match the host (issue #18)', async () => {
    // aarch64 `virt` guest on an x86_64 host: KVM cannot cross architectures, so even
    // with /dev/kvm available the accelerator must resolve to TCG (not "invalid kvm").
    const orch = makeOrchestrator(new FakeQemuDriver(), {
      qemuBinaryOverride: undefined,
      kvmAvailable: () => true,
    });
    const result = await orch.createInstance({ machine: 'virt' });
    expect(result.accel).toBe('tcg');
    expect(result.accelReason).toMatch(/does not match host arch/);
  });

  it('accel=auto keys off the OVERRIDE binary arch, not the machine (issue #18)', async () => {
    // machine q35 -> machineArch x86_64 (== host), but the operator overrode the binary
    // to an aarch64 emulator: KVM must NOT be chosen (the launched binary is aarch64).
    const orch = makeOrchestrator(new FakeQemuDriver(), {
      qemuBinaryOverride: 'qemu-system-aarch64',
      kvmAvailable: () => true,
    });
    const result = await orch.createInstance({ machine: 'q35' });
    expect(result.accel).toBe('tcg');
    expect(result.accelReason).toMatch(/guest arch aarch64 does not match host arch x86_64/);
  });

  it('rejects create while an Instance already exists, actionably', async () => {
    const orch = makeOrchestrator(new FakeQemuDriver());
    await orch.createInstance({});
    await expect(orch.createInstance({})).rejects.toBeInstanceOf(LifecycleError);
    await expect(orch.createInstance({})).rejects.toThrow(/destroy_instance/);
    // Still exactly one Instance (PAUSED by default — issue #10).
    expect(orch.getInstance().state).toBe('PAUSED');
  });

  it('refuses to start when the QMP socket path is occupied, actionably', async () => {
    const orch = makeOrchestrator(new FakeQemuDriver(), { socketOccupied: async () => true });
    await expect(orch.createInstance({})).rejects.toThrow(/already occupied/);
    expect(orch.getInstance().state).toBe('NONE');
  });

  it('rejects an invalid Hardware Spec before launching', async () => {
    const driver = new FakeQemuDriver();
    const orch = makeOrchestrator(driver);
    await expect(orch.createInstance({ vcpus: 0 })).rejects.toBeInstanceOf(HardwareSpecError);
    expect(driver.launches).toHaveLength(0);
    expect(orch.getInstance().state).toBe('NONE');
  });

  it('rejects an over-cap spec before launching, naming the cap (issue #9)', async () => {
    const driver = new FakeQemuDriver();
    const orch = makeOrchestrator(driver, { maxMemoryMb: 4096, maxVcpus: 2 });
    await expect(orch.createInstance({ memoryMb: 8192 })).rejects.toThrowError(
      /memoryMb 8192 exceeds QMP_MCP_MAX_MEMORY_MB=4096/,
    );
    await expect(orch.createInstance({ vcpus: 4 })).rejects.toThrowError(
      /vcpus 4 exceeds QMP_MCP_MAX_VCPUS=2/,
    );
    // Fail-closed: no qemu was launched and the Instance stays NONE.
    expect(driver.launches).toHaveLength(0);
    expect(orch.getInstance().state).toBe('NONE');
  });

  it('get_status returns the live QMP query-status result', async () => {
    const driver = new FakeQemuDriver({
      responses: { 'query-status': { status: 'prelaunch', running: false } },
    });
    const orch = makeOrchestrator(driver);
    await orch.createInstance({});
    expect(await orch.getStatus()).toEqual({ status: 'prelaunch', running: false });
  });

  it('destroy terminates the process and returns to NONE', async () => {
    const driver = new FakeQemuDriver();
    const orch = makeOrchestrator(driver);
    await orch.createInstance({});
    const process = driver.lastProcess;

    const result = await orch.destroyInstance();

    expect(result.state).toBe('NONE');
    expect(orch.getInstance().state).toBe('NONE');
    expect(process?.closed).toBe(true);
  });

  it('allows create again after destroy', async () => {
    const driver = new FakeQemuDriver();
    const orch = makeOrchestrator(driver);
    await orch.createInstance({});
    await orch.destroyInstance();
    await expect(orch.createInstance({})).resolves.toMatchObject({ state: 'PAUSED' });
    expect(driver.launches).toHaveLength(2);
  });

  it('rejects destroy and get_status when no Instance is running', async () => {
    const orch = makeOrchestrator(new FakeQemuDriver());
    await expect(orch.destroyInstance()).rejects.toBeInstanceOf(LifecycleError);
    await expect(orch.getStatus()).rejects.toBeInstanceOf(LifecycleError);
  });

  it('surfaces a driver launch failure as a LifecycleError and stays NONE', async () => {
    const driver = new FakeQemuDriver({ launchError: new Error('boom') });
    const orch = makeOrchestrator(driver);
    await expect(orch.createInstance({})).rejects.toBeInstanceOf(LifecycleError);
    expect(orch.getInstance().state).toBe('NONE');
  });

  it('reconciles to NONE when the process exits unexpectedly, and allows a fresh create', async () => {
    const driver = new FakeQemuDriver();
    const orch = makeOrchestrator(driver);
    await orch.createInstance({});
    expect(orch.getInstance().state).toBe('PAUSED');

    // qemu vanishes on its own (crash/SIGKILL) without an explicit destroy.
    driver.lastProcess?.simulateExit();
    await tick(); // let `exited.then(onProcessExit)` run

    expect(orch.getInstance().state).toBe('NONE');
    // The crashed Instance's handle was released, so a new create is accepted.
    await expect(orch.createInstance({})).resolves.toMatchObject({ state: 'PAUSED' });
    expect(driver.launches).toHaveLength(2);
  });

  it('reserves the slot synchronously: concurrent creates launch exactly one qemu', async () => {
    const driver = new FakeQemuDriver();
    const orch = makeOrchestrator(driver);
    // Fire two creates without awaiting between them; the second must lose.
    const [a, b] = await Promise.allSettled([orch.createInstance({}), orch.createInstance({})]);
    const fulfilled = [a, b].filter((r) => r.status === 'fulfilled');
    const rejected = [a, b].filter((r) => r.status === 'rejected');
    expect(fulfilled).toHaveLength(1);
    expect(rejected).toHaveLength(1);
    expect((rejected[0] as PromiseRejectedResult).reason).toBeInstanceOf(LifecycleError);
    // Crucially: only one qemu was ever launched.
    expect(driver.launches).toHaveLength(1);
    expect(orch.getInstance().state).toBe('PAUSED');
  });
});

describe('Orchestrator vnc Display + Viewer (fake driver, ADR-0010)', () => {
  /** A recording fake Viewer + its factory, standing in for the real in-process one. */
  interface FakeViewer {
    host: string;
    port: number;
    stopped: boolean;
    options: {
      host: string;
      port: number;
      password: string;
      vncHost: string;
      vncPort: number;
      vncPassword: string;
    };
    stop(): Promise<void>;
  }

  function fakeViewerFactory(): {
    factory: (options: FakeViewer['options']) => Promise<FakeViewer>;
    viewers: FakeViewer[];
  } {
    const viewers: FakeViewer[] = [];
    const factory = async (options: FakeViewer['options']): Promise<FakeViewer> => {
      const viewer: FakeViewer = {
        host: options.host,
        port: options.port,
        stopped: false,
        options,
        stop: async () => {
          viewer.stopped = true;
        },
      };
      viewers.push(viewer);
      return viewer;
    };
    return { factory, viewers };
  }

  it('create(display:vnc) arms the VNC password over QMP and starts the Viewer; destroy stops it', async () => {
    const driver = new FakeQemuDriver();
    const { factory, viewers } = fakeViewerFactory();
    const orch = makeOrchestrator(driver, {
      viewerPassword: 'human-secret',
      viewerHost: '127.0.0.1',
      viewerPort: 6080,
      startViewer: factory,
    });

    await orch.createInstance({ display: 'vnc' });

    // The Display password was set over QMP (protocol vnc), NOT placed in argv.
    const setPassword = driver.lastProcess?.executed.find((e) => e.command === 'set_password');
    expect(setPassword?.args?.protocol).toBe('vnc');
    const vncPassword = setPassword?.args?.password;
    expect(typeof vncPassword).toBe('string');
    expect((vncPassword as string).length).toBeGreaterThan(0);

    // The generated argv carries a loopback -vnc with no plaintext password.
    const argv = driver.launches[0]?.argv ?? [];
    expect(argv[argv.indexOf('-vnc') + 1]).toBe('127.0.0.1:0,password=on');
    expect(argv.join(' ')).not.toContain(vncPassword as string);

    // Exactly one Viewer was started, dialing the server-controlled loopback VNC
    // port, gated by the human password, and embedding the SAME vnc password.
    expect(viewers).toHaveLength(1);
    expect(viewers[0]?.options.vncHost).toBe('127.0.0.1');
    expect(viewers[0]?.options.vncPort).toBe(5900);
    expect(viewers[0]?.options.password).toBe('human-secret');
    expect(viewers[0]?.options.vncPassword).toBe(vncPassword);
    // The vnc Instance loads PAUSED by default like any other (issue #10).
    expect(orch.getInstance().state).toBe('PAUSED');

    await orch.destroyInstance();
    // The Viewer's lifetime equals the Instance's: destroy stops it.
    expect(viewers[0]?.stopped).toBe(true);
    expect(orch.getInstance().state).toBe('NONE');
  });

  it('rejects create(display:vnc) when QMP_MCP_VIEWER_PASSWORD is unset, before launching', async () => {
    const driver = new FakeQemuDriver();
    const { factory, viewers } = fakeViewerFactory();
    // No viewerPassword injected — the fail-closed coupling must refuse.
    const orch = makeOrchestrator(driver, { startViewer: factory });

    await expect(orch.createInstance({ display: 'vnc' })).rejects.toBeInstanceOf(LifecycleError);
    await expect(orch.createInstance({ display: 'vnc' })).rejects.toThrow(
      /QMP_MCP_VIEWER_PASSWORD/,
    );

    // Fail-closed: no qemu was launched and no Viewer was started.
    expect(driver.launches).toHaveLength(0);
    expect(viewers).toHaveLength(0);
    expect(orch.getInstance().state).toBe('NONE');
  });

  it('does not start a Viewer for a headless (display:none) Instance', async () => {
    const driver = new FakeQemuDriver();
    const { factory, viewers } = fakeViewerFactory();
    const orch = makeOrchestrator(driver, { viewerPassword: 'human-secret', startViewer: factory });

    await orch.createInstance({});

    expect(viewers).toHaveLength(0);
    // No set_password is issued when there is no vnc Display.
    expect(driver.lastProcess?.executed.some((e) => e.command === 'set_password')).toBe(false);
  });

  it('stops the Viewer when qemu exits unexpectedly', async () => {
    const driver = new FakeQemuDriver();
    const { factory, viewers } = fakeViewerFactory();
    const orch = makeOrchestrator(driver, { viewerPassword: 'human-secret', startViewer: factory });

    await orch.createInstance({ display: 'vnc' });
    expect(viewers).toHaveLength(1);

    driver.lastProcess?.simulateExit();
    await tick();

    expect(orch.getInstance().state).toBe('NONE');
    expect(viewers[0]?.stopped).toBe(true);
  });
});

describe('Orchestrator control commands (fake driver)', () => {
  /** Create an Instance and hand back the orchestrator + its fake process. */
  async function running(options: Partial<OrchestratorOptions> = {}, driverOptions = {}) {
    const driver = new FakeQemuDriver(driverOptions);
    const orch = makeOrchestrator(driver, { autoStart: true, ...options });
    await orch.createInstance({});
    // biome-ignore lint/style/noNonNullAssertion: a process exists right after create.
    const process = driver.lastProcess!;
    return { orch, process };
  }

  /** The QMP command names issued on the fake process, in order. */
  const commands = (process: { executed: Array<{ command: string }> }): string[] =>
    process.executed.map((e) => e.command);

  it('pause issues QMP stop and moves RUNNING -> PAUSED (reflected by get_status)', async () => {
    const { orch, process } = await running();
    expect(orch.getInstance().state).toBe('RUNNING');

    const result = await orch.pauseInstance();

    expect(result).toEqual({ state: 'PAUSED' });
    expect(orch.getInstance().state).toBe('PAUSED');
    expect(commands(process)).toContain('stop');
    // get_status (live query-status) reflects the pause.
    expect(await orch.getStatus()).toMatchObject({ status: 'paused', running: false });
  });

  it('resume issues QMP cont and moves PAUSED -> RUNNING (reflected by get_status)', async () => {
    const { orch, process } = await running();
    await orch.pauseInstance();

    const result = await orch.resumeInstance();

    expect(result).toEqual({ state: 'RUNNING' });
    expect(orch.getInstance().state).toBe('RUNNING');
    expect(commands(process)).toEqual(expect.arrayContaining(['stop', 'cont']));
    expect(await orch.getStatus()).toMatchObject({ status: 'running', running: true });
  });

  it('pause is idempotent: pausing an already-PAUSED Instance stays PAUSED', async () => {
    const { orch } = await running();
    await orch.pauseInstance();
    await expect(orch.pauseInstance()).resolves.toEqual({ state: 'PAUSED' });
    expect(orch.getInstance().state).toBe('PAUSED');
  });

  it('resume is idempotent: resuming a RUNNING Instance stays RUNNING', async () => {
    const { orch } = await running();
    await expect(orch.resumeInstance()).resolves.toEqual({ state: 'RUNNING' });
    expect(orch.getInstance().state).toBe('RUNNING');
  });

  it('reset issues QMP system_reset and leaves the lifecycle state unchanged', async () => {
    const { orch, process } = await running();
    const result = await orch.resetInstance();
    expect(result).toEqual({ state: 'RUNNING' });
    expect(orch.getInstance().state).toBe('RUNNING');
    expect(commands(process)).toContain('system_reset');
  });

  it('powerdown issues QMP system_powerdown and leaves the lifecycle state unchanged', async () => {
    const { orch, process } = await running();
    const result = await orch.powerdownInstance();
    expect(result).toEqual({ state: 'RUNNING' });
    expect(orch.getInstance().state).toBe('RUNNING');
    expect(commands(process)).toContain('system_powerdown');
  });

  it('list_block_devices issues query-block and returns the canned result', async () => {
    const canned = [{ device: 'virtio0', inserted: { file: 'disk.qcow2' } }];
    const { orch, process } = await running({}, { responses: { 'query-block': canned } });
    expect(await orch.queryBlock()).toEqual(canned);
    expect(commands(process)).toContain('query-block');
  });

  it('query_cpus issues query-cpus-fast and returns the canned result', async () => {
    const canned = [{ 'cpu-index': 0, 'thread-id': 4242, target: 'x86_64' }];
    const { orch, process } = await running({}, { responses: { 'query-cpus-fast': canned } });
    expect(await orch.queryCpus()).toEqual(canned);
    expect(commands(process)).toContain('query-cpus-fast');
  });

  it('screendump issues QMP screendump to a SERVER-chosen path and returns image content', async () => {
    const png = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
    // The fake plays qemu: it writes the image to whatever path the SERVER chose.
    const { orch, process } = await running(
      {},
      {
        responses: {
          screendump: async (args?: Record<string, unknown>) => {
            await writeFile(args?.filename as string, png);
            return {};
          },
        },
      },
    );

    const result = await orch.screendump();

    // Image is returned inline (base64 PNG), not as a host path.
    expect(result).toEqual({
      mimeType: 'image/png',
      data: png.toString('base64'),
      bytes: png.length,
    });

    // The filename was server-chosen, under a server-controlled directory, and
    // the method takes no path input, so the agent cannot influence it.
    const call = process.executed.find((e) => e.command === 'screendump');
    const filename = call?.args?.filename as string;
    expect(filename.startsWith(join(tmpdir(), 'qmp-mcp', 'screendumps'))).toBe(true);
    expect(filename.endsWith('.png')).toBe(true);
    expect(call?.args?.format).toBe('png');

    // The temp file is cleaned up after the bytes are read back.
    await expect(stat(filename)).rejects.toThrow();
  });

  it('screendump paths are unguessable and unique per capture', async () => {
    const seen = new Set<string>();
    const { orch, process } = await running(
      {},
      {
        responses: {
          screendump: async (args?: Record<string, unknown>) => {
            await writeFile(args?.filename as string, Buffer.from('x'));
            return {};
          },
        },
      },
    );
    await orch.screendump();
    await orch.screendump();
    for (const e of process.executed) {
      if (e.command === 'screendump') seen.add(e.args?.filename as string);
    }
    expect(seen.size).toBe(2);
  });

  it('every control command rejects (actionably) when no Instance is running', async () => {
    const orch = makeOrchestrator(new FakeQemuDriver());
    for (const op of [
      () => orch.pauseInstance(),
      () => orch.resumeInstance(),
      () => orch.resetInstance(),
      () => orch.powerdownInstance(),
      () => orch.queryBlock(),
      () => orch.queryCpus(),
      () => orch.screendump(),
    ]) {
      await expect(op()).rejects.toBeInstanceOf(LifecycleError);
      await expect(op()).rejects.toThrow(/create_instance/);
    }
  });
});

describe('Orchestrator generic execute (Command Policy, fake driver)', () => {
  /** Create an Instance and hand back the orchestrator + its fake process. */
  async function running(options: Partial<OrchestratorOptions> = {}, driverOptions = {}) {
    const driver = new FakeQemuDriver(driverOptions);
    const orch = makeOrchestrator(driver, { autoStart: true, ...options });
    await orch.createInstance({});
    // biome-ignore lint/style/noNonNullAssertion: a process exists right after create.
    const process = driver.lastProcess!;
    return { orch, process };
  }

  const commands = (process: { executed: Array<{ command: string }> }): string[] =>
    process.executed.map((e) => e.command);

  it('forwards an ALLOWED command to the QMP session and returns its result', async () => {
    const canned = [{ bus: 0, devices: [] }];
    const { orch, process } = await running({}, { responses: { 'query-pci': canned } });

    expect(await orch.executeCommand('query-pci')).toEqual(canned);
    expect(commands(process)).toContain('query-pci');
  });

  it('passes arguments through to the QMP session for an allowed command', async () => {
    const { orch, process } = await running(
      { commandPolicy: buildPolicy({ allow: ['query-rocker'] }) },
      { responses: { 'query-rocker': (args?: Record<string, unknown>) => ({ echoed: args }) } },
    );

    expect(await orch.executeCommand('query-rocker', { name: 'sw1' })).toEqual({
      echoed: { name: 'sw1' },
    });
    expect(process.executed.find((e) => e.command === 'query-rocker')?.args).toEqual({
      name: 'sw1',
    });
  });

  it('a DENIED command never reaches the session and reports a hard denial', async () => {
    const { orch, process } = await running();
    await expect(orch.executeCommand('human-monitor-command')).rejects.toBeInstanceOf(
      CommandPolicyError,
    );
    await expect(orch.executeCommand('human-monitor-command')).rejects.toThrow(/hard denylist/i);
    // Fail-closed: it was never issued to QEMU.
    expect(commands(process)).not.toContain('human-monitor-command');
  });

  it('DENIES screendump through the generic path, closing the arbitrary host-file write (#11)', async () => {
    const { orch, process } = await running();
    // screendump writes an arbitrary host file at its `filename` arg; the generic
    // policy gates command NAMES not arguments, so it must be default-denied here
    // and the agent-chosen path must never reach QEMU.
    const err = await orch
      .executeCommand('screendump', { filename: '/home/user/.ssh/authorized_keys' })
      .catch((e: unknown) => e);
    expect(err).toBeInstanceOf(CommandPolicyError);
    // Default-deny (not allowlisted) — the dedicated screendump tool still serves
    // it with a server-chosen path, so this is not a hard denial.
    expect((err as CommandPolicyError).hardDenied).toBe(false);
    // Fail-closed: it never reached the QMP session, so no host file was written.
    expect(commands(process)).not.toContain('screendump');
  });

  it('an allow override cannot resurrect a hard-denied command through the orchestrator', async () => {
    const { orch, process } = await running({
      commandPolicy: buildPolicy({ allow: ['migrate'] }),
    });
    const err = await orch.executeCommand('migrate').catch((e: unknown) => e);
    expect(err).toBeInstanceOf(CommandPolicyError);
    expect((err as CommandPolicyError).hardDenied).toBe(true);
    expect(commands(process)).not.toContain('migrate');
  });

  it('rejects a non-allowlisted command before requiring an Instance', async () => {
    const orch = makeOrchestrator(new FakeQemuDriver());
    // No Instance running, but the policy denies it first — fail-closed.
    await expect(orch.executeCommand('totally-made-up')).rejects.toBeInstanceOf(CommandPolicyError);
  });

  it('rejects an allowed command with an actionable LifecycleError when no Instance is running', async () => {
    const orch = makeOrchestrator(new FakeQemuDriver());
    await expect(orch.executeCommand('query-status')).rejects.toBeInstanceOf(LifecycleError);
    await expect(orch.executeCommand('query-status')).rejects.toThrow(/create_instance/);
  });
});

describe('Orchestrator Event Buffer (fake driver)', () => {
  /** Create an Instance and hand back the orchestrator + its fake process. */
  async function running(options: Partial<OrchestratorOptions> = {}) {
    const driver = new FakeQemuDriver();
    const orch = makeOrchestrator(driver, { autoStart: true, ...options });
    await orch.createInstance({});
    // biome-ignore lint/style/noNonNullAssertion: a process exists right after create.
    const process = driver.lastProcess!;
    return { orch, driver, process };
  }

  it('captures events emitted by the Instance; get_events returns them with a cursor', async () => {
    const { orch, process } = await running();
    process.emitEvent({ event: 'STOP', data: { reason: 'pause' } });
    process.emitEvent({ event: 'RESET' });

    const { events, cursor } = orch.getEvents();
    expect(events.map((e) => e.event)).toEqual(['STOP', 'RESET']);
    expect(events[0]?.data).toEqual({ reason: 'pause' });
    expect(cursor).toBe(events[events.length - 1]?.seq);

    // Cursor paging: only newer events come back.
    process.emitEvent({ event: 'SHUTDOWN' });
    expect(orch.getEvents(cursor).events.map((e) => e.event)).toEqual(['SHUTDOWN']);
  });

  it('bounds the buffer: emitting past capacity evicts the oldest', async () => {
    const { orch, process } = await running({ eventBufferSize: 3 });
    for (const name of ['e1', 'e2', 'e3', 'e4', 'e5']) process.emitEvent({ event: name });

    const { events } = orch.getEvents();
    // Only the last 3 survive; e1/e2 were evicted — memory stays bounded.
    expect(events.map((e) => e.event)).toEqual(['e3', 'e4', 'e5']);
  });

  it('wait_for_event resolves when a matching (filtered) event is emitted', async () => {
    const { orch, process } = await running();
    const pending = orch.waitForEvent({ eventName: 'SHUTDOWN', timeoutMs: 1_000 });
    process.emitEvent({ event: 'STOP' }); // non-matching
    process.emitEvent({ event: 'SHUTDOWN', data: { guest: true } });

    const result = await pending;
    expect(result.timedOut).toBe(false);
    expect(result.event?.event).toBe('SHUTDOWN');
    expect(result.event?.data).toEqual({ guest: true });
  });

  it('wait_for_event with no filter resolves on any event', async () => {
    const { orch, process } = await running();
    const pending = orch.waitForEvent({ timeoutMs: 1_000 });
    process.emitEvent({ event: 'POWERDOWN' });
    const result = await pending;
    expect(result.timedOut).toBe(false);
    expect(result.event?.event).toBe('POWERDOWN');
  });

  it('wait_for_event times out cleanly (no throw) when no matching event arrives', async () => {
    const { orch, process } = await running();
    const pending = orch.waitForEvent({ eventName: 'SHUTDOWN', timeoutMs: 15 });
    process.emitEvent({ event: 'STOP' }); // never matches
    const result = await pending;
    expect(result.timedOut).toBe(true);
    expect(result.event).toBeUndefined();
  });

  it('wait_for_event is race-safe: sinceCursor catches an event buffered between calls', async () => {
    const { orch, process } = await running();
    // The event lands before the wait is issued; a future-only wait would miss it.
    process.emitEvent({ event: 'SHUTDOWN' });
    const cursorBefore = 0; // the agent had not seen any event yet
    const result = await orch.waitForEvent({
      eventName: 'SHUTDOWN',
      timeoutMs: 0,
      sinceCursor: cursorBefore,
    });
    expect(result.timedOut).toBe(false);
    expect(result.event?.event).toBe('SHUTDOWN');
    // Keep `process` referenced to satisfy lint on the destructured binding.
    expect(process.closed).toBe(false);
  });

  it('rejects get_events and wait_for_event actionably when no Instance is running', async () => {
    const orch = makeOrchestrator(new FakeQemuDriver());
    expect(() => orch.getEvents()).toThrow(LifecycleError);
    expect(() => orch.getEvents()).toThrow(/create_instance/);
    await expect(orch.waitForEvent({ timeoutMs: 0 })).rejects.toBeInstanceOf(LifecycleError);
    await expect(orch.waitForEvent({ timeoutMs: 0 })).rejects.toThrow(/create_instance/);
  });

  it('does not leak events across Instances: destroy + recreate starts empty', async () => {
    const driver = new FakeQemuDriver();
    const orch = makeOrchestrator(driver);

    await orch.createInstance({});
    const firstProcess = driver.lastProcess;
    firstProcess?.emitEvent({ event: 'STOP' });
    expect(orch.getEvents().events).toHaveLength(1);

    await orch.destroyInstance();
    await orch.createInstance({});

    // The new Instance's buffer is empty — no STOP from the previous Instance.
    expect(orch.getEvents().events).toEqual([]);
    // The now-stale first handle was unsubscribed on destroy, so it cannot inject
    // into the new Instance's buffer.
    firstProcess?.emitEvent({ event: 'RESET' });
    expect(orch.getEvents().events).toEqual([]);
  });

  it('settles a pending wait_for_event when the Instance is destroyed', async () => {
    const { orch } = await running();
    const pending = orch.waitForEvent({ eventName: 'SHUTDOWN', timeoutMs: 5_000 });
    await orch.destroyInstance();
    // The wait resolves as a clean timeout rather than hanging on the dead Instance.
    const result = await pending;
    expect(result.timedOut).toBe(true);
  });
});

describe('defaultSocketOccupied', () => {
  it('returns true for an existing path and false for a missing one', async () => {
    const dir = await mkdtemp(join(tmpdir(), 'orch-occ-'));
    try {
      const present = join(dir, 'present');
      await writeFile(present, '');
      expect(await defaultSocketOccupied(present)).toBe(true);
      expect(await defaultSocketOccupied(join(dir, 'absent'))).toBe(false);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});
