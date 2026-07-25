import { describe, expect, it } from 'vitest';
import {
  buildFfmpegArgs,
  parseFfmpegEncoders,
  RecordingError,
  recordingCapability,
  resolveRecordingPath,
  spawnFfmpegSink,
  validateRecordingName,
} from './recording.js';

describe('buildFfmpegArgs (ADR-0017)', () => {
  it('builds the wallclock+VFR image2pipe argv, adding -tune stillimage for libx264', () => {
    expect(buildFfmpegArgs('libx264', 22, 15, 'yuv420p', '/rec/out.mkv')).toEqual([
      '-f',
      'image2pipe',
      '-use_wallclock_as_timestamps',
      '1',
      '-i',
      'pipe:0',
      '-c:v',
      'libx264',
      '-tune',
      'stillimage',
      '-crf',
      '22',
      '-pix_fmt',
      'yuv420p',
      '-fps_mode',
      'vfr',
      '-y',
      '/rec/out.mkv',
    ]);
  });

  it('omits -tune stillimage for a non-libx264 codec and threads the overrides through', () => {
    expect(buildFfmpegArgs('libx265', 30, 24, 'yuv444p', '/rec/clip.mkv')).toEqual([
      '-f',
      'image2pipe',
      '-use_wallclock_as_timestamps',
      '1',
      '-i',
      'pipe:0',
      '-c:v',
      'libx265',
      '-crf',
      '30',
      '-pix_fmt',
      'yuv444p',
      '-fps_mode',
      'vfr',
      '-y',
      '/rec/clip.mkv',
    ]);
    // libsvtav1 likewise carries no stillimage tune.
    expect(buildFfmpegArgs('libsvtav1', 35, 15, 'yuv420p', '/rec/a.mkv')).not.toContain('-tune');
  });

  it('never emits an input -framerate — maxFps is only the capture-loop pacing cap now', () => {
    // Different maxFps values must not change the argv (regression guard against re-introducing
    // a constant input frame rate that would skew the wallclock/VFR duration).
    const a = buildFfmpegArgs('libx264', 22, 5, 'yuv420p', '/rec/x.mkv');
    const b = buildFfmpegArgs('libx264', 22, 60, 'yuv420p', '/rec/x.mkv');
    expect(a).toEqual(b);
    expect(a).not.toContain('-framerate');
    expect(a).toContain('-fps_mode');
  });

  it('accepts CRF 0 (lossless) — the arg builder does not constrain the range', () => {
    expect(buildFfmpegArgs('libx264', 0, 15, 'yuv420p', '/rec/ll.mkv')).toContain('0');
  });
});

describe('recordingCapability (ADR-0017)', () => {
  const encoders = ['libx264', 'libvpx-vp9'];

  it('is available when ffmpeg resolved, a dir is set, and the codec is in the build', () => {
    const cap = recordingCapability({ recordingDir: '/rec', codec: 'libx264', encoders });
    expect(cap.available).toBe(true);
    expect(cap.reason).toContain('libx264');
  });

  it('fails closed with an actionable reason when ffmpeg is unavailable (encoders undefined)', () => {
    const cap = recordingCapability({
      recordingDir: '/rec',
      codec: 'libx264',
      encoders: undefined,
    });
    expect(cap.available).toBe(false);
    expect(cap.reason).toContain('ffmpeg');
    expect(cap.reason).toContain('QMP_MCP_FFMPEG_BINARY');
  });

  it('fails closed when the recording directory is unset, naming the variable', () => {
    const cap = recordingCapability({ recordingDir: undefined, codec: 'libx264', encoders });
    expect(cap.available).toBe(false);
    expect(cap.reason).toContain('QMP_MCP_RECORDING_DIR');
  });

  it('fails closed when the selected codec is not compiled into the build', () => {
    const cap = recordingCapability({ recordingDir: '/rec', codec: 'libsvtav1', encoders });
    expect(cap.available).toBe(false);
    expect(cap.reason).toContain('libsvtav1');
    expect(cap.reason).toContain('-encoders');
  });
});

describe('parseFfmpegEncoders (ADR-0017)', () => {
  it('extracts encoder names from the flags column after the separator', () => {
    const output = [
      'Encoders:',
      ' V..... = Video',
      ' A..... = Audio',
      ' ------',
      ' V....D libx264              libx264 H.264 / AVC / MPEG-4 AVC',
      ' V....D libx265              libx265 H.265 / HEVC',
      ' V....D libvpx-vp9           libvpx VP9',
      ' A....D aac                  AAC (Advanced Audio Coding)',
    ].join('\n');
    expect(parseFfmpegEncoders(output)).toEqual(['libx264', 'libx265', 'libvpx-vp9', 'aac']);
  });

  it('returns an empty list when there is no encoder table', () => {
    expect(parseFfmpegEncoders('ffmpeg version 10\nunrelated output\n')).toEqual([]);
  });
});

describe('spawnFfmpegSink hardening (ADR-0017)', () => {
  it('swallows a stdin error when the encoder is already gone, never throwing (T2)', async () => {
    // A process that exits immediately: the frame writes that follow hit a dead reader and emit
    // EPIPE on stdin. Without the sink's swallowing 'error' handler that would be an unhandled
    // error that tears down the whole server. The sink must neither throw nor crash the process.
    const sink = spawnFfmpegSink('sh', ['-c', 'exit 0']);
    const status = await sink.checkAlive(500);
    expect(status.alive).toBe(false);
    // Push enough bytes to overflow the pipe so the write actually fails with EPIPE.
    expect(() => {
      for (let i = 0; i < 256; i++) sink.writeFrame(Buffer.alloc(64 * 1024, i % 256));
    }).not.toThrow();
    // Let any async 'error' be delivered to the swallowing handler before we finish.
    await new Promise((resolve) => setTimeout(resolve, 50));
    await sink.abort();
  });

  it('finish() finalizes a well-behaved encoder promptly on stdin EOF (T1 happy path)', async () => {
    // `cat` copies stdin to stdout (ignored) and exits when stdin closes — a stand-in encoder
    // that finalizes on EOF. finish() must resolve without hanging.
    const sink = spawnFfmpegSink('cat', []);
    expect(sink.writeFrame(Buffer.from('frame'))).toBe(true);
    const start = Date.now();
    await sink.finish();
    // Well under the 5s finalize timeout — proves the normal (non-force-kill) path.
    expect(Date.now() - start).toBeLessThan(4_000);
  });
});

describe('recording name + path resolution (ADR-0017)', () => {
  it('resolves an agent-named file as <dir>/<name>.mkv under the operator root', () => {
    expect(resolveRecordingPath('/rec', 'run1')).toBe('/rec/run1.mkv');
  });

  it('server-chooses a unique .mkv name under the root when none is supplied', () => {
    const a = resolveRecordingPath('/rec', undefined);
    const b = resolveRecordingPath('/rec', undefined);
    expect(a.startsWith('/rec/recording-')).toBe(true);
    expect(a.endsWith('.mkv')).toBe(true);
    expect(a).not.toBe(b);
  });

  it('rejects a name that could escape the root (slash, traversal, leading dot, empty)', () => {
    for (const bad of ['../etc', 'a/b', '.hidden', '', 'has space']) {
      expect(() => validateRecordingName(bad)).toThrowError(RecordingError);
      expect(() => resolveRecordingPath('/rec', bad)).toThrowError(/must match/);
    }
  });

  it('throws when the recording root is unconfigured', () => {
    expect(() => resolveRecordingPath(undefined, 'run1')).toThrowError(/QMP_MCP_RECORDING_DIR/);
  });
});
