import { MCPTool } from 'mcp-framework';
import { z } from 'zod';
import { orchestrator } from '../instance/orchestrator.js';

/** Validated input for {@link StartRecordingTool}. */
const schema = z.object({
  name: z
    .string()
    .optional()
    .describe(
      'Optional file name for the recording (a single segment matching ' +
        '^[A-Za-z0-9][A-Za-z0-9._-]*, no slash/.. — never a host path). Resolved under ' +
        'QMP_MCP_RECORDING_DIR as <name>.mkv; omit to let the server choose a unique name.',
    ),
});

/**
 * Starts recording the running Instance's Display to a video file (ADR-0017), using the
 * `screendump`-loop → ffmpeg pipeline: a frame loop polls QMP screendump at up to
 * QMP_MCP_RECORDING_MAX_FPS and pipes the frames to a co-located ffmpeg encoding them under the
 * operator recording root. Recording is capability-gated on ffmpeg being available and carrying
 * the selected codec (QMP_MCP_RECORDING_CODEC). The finished file is NOT returned inline (videos
 * are large): the tool returns the host path the operator retrieves it from. Fails if no Instance
 * is running, the Instance has no display device, recording is unavailable, or one is already
 * running. Auto-discovered from `dist/tools`.
 */
export default class StartRecordingTool extends MCPTool {
  name = 'start_recording';
  description =
    "Start recording the running QEMU Instance's Display to a video file (screendump-loop to " +
    'ffmpeg). Requires an Instance with a display device and an available ffmpeg carrying the ' +
    'configured codec. Optionally name the file (resolved under QMP_MCP_RECORDING_DIR); the video ' +
    'is written to the host (not returned inline) and the tool returns its path. Fails if no ' +
    'Instance is running, there is no display device, recording is unavailable, or one is already ' +
    'running.';
  schema = schema;
  annotations = {
    title: 'Start Recording',
    readOnlyHint: false,
    openWorldHint: false,
  };

  async execute(input: z.infer<typeof schema>): Promise<unknown> {
    return orchestrator.startRecording(input.name);
  }
}
