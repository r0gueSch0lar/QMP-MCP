import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { orchestrator } from '../instance/orchestrator.js';

/**
 * Returns the running Instance's live QMP `query-status` result (e.g.
 * `{ status: "running", running: true }`). Takes no input. Read-only. Fails if
 * no Instance is running. Auto-discovered from `dist/tools`.
 */
export default class GetStatusTool extends MCPTool {
  name = 'get_status';
  description =
    "Return the running QEMU Instance's live QMP query-status result (run state of the Guest CPUs). " +
    'Fails if no Instance is running.';
  schema = z.object({});
  annotations = {
    title: 'Get Status',
    readOnlyHint: true,
    openWorldHint: false,
  };

  async execute(): Promise<unknown> {
    return orchestrator.getStatus();
  }
}
