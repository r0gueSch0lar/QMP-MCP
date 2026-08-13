import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { downloadManagerFromEnv } from '../instance/download.js';

/** Input schema for {@link DownloadIsoTool}: the catalog id to fetch. */
const downloadIsoSchema = z.object({
  id: z
    .string()
    .min(1)
    .describe(
      'The catalog id of the OS image to download (e.g. ubuntu, debian, archlinux). Use ' +
        'list_iso_catalog to see the available ids.',
    ),
});

type DownloadIsoInput = z.infer<typeof downloadIsoSchema>;

/**
 * Starts downloading an OS image (by catalog id) into the ISO Store (ADR-0018). DISABLED by
 * default — the server must run with `QMP_MCP_ALLOW_DOWNLOAD=true`, else it fails closed.
 * Returns immediately with the initial (downloading) status; the multi-GB fetch runs in the
 * background (poll `get_download`). Mirrors are tried in order and the file is atomically
 * renamed into place on success. Auto-discovered from `dist/tools`.
 */
export default class DownloadIsoTool extends MCPTool {
  name = 'download_iso';
  description =
    'Start downloading an OS image (by catalog id) into the ISO Store. DISABLED by default: the ' +
    'server must run with QMP_MCP_ALLOW_DOWNLOAD=true, else this fails closed. Returns immediately ' +
    '— the multi-GB fetch runs in the background; poll get_download for progress. Mirrors are ' +
    'tried in order and the file is atomically renamed into place on success. Fails if the id is ' +
    'unknown (see list_iso_catalog), the file already exists, or a download is already running ' +
    '(only one at a time).';
  schema = downloadIsoSchema;
  annotations = {
    title: 'Download ISO',
    readOnlyHint: false,
    destructiveHint: false,
    idempotentHint: false,
    openWorldHint: true,
  };

  async execute(input: DownloadIsoInput): Promise<unknown> {
    return downloadManagerFromEnv().start(input.id);
  }
}
