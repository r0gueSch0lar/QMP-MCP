import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { IMAGE_FORMATS, imageStoreFromEnv } from '../instance/image-store.js';

/** Input schema for {@link CreateImageTool}: name, size (GiB), and format. */
const createImageSchema = z.object({
  name: z
    .string()
    .min(1)
    .describe('Bare name for the new image inside the Image Store (no path separators).'),
  sizeGb: z
    .number()
    .int()
    .min(1)
    .describe('Virtual disk size in GiB. Rejected when it exceeds QMP_MCP_MAX_DISK_GB.'),
  format: z
    .enum(IMAGE_FORMATS)
    .default('qcow2')
    .describe("Image format: 'qcow2' (default) or 'raw'."),
});

type CreateImageInput = z.infer<typeof createImageSchema>;

/**
 * Creates a blank disk image of the requested name/size/format INSIDE the Image
 * Store (ADR-0006) via `qemu-img create`. Enforces the `QMP_MCP_MAX_DISK_GB`
 * size cap and the format allowlist, and rejects any name that escapes the Store.
 * Auto-discovered from `dist/tools`.
 */
export default class CreateImageTool extends MCPTool {
  name = 'create_image';
  description =
    'Create a blank disk image of the given name, size (GiB), and format (qcow2 or raw) inside the ' +
    'Image Store using qemu-img. Enforces the QMP_MCP_MAX_DISK_GB size cap and rejects names that ' +
    'escape the Store.';
  schema = createImageSchema;
  annotations = {
    title: 'Create Image',
    readOnlyHint: false,
    destructiveHint: false,
    idempotentHint: false,
    openWorldHint: false,
  };

  async execute(input: CreateImageInput): Promise<unknown> {
    return imageStoreFromEnv().create(input);
  }
}
