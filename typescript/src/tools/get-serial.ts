import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { orchestrator } from '../instance/orchestrator.js';

/**
 * Reports the Guest Serial Port capture configuration (ADR-0015): the backend, ring-buffer
 * size, how read_serial behaves (drain), whether writing is enabled, and a best-effort guess of
 * the guest console device. Takes no input. Read-only — never changes VM state. The Serial Port
 * is attached at boot via `create_instance` with `serial: true`. Auto-discovered from
 * `dist/tools`.
 */
export default class GetSerialTool extends MCPTool {
  name = 'get_serial';
  description =
    'Report the Guest Serial Port capture configuration: the backend, ring-buffer size, how ' +
    'read_serial behaves (drain), whether writing is enabled, and a best-effort guess of the guest ' +
    "console device (e.g. ttyS0 on q35/pc, ttyAMA0 on virt) — which the guest's own console= cmdline " +
    'ultimately decides. Attach the Serial Port at boot with create_instance serial: true. Read-only.';
  schema = z.object({});
  annotations = {
    title: 'Get Serial',
    readOnlyHint: true,
    openWorldHint: false,
  };

  async execute(): Promise<unknown> {
    return orchestrator.describeSerial();
  }
}
