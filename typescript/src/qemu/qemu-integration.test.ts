import { accessSync, constants } from 'node:fs';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { delimiter, join } from 'node:path';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { Orchestrator } from '../instance/orchestrator.js';
import { RealQemuDriver } from './real-driver.js';

const QEMU_BINARY = 'qemu-system-x86_64';

/** True when the QEMU binary is resolvable on PATH (executable). */
function qemuOnPath(): boolean {
  for (const dir of (process.env.PATH ?? '').split(delimiter)) {
    if (!dir) continue;
    try {
      accessSync(join(dir, QEMU_BINARY), constants.X_OK);
      return true;
    } catch {
      // try the next PATH entry
    }
  }
  return false;
}

// The ONE gated integration test: it spawns a real qemu under TCG (deterministic,
// no KVM required) and round-trips QMP. It auto-skips when qemu is absent so CI
// without qemu stays green.
describe.skipIf(!qemuOnPath())('real QEMU lifecycle (TCG)', () => {
  let dir: string;
  let orchestrator: Orchestrator;

  beforeAll(async () => {
    dir = await mkdtemp(join(tmpdir(), 'qmp-mcp-it-'));
    orchestrator = new Orchestrator(new RealQemuDriver(), {
      binary: QEMU_BINARY,
      qmpSocketPath: join(dir, 'qmp.sock'),
      // Force TCG so the test is deterministic regardless of host KVM.
      kvmAvailable: () => false,
      socketOccupied: async () => false,
    });
  });

  afterAll(async () => {
    // Defensive: ensure no qemu is left running if an assertion threw.
    if (orchestrator.getInstance().state !== 'NONE') {
      await orchestrator.destroyInstance().catch(() => undefined);
    }
    await rm(dir, { recursive: true, force: true });
  });

  it('create -> RUNNING under TCG, round-trips query-status, then destroy -> NONE', async () => {
    const created = await orchestrator.createInstance({ accel: 'tcg', memoryMb: 128, vcpus: 1 });
    expect(created.state).toBe('RUNNING');
    expect(created.accel).toBe('tcg');
    expect(orchestrator.getInstance().state).toBe('RUNNING');

    // The real QMP handshake completed; query-status round-trips. Launched with
    // -S, so the Guest CPUs are frozen at "prelaunch".
    const status = (await orchestrator.getStatus()) as { status?: string; running?: boolean };
    expect(status).toMatchObject({ running: false });
    expect(typeof status.status).toBe('string');

    await orchestrator.destroyInstance();
    expect(orchestrator.getInstance().state).toBe('NONE');
  }, 30_000);
});
