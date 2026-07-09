import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { orchestrator } from '../instance/orchestrator.js';

/**
 * Reports the host↔guest folder-sharing configuration (ADR-0014): whether a host
 * folder is shared into guests, the fixed 9p mount tag, the intended guest mountpoint,
 * whether it is read-only, and the exact `mount` command to run inside the guest. Takes
 * no input. Read-only — it never changes VM state and never returns the host path. The
 * share is attached at boot via `create_instance` with `share: true`. Auto-discovered
 * from `dist/tools`.
 */
export default class GetShareTool extends MCPTool {
  name = 'get_share';
  description =
    'Report the host↔guest folder-sharing configuration: whether sharing is available, the 9p mount ' +
    'tag, the intended guest mountpoint, read-only vs read-write, and the exact mount command to run ' +
    'inside the guest. Attach the share at boot with create_instance share: true. Read-only.';
  schema = z.object({});
  annotations = {
    title: 'Get Share',
    readOnlyHint: true,
    openWorldHint: false,
  };

  async execute(): Promise<unknown> {
    return orchestrator.describeShare();
  }
}
