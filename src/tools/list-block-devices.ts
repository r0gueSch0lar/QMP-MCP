import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { orchestrator } from '../instance/orchestrator.js';

/**
 * Returns the running Instance's live QMP `query-block` result: the block
 * (storage) devices attached to the Guest and their backing media. Takes no
 * input. Read-only. Fails if no Instance is running. Auto-discovered from
 * `dist/tools`.
 */
export default class ListBlockDevicesTool extends MCPTool {
  name = 'list_block_devices';
  description =
    "Return the running QEMU Instance's block (storage) devices and their backing media (QMP " +
    'query-block). Read-only. Fails if no Instance is running.';
  schema = z.object({});
  annotations = {
    title: 'List Block Devices',
    readOnlyHint: true,
    openWorldHint: false,
  };

  async execute(): Promise<unknown> {
    return orchestrator.queryBlock();
  }
}
