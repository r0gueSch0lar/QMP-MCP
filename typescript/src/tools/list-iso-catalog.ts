import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { downloadManagerFromEnv } from '../instance/download.js';

/**
 * Lists the OS installation images the downloader can fetch into the ISO Store (ADR-0018),
 * from the download catalog — each with its `id` (what `download_iso` selects), name, the
 * filename it is saved as, and its mirror URLs. The list is the built-in bundled catalog
 * unless `QMP_MCP_ISO_CATALOG` overrides it. Read-only. Auto-discovered from `dist/tools`.
 */
export default class ListIsoCatalogTool extends MCPTool {
  name = 'list_iso_catalog';
  description =
    'List the OS installation images available to download into the ISO Store, from the download ' +
    'catalog. Each entry has an id (pass it to download_iso), a name, the filename it will be ' +
    'saved as, and one or more mirror URLs. The catalog is the built-in list unless ' +
    'QMP_MCP_ISO_CATALOG overrides it. Read-only.';
  schema = z.object({});
  annotations = {
    title: 'List ISO Catalog',
    readOnlyHint: true,
    openWorldHint: false,
  };

  async execute(): Promise<unknown> {
    const catalog = downloadManagerFromEnv().getCatalog();
    return { source: catalog.source, count: catalog.entries.length, isos: catalog.entries };
  }
}
