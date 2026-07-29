import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { downloadManagerFromEnv } from '../instance/download.js';

/**
 * Reports the ISO downloader's capability (whether `QMP_MCP_ALLOW_DOWNLOAD` is set) and the
 * current or most recent download's progress: state, bytes/total/percent, the mirror in use,
 * and the final path once complete (ADR-0018). Takes no input. Read-only. Auto-discovered
 * from `dist/tools`.
 */
export default class GetDownloadTool extends MCPTool {
  name = 'get_download';
  description =
    'Report the ISO downloader status: whether downloading is enabled, the catalog source and ' +
    'size, whether a download is active, and the current or most recent download’s progress ' +
    '(state, bytesDownloaded/totalBytes/percent, the mirror being tried, and the final path on ' +
    'completion). Read-only.';
  schema = z.object({});
  annotations = {
    title: 'Get Download',
    readOnlyHint: true,
    openWorldHint: false,
  };

  async execute(): Promise<unknown> {
    return downloadManagerFromEnv().describe();
  }
}
