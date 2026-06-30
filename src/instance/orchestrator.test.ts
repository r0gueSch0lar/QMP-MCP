import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';
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
    binary: 'qemu-system-x86_64',
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

  it('create brings the Instance to RUNNING and launches via the driver port', async () => {
    const driver = new FakeQemuDriver();
    const orch = makeOrchestrator(driver);

    const result = await orch.createInstance({ machine: 'q35', memoryMb: 128 });

    expect(result.state).toBe('RUNNING');
    expect(orch.getInstance().state).toBe('RUNNING');
    expect(driver.launches).toHaveLength(1);
    expect(driver.launches[0]?.binary).toBe('qemu-system-x86_64');
    expect(driver.launches[0]?.qmpSocketPath).toBe(SOCK);
    // The driver was handed a generated argv that wires the QMP socket.
    expect(driver.launches[0]?.argv).toContain('-qmp');
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

  it('rejects create while an Instance already exists, actionably', async () => {
    const orch = makeOrchestrator(new FakeQemuDriver());
    await orch.createInstance({});
    await expect(orch.createInstance({})).rejects.toBeInstanceOf(LifecycleError);
    await expect(orch.createInstance({})).rejects.toThrow(/destroy_instance/);
    // Still exactly one Instance.
    expect(orch.getInstance().state).toBe('RUNNING');
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
    await expect(orch.createInstance({})).resolves.toMatchObject({ state: 'RUNNING' });
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
    expect(orch.getInstance().state).toBe('RUNNING');

    // qemu vanishes on its own (crash/SIGKILL) without an explicit destroy.
    driver.lastProcess?.simulateExit();
    await tick(); // let `exited.then(onProcessExit)` run

    expect(orch.getInstance().state).toBe('NONE');
    // The crashed Instance's handle was released, so a new create is accepted.
    await expect(orch.createInstance({})).resolves.toMatchObject({ state: 'RUNNING' });
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
    expect(orch.getInstance().state).toBe('RUNNING');
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
