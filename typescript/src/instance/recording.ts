/**
 * Guest Display recording (ADR-0017) — the `screendump`-loop → ffmpeg FALLBACK.
 *
 * This module implements the ADR's documented *fallback* pipeline (NOT the primary
 * live-RFB streaming path, which is a separate tracked issue): the Orchestrator polls
 * QMP `screendump` at a capped rate (ADR-0015 uses the same QMP surface) and feeds the
 * PNG frames to a co-located `ffmpeg` reading `-f image2pipe` from its stdin, which
 * encodes them to a file under the operator's recording root.
 *
 * The pure, byte-pinnable pieces live here and are unit-tested:
 *   - {@link buildFfmpegArgs}       — the exact ffmpeg argv for a codec/CRF/fps/pixfmt/out.
 *   - {@link recordingCapability}   — the fail-closed availability decision + actionable reason.
 *   - {@link parseFfmpegEncoders}   — parse `ffmpeg -encoders` output into encoder names.
 *   - {@link resolveRecordingPath}  — resolve the agent-named file under the operator root.
 *
 * The host-touching pieces ({@link probeFfmpegEncoders}, {@link spawnFfmpegSink}) spawn a
 * real ffmpeg and are injected into the Orchestrator so its lifecycle stays testable
 * against fakes — the real ffmpeg spawn is deliberately kept out of unit tests.
 */

import { execFile, spawn } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { join } from 'node:path';
import { logger } from '../logger.js';

/**
 * Video encoders this server knows how to advertise as recording codecs (ADR-0017).
 * `libx264` is the default and the one guaranteed present in every bundled and mainstream
 * distro ffmpeg; the others are smaller-file opt-ins the capability probe confirms are
 * compiled into the build before offering them.
 */
export const KNOWN_RECORDING_CODECS: readonly string[] = [
  'libx264',
  'libx265',
  'libvpx-vp9',
  'libsvtav1',
];

/**
 * Single-segment allowlist for an agent-supplied recording file name — the SAME charset as
 * `serialSpool` / the Image & ISO Store names: a leading alphanumeric then alphanumerics, dot,
 * underscore, or hyphen. It excludes the slash, `..`, and leading dot, so a name can never
 * escape the operator recording root (ADR-0017 reuses the spool trust model).
 */
export const RECORDING_NAME_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._-]*$/;

/** Output container extension. Matroska muxes every codec the operator may select (x264/x265/vp9/av1). */
export const RECORDING_FILE_EXTENSION = 'mkv';

/**
 * Raised when a recording name is invalid or the recording root is unconfigured. Mirrors the
 * `HardwareSpecError`/`ImageStoreError` pattern: the message names the offending value and the
 * remediation, so the caller can fix it.
 */
export class RecordingError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'RecordingError';
  }
}

/**
 * Build the exact ffmpeg argv for the screendump-loop fallback (ADR-0017): read PNG frames
 * from stdin as an image pipe, stamp each with a real wall-clock presentation timestamp
 * (`-use_wallclock_as_timestamps 1`), encode with the selected codec at quality-targeted CRF,
 * and mux variable-frame-rate (`-fps_mode vfr`) so the output's duration matches real elapsed
 * time regardless of how fast frames actually arrived. `libx264` additionally gets
 * `-tune stillimage`, the screen-content tuning for the static, text-heavy VM display.
 *
 * `maxFps` is intentionally NOT part of the argv: with wallclock timestamps + VFR it is only the
 * capture-loop pacing cap, so an input `-framerate` would double-count time and skew duration.
 * It stays in the signature (and the shared fixture tuple) so the golden fixture pins that it
 * never leaks into the argv (ADR-0012).
 *
 * Pure and deterministic so a golden fixture can pin it byte-for-byte across both
 * implementations (ADR-0012, `testdata/ffmpeg-argv/*.json`).
 */
export function buildFfmpegArgs(
  codec: string,
  crf: number,
  // Kept in the signature (and the shared fixture tuple) for cross-variant parity, but
  // deliberately NOT emitted into the argv — see the note above. Underscored so the linter
  // knows the omission is intentional, not an incomplete refactor.
  _maxFps: number,
  pixfmt: string,
  outPath: string,
): string[] {
  const args = [
    '-f',
    'image2pipe',
    '-use_wallclock_as_timestamps',
    '1',
    '-i',
    'pipe:0',
    '-c:v',
    codec,
  ];
  if (codec === 'libx264') {
    args.push('-tune', 'stillimage');
  }
  args.push('-crf', String(crf), '-pix_fmt', pixfmt, '-fps_mode', 'vfr', '-y', outPath);
  return args;
}

/** Inputs to the fail-closed recording-availability decision. */
export interface RecordingCapabilityInput {
  /** The operator recording root (`QMP_MCP_RECORDING_DIR`), or undefined when unset. */
  recordingDir: string | undefined;
  /** The selected codec (`QMP_MCP_RECORDING_CODEC`). */
  codec: string;
  /**
   * The encoders compiled into the ffmpeg build (from `ffmpeg -encoders`), or undefined when
   * ffmpeg could not be resolved or run — the "ffmpeg unavailable" signal.
   */
  encoders: string[] | undefined;
}

/** The recording-availability verdict: available, plus an actionable reason either way. */
export interface RecordingCapability {
  available: boolean;
  reason: string;
}

/**
 * Decide whether recording is available and why (ADR-0017), fail-closed: available IFF ffmpeg
 * resolved (a non-undefined encoder list) AND the recording root is set AND the selected codec
 * is compiled into the build. Each unmet condition returns a specific, actionable reason —
 * exactly as KVM inaccessibility is probed and reported rather than assumed (ADR-0008).
 */
export function recordingCapability(input: RecordingCapabilityInput): RecordingCapability {
  const { recordingDir, codec, encoders } = input;
  if (encoders === undefined) {
    return {
      available: false,
      reason:
        'Recording requires ffmpeg, which is not available. Install ffmpeg or set ' +
        'QMP_MCP_FFMPEG_BINARY to its path.',
    };
  }
  if (recordingDir === undefined || recordingDir.trim() === '') {
    return {
      available: false,
      reason:
        'Recording requires QMP_MCP_RECORDING_DIR — the operator host directory recordings are ' +
        'written under. Set it to an absolute path to enable recording.',
    };
  }
  if (!encoders.includes(codec)) {
    return {
      available: false,
      reason:
        `The selected recording codec "${codec}" is not compiled into this ffmpeg build ` +
        '(checked via "ffmpeg -encoders"). Set QMP_MCP_RECORDING_CODEC to an encoder this build ' +
        'carries (e.g. libx264), or rebuild ffmpeg with it.',
    };
  }
  return { available: true, reason: `Recording is available (codec "${codec}").` };
}

/**
 * Validate an agent-supplied recording file name against {@link RECORDING_NAME_PATTERN}.
 * Throws a {@link RecordingError} naming the constraint on failure. Pure and filesystem-free:
 * the single-segment charset guarantees the name is a leaf that cannot traverse out of the
 * operator recording root.
 */
export function validateRecordingName(name: string): void {
  if (!RECORDING_NAME_PATTERN.test(name)) {
    throw new RecordingError(
      `Recording name "${name}" must match ^[A-Za-z0-9][A-Za-z0-9._-]* — a single file name ` +
        "under QMP_MCP_RECORDING_DIR with no slash, '..', or leading dot (never a host path).",
    );
  }
}

/**
 * Resolve the host output path for a recording under the operator root (ADR-0017):
 * `<recordingDir>/<name>.mkv` for an agent-supplied (validated) name, or a server-chosen
 * `<recordingDir>/recording-<timestamp>-<rand>.mkv` when none is supplied. Throws a
 * {@link RecordingError} when the root is unconfigured. Because the name is a single validated
 * segment joined onto the operator root, it can never escape it.
 */
export function resolveRecordingPath(
  recordingDir: string | undefined,
  name: string | undefined,
): string {
  if (recordingDir === undefined || recordingDir.trim() === '') {
    throw new RecordingError('Recording requires QMP_MCP_RECORDING_DIR to be configured.');
  }
  let base: string;
  if (name === undefined) {
    base = `recording-${Date.now()}-${randomUUID().slice(0, 8)}`;
  } else {
    validateRecordingName(name);
    base = name;
  }
  return join(recordingDir, `${base}.${RECORDING_FILE_EXTENSION}`);
}

/**
 * Parse the output of `ffmpeg -encoders` into the list of encoder names. The command lists a
 * flags column (six chars like `V....D`) followed by the encoder name and a description, after
 * a `------` separator line; this returns the second token of every post-separator entry.
 * Pure so it can be unit-tested against a captured corpus without spawning ffmpeg.
 */
export function parseFfmpegEncoders(output: string): string[] {
  const names: string[] = [];
  let pastHeader = false;
  for (const line of output.split('\n')) {
    if (!pastHeader) {
      if (line.includes('------')) pastHeader = true;
      continue;
    }
    const name = /^\s*[A-Z.]{6}\s+(\S+)/.exec(line)?.[1];
    if (name !== undefined) names.push(name);
  }
  return names;
}

/**
 * Probe the ffmpeg build for its compiled-in encoders by running `ffmpeg -encoders` (ADR-0017).
 * Resolves to the encoder-name list, or `undefined` when the binary cannot be found or run —
 * the "ffmpeg unavailable" signal {@link recordingCapability} keys off. Never rejects: a probe
 * failure is a normal "not available" outcome, not an error to propagate.
 */
export function probeFfmpegEncoders(binary: string): Promise<string[] | undefined> {
  return new Promise((resolve) => {
    execFile(
      binary,
      ['-hide_banner', '-encoders'],
      { maxBuffer: 8 * 1024 * 1024 },
      (err, stdout) => {
        if (err) {
          resolve(undefined);
          return;
        }
        resolve(parseFfmpegEncoders(stdout));
      },
    );
  });
}

/**
 * How long {@link RecordingSink.finish} waits for ffmpeg to flush and finalize the container
 * after stdin closes before it force-kills the encoder (ADR-0017, T1). Ample for a low-fps
 * screen capture; it bounds the teardown path so a wedged ffmpeg can never block qemu close.
 * Mirrors the Rust `FFMPEG_FINALIZE_TIMEOUT`.
 */
export const FFMPEG_FINALIZE_TIMEOUT_MS = 5_000;

/**
 * How long {@link RecordingSink.checkAlive} watches a freshly-spawned ffmpeg for an immediate
 * exit before declaring it live (ADR-0017 post-spawn liveness, parity with Rust R5).
 */
export const FFMPEG_LIVENESS_WINDOW_MS = 150;

/** Cap on the ffmpeg stderr this sink retains for the liveness diagnostic (last N bytes). */
const STDERR_CAPTURE_LIMIT = 4_096;

/**
 * The recording encoder sink: the frame loop writes PNG frames into it, then either
 * {@link finish}es it (end-of-input, await the encoder finalizing the file — bounded, force-kills
 * on overrun) on `stop_recording` or {@link abort}s it (kill the encoder) on teardown. Injected
 * into the Orchestrator so the lifecycle is testable against a fake — the real implementation
 * spawns ffmpeg.
 */
export interface RecordingSink {
  /**
   * Write one encoded frame (PNG image bytes) to the encoder's stdin. Returns `false` when the
   * OS pipe buffer is full (backpressure); the caller must then await {@link drain} before the
   * next write so a slow encoder cannot grow the heap unbounded (T3).
   */
  writeFrame(frame: Buffer): boolean;
  /** Resolve once the encoder's stdin has drained after {@link writeFrame} returned `false` (T3). */
  drain(): Promise<void>;
  /**
   * Resolve after `ms`, or sooner if the encoder exits first, reporting whether it is still alive
   * and any stderr it emitted — used right after spawn to surface an ffmpeg that dies immediately
   * (bad codec/pixfmt/output) as an actionable `start_recording` error (T5, parity with Rust R5).
   */
  checkAlive(ms: number): Promise<{ alive: boolean; stderr: string }>;
  /**
   * End the input stream and await the encoder finalizing the file, BOUNDED by
   * {@link FFMPEG_FINALIZE_TIMEOUT_MS}: on overrun it force-kills via {@link abort} so teardown
   * never blocks (T1).
   */
  finish(): Promise<void>;
  /** Abort immediately: destroy the input and kill the encoder, then await its exit. */
  abort(): Promise<void>;
}

/** Factory that starts a {@link RecordingSink} for a given ffmpeg binary + argv. */
export type RecordingSinkFactory = (binary: string, args: string[]) => RecordingSink;

/**
 * The real {@link RecordingSink}: spawn ffmpeg with the built argv, pipe PNG frames to its
 * stdin, and drain stderr so a full pipe never blocks it.
 *
 * Hardening (ADR-0017):
 *  - {@link finish} ends stdin and awaits exit BOUNDED (T1): force-kills on overrun so a wedged
 *    ffmpeg can never block qemu close.
 *  - stdin `'error'` (EPIPE from a mid-recording ffmpeg death) is swallowed/logged (T2) so it can
 *    never surface as an unhandled error that crashes the server.
 *  - {@link writeFrame} honours the stream's backpressure signal and {@link drain} waits for the
 *    pipe to empty (T3).
 *  - {@link checkAlive} surfaces an ffmpeg that exits immediately after spawn, with its stderr (T5).
 */
export const spawnFfmpegSink: RecordingSinkFactory = (binary, args) => {
  const child = spawn(binary, args, { stdio: ['pipe', 'ignore', 'pipe'] });
  let hasExited = false;
  let stderrText = '';
  const exited = new Promise<void>((resolve) => {
    const done = (): void => {
      hasExited = true;
      resolve();
    };
    child.once('exit', done);
    child.once('error', done);
  });
  // Surface ffmpeg diagnostics at debug, keep the stderr pipe drained, and retain the tail so an
  // immediate-exit liveness failure can report why (T5).
  child.stderr?.on('data', (chunk: Buffer) => {
    const text = chunk.toString('utf8');
    stderrText = (stderrText + text).slice(-STDERR_CAPTURE_LIMIT);
    logger.debug(`ffmpeg: ${text.trimEnd()}`);
  });
  // A spawn error (e.g. ENOENT) would otherwise crash the process; the exited promise
  // already resolves on it, so swallow it here — the capability probe is the real gate.
  child.on('error', (err) => {
    logger.warning(`ffmpeg recording process error: ${err.message}`);
  });
  // T2: a mid-recording ffmpeg death makes the next stdin write emit EPIPE; without a listener
  // that would be an unhandled 'error' that exits the whole server. Swallow + log it here.
  child.stdin?.on('error', (err) => {
    logger.warning(`ffmpeg recording stdin error (encoder gone?): ${err.message}`);
  });

  const abort = async (): Promise<void> => {
    child.stdin?.destroy();
    child.kill('SIGKILL');
    await exited;
  };

  return {
    writeFrame(frame): boolean {
      const { stdin } = child;
      if (!stdin?.writable) return true; // nothing to write to → no backpressure
      return stdin.write(frame);
    },
    drain(): Promise<void> {
      const { stdin } = child;
      if (!stdin || stdin.writableEnded || stdin.destroyed) return Promise.resolve();
      return new Promise<void>((resolve) => {
        const done = (): void => {
          stdin.off('drain', done);
          stdin.off('close', done);
          stdin.off('error', done);
          resolve();
        };
        stdin.once('drain', done);
        // Never hang if the stream dies while we wait for it to drain.
        stdin.once('close', done);
        stdin.once('error', done);
      });
    },
    async checkAlive(ms): Promise<{ alive: boolean; stderr: string }> {
      let timer: NodeJS.Timeout | undefined;
      await Promise.race([
        exited,
        new Promise<void>((resolve) => {
          timer = setTimeout(resolve, ms);
        }),
      ]);
      if (timer) clearTimeout(timer);
      return { alive: !hasExited, stderr: stderrText.trim() };
    },
    async finish(): Promise<void> {
      child.stdin?.end();
      let timer: NodeJS.Timeout | undefined;
      const timedOut = await Promise.race([
        exited.then(() => false),
        new Promise<boolean>((resolve) => {
          timer = setTimeout(() => resolve(true), FFMPEG_FINALIZE_TIMEOUT_MS);
        }),
      ]);
      if (timer) clearTimeout(timer);
      // T1: ffmpeg overran the finalize window — force-kill it so teardown never blocks.
      if (timedOut) await abort();
    },
    abort,
  };
};
