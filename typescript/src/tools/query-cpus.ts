import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { orchestrator } from '../instance/orchestrator.js';

/**
 * Returns the running Instance's live QMP `query-cpus-fast` result: per-vCPU
 * information (index, thread id, target architecture) for the Guest. Takes no
 * input. Read-only. Fails if no Instance is running. Auto-discovered from
 * `dist/tools`.
 */
export default class QueryCpusTool extends MCPTool {
  name = 'query_cpus';
  description =
    "Return per-vCPU information for the running QEMU Instance's Guest (QMP query-cpus-fast). " +
    'Read-only. Fails if no Instance is running.';
  schema = z.object({});
  annotations = {
    title: 'Query CPUs',
    readOnlyHint: true,
    openWorldHint: false,
  };

  async execute(): Promise<unknown> {
    return orchestrator.queryCpus();
  }
}
