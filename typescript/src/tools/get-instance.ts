import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { orchestrator } from '../instance/orchestrator.js';

/**
 * Reports the current Instance and its lifecycle state. Takes no input and
 * returns state `NONE` when no Instance is running. Auto-discovered by
 * mcp-framework from the compiled `dist/tools` directory.
 */
export default class GetInstanceTool extends MCPTool {
  name = 'get_instance';
  description =
    'Return the current QEMU Instance and its lifecycle state. Reports state "NONE" when no Instance is running.';
  schema = z.object({});
  annotations = {
    title: 'Get Instance',
    readOnlyHint: true,
    openWorldHint: false,
  };

  async execute(): Promise<unknown> {
    return orchestrator.getInstance();
  }
}
