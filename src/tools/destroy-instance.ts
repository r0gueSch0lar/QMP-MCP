import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { orchestrator } from '../instance/orchestrator.js';

/**
 * Terminates the running Instance's `qemu-system-*` process, closes its QMP
 * Session, and returns the lifecycle to `NONE`. Takes no input. Fails if no
 * Instance is running. Auto-discovered from `dist/tools`.
 */
export default class DestroyInstanceTool extends MCPTool {
  name = 'destroy_instance';
  description =
    'Terminate the running QEMU Instance and tear down its QMP session, returning the lifecycle to ' +
    'state "NONE". Fails if no Instance is running.';
  schema = z.object({});
  annotations = {
    title: 'Destroy Instance',
    readOnlyHint: false,
    destructiveHint: true,
    idempotentHint: true,
    openWorldHint: false,
  };

  async execute(): Promise<unknown> {
    return orchestrator.destroyInstance();
  }
}
