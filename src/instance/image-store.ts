/**
 * The Image Store (ADR-0006): a single configured, read-write directory that
 * holds guest disk images. Disks are referenced by *name* within it, never by
 * host path; new blank images may be created inside it via `qemu-img create`.
 *
 * This module is the security boundary. {@link resolveImagePath} is the airtight
 * containment check: a deterministic function of `(name, storeDir)` (plus the
 * live filesystem) that maps a disk name to a safe absolute path inside the Store
 * or throws an actionable {@link ImageStoreError}. It rejects absolute paths,
 * `..`/path-separator traversal, and symlink escape (a symlink whose real target
 * leaves the Store). It is exported standalone so every traversal case is unit
 * testable without spawning anything.
 *
 * Subdirectory policy: disk names are a SINGLE path segment — no `/` or `\`, and
 * never `.`/`..`. Nested names are rejected. This keeps the Store flat, makes the
 * traversal analysis trivial (the only path component below the Store is the leaf
 * itself), and is the simplest thing that is obviously correct.
 */

import { type ChildProcess, spawn } from 'node:child_process';
import { type Dirent, lstatSync, realpathSync } from 'node:fs';
import { readdir, stat } from 'node:fs/promises';
import { isAbsolute, join, sep } from 'node:path';
import { resolveImageDir, resolveMaxDiskGb } from '../config.js';

/** Disk image formats this server will create and pin explicitly into argv. */
export const IMAGE_FORMATS = ['qcow2', 'raw'] as const;
export type ImageFormat = (typeof IMAGE_FORMATS)[number];

/**
 * Raised for any Image Store violation: an invalid/traversing disk name, a
 * symlink that escapes the Store, a missing Store directory, an over-cap size,
 * or a failed `qemu-img` invocation. The message is always actionable.
 */
export class ImageStoreError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'ImageStoreError';
  }
}

/**
 * Conservative single-segment allowlist for disk image names: a leading
 * alphanumeric followed by alphanumerics, dot, underscore, or hyphen. This is
 * the security-critical rule — it excludes the comma, `=`, `:`, space, and
 * leading `-` that would otherwise let a name inject extra `-drive`/QemuOpts
 * properties (QEMU parses `-drive` values as comma-separated key=value props).
 */
const VALID_IMAGE_NAME = /^[A-Za-z0-9][A-Za-z0-9._-]*$/;

/**
 * Validate that a disk name is a single, safe path segment. Pure and
 * filesystem-free: rejects empties, NUL bytes, absolute paths, `.`/`..`, and any
 * name containing a path separator with their own actionable messages, then
 * enforces the {@link VALID_IMAGE_NAME} allowlist (no comma/`=`/`:`/space/leading
 * dash — values that would inject QemuOpts properties downstream). Throws an
 * actionable {@link ImageStoreError}.
 */
export function assertValidImageName(name: string): void {
  if (typeof name !== 'string' || name.trim() === '') {
    throw new ImageStoreError('Disk image name must be a non-empty string.');
  }
  if (name.includes('\0')) {
    throw new ImageStoreError(`Disk image name "${name}" contains a NUL byte.`);
  }
  if (isAbsolute(name)) {
    throw new ImageStoreError(
      `Disk image name "${name}" must be a bare name inside the Image Store, not an absolute path.`,
    );
  }
  if (name === '.' || name === '..') {
    throw new ImageStoreError(`Disk image name "${name}" is not a valid file name.`);
  }
  if (name.includes('/') || name.includes('\\')) {
    throw new ImageStoreError(
      `Disk image name "${name}" must not contain a path separator; subdirectories are not allowed in the Image Store.`,
    );
  }
  if (!VALID_IMAGE_NAME.test(name)) {
    throw new ImageStoreError(
      `Disk image name "${name}" must match ${VALID_IMAGE_NAME.source} — a single ` +
        `segment of letters, digits, dot, underscore, or hyphen, with no leading hyphen ` +
        `and no comma, '=', ':', or spaces (these could inject QEMU -drive properties).`,
    );
  }
}

/**
 * Resolve a disk name against the Image Store directory and return its safe
 * absolute path, or throw {@link ImageStoreError}. The containment guarantee:
 *
 *  1. The name passes {@link assertValidImageName} (single safe segment).
 *  2. The Store directory's real (symlink-resolved) path is computed; a missing
 *     Store fails closed with an actionable message.
 *  3. If the target already exists, its real path must stay within the Store's
 *     real path — so a symlink (or dangling symlink) at the leaf that points
 *     outside the Store is rejected rather than followed.
 *
 * Because the name is a single non-`..` segment joined onto the *canonical*
 * Store path, a not-yet-existing target cannot escape; the realpath check closes
 * the symlink-at-the-leaf hole for targets that do exist.
 */
export function resolveImagePath(name: string, storeDir: string): string {
  // NOTE: residual resolve->use TOCTOU and hardlink-to-target hardening (O_NOFOLLOW
  // / fd plumbing) is out of the agent threat model — the agent cannot plant
  // symlinks/hardlinks; it only writes via `qemu-img create`. Deliberately not done.
  assertValidImageName(name);

  let realStore: string;
  try {
    realStore = realpathSync(storeDir);
  } catch {
    throw new ImageStoreError(
      `Image Store directory "${storeDir}" does not exist or is not accessible. ` +
        `Create it or set QMP_MCP_IMAGE_DIR to an existing directory.`,
    );
  }

  const candidate = join(realStore, name);

  // Only existing leaves can introduce a symlink escape; a missing leaf is safe
  // by construction (single segment under the canonical Store path).
  let exists = true;
  try {
    lstatSync(candidate);
  } catch {
    exists = false;
  }
  if (exists) {
    let real: string;
    try {
      real = realpathSync(candidate);
    } catch {
      throw new ImageStoreError(
        `Disk image "${name}" is a dangling symlink; refusing to follow it out of the Image Store.`,
      );
    }
    if (real !== candidate && !real.startsWith(realStore + sep)) {
      throw new ImageStoreError(
        `Disk image "${name}" resolves outside the Image Store (symlink escape); refusing.`,
      );
    }
  }

  return candidate;
}

/** A disk image present in the Store, as reported by {@link ImageStore.list}. */
export interface ImageInfo {
  /** The bare name to reference this image by. */
  name: string;
  /** On-disk (host) size in bytes — sparse images report their allocated size. */
  sizeBytes: number;
}

/** A request to create a blank image inside the Store. */
export interface CreateImageRequest {
  /** Bare name of the image to create (resolved through {@link resolveImagePath}). */
  name: string;
  /** Virtual size in GiB; capped by {@link ImageStore.maxDiskGb}. */
  sizeGb: number;
  /** Image format; validated against {@link IMAGE_FORMATS}. */
  format: ImageFormat;
}

/** The outcome of a successful {@link ImageStore.create}. */
export interface CreateImageResult {
  name: string;
  /** Absolute host path of the created image (inside the Store). */
  path: string;
  format: ImageFormat;
  sizeGb: number;
}

/** Runs `qemu-img` and resolves on exit 0, else rejects with its stderr. */
export type QemuImgRunner = (binary: string, args: string[]) => Promise<void>;

/** Options for an {@link ImageStore}. */
export interface ImageStoreOptions {
  /** Absolute path of the Image Store directory. */
  dir: string;
  /** Hard cap on virtual disk size, in GiB. */
  maxDiskGb: number;
  /** The `qemu-img` binary to invoke (default `qemu-img`). */
  qemuImgBinary?: string;
  /** Injected `qemu-img` runner (default spawns the real binary). */
  run?: QemuImgRunner;
}

/**
 * Spawn `qemu-img` as a child process (mirroring how the real QEMU driver spawns
 * `qemu-system-*`): no shell, argv array, stderr captured for diagnostics.
 */
function spawnQemuImg(binary: string, args: string[]): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    const child: ChildProcess = spawn(binary, args, { stdio: ['ignore', 'ignore', 'pipe'] });
    let stderr = '';
    child.stderr?.setEncoding('utf8');
    child.stderr?.on('data', (chunk: string) => {
      stderr += chunk;
    });
    child.once('error', (err: Error) =>
      reject(new Error(`Failed to spawn ${binary}: ${err.message}`)),
    );
    child.once('exit', (code, signal) => {
      if (code === 0) {
        resolve();
        return;
      }
      reject(
        new Error(
          `${binary} ${args.join(' ')} failed (code=${code}, signal=${signal}): ` +
            `${stderr.trim() || '(no stderr)'}`,
        ),
      );
    });
  });
}

/**
 * The read-write Image Store: lists images present in the Store and creates new
 * blank ones via `qemu-img create`, enforcing the size cap, the format
 * allowlist, and the {@link resolveImagePath} containment boundary.
 */
export class ImageStore {
  readonly dir: string;
  readonly maxDiskGb: number;
  #binary: string;
  #run: QemuImgRunner;

  constructor(options: ImageStoreOptions) {
    this.dir = options.dir;
    this.maxDiskGb = options.maxDiskGb;
    this.#binary = options.qemuImgBinary ?? 'qemu-img';
    this.#run = options.run ?? ((bin, args) => spawnQemuImg(bin, args));
  }

  /**
   * List the disk images in the Store: regular files only (symlinks are skipped
   * rather than followed, so a planted symlink can never surface as a listable
   * "image"). Fails closed with an actionable message when the Store is missing.
   */
  async list(): Promise<{ store: string; images: ImageInfo[] }> {
    let entries: Dirent[];
    try {
      entries = await readdir(this.dir, { withFileTypes: true });
    } catch {
      throw new ImageStoreError(
        `Image Store directory "${this.dir}" does not exist or is not accessible. ` +
          `Create it or set QMP_MCP_IMAGE_DIR to an existing directory.`,
      );
    }
    const images: ImageInfo[] = [];
    for (const entry of entries) {
      if (!entry.isFile()) continue;
      let sizeBytes = 0;
      try {
        sizeBytes = (await stat(join(this.dir, entry.name))).size;
      } catch {
        // Raced away between readdir and stat; skip it.
        continue;
      }
      images.push({ name: entry.name, sizeBytes });
    }
    images.sort((a, b) => a.name.localeCompare(b.name));
    return { store: this.dir, images };
  }

  /**
   * Create a blank image of the requested name/size/format inside the Store.
   * Rejects over-cap sizes (naming `QMP_MCP_MAX_DISK_GB` and the
   * requested-vs-allowed values), unknown formats, escaping names, and a name
   * that is already taken.
   */
  async create(request: CreateImageRequest): Promise<CreateImageResult> {
    const { name, sizeGb, format } = request;

    if (!Number.isInteger(sizeGb) || sizeGb < 1) {
      throw new ImageStoreError(
        `Disk size must be a positive integer number of GiB (got ${String(sizeGb)}).`,
      );
    }
    if (sizeGb > this.maxDiskGb) {
      throw new ImageStoreError(
        `Requested disk size ${sizeGb} GiB exceeds the maximum allowed ${this.maxDiskGb} GiB ` +
          `(QMP_MCP_MAX_DISK_GB). Request ${this.maxDiskGb} GiB or less.`,
      );
    }
    if (!(IMAGE_FORMATS as readonly string[]).includes(format)) {
      throw new ImageStoreError(
        `Unsupported image format "${format}". Allowed formats: ${IMAGE_FORMATS.join(', ')}.`,
      );
    }

    // Containment boundary: rejects absolute/`..`/separator/symlink-escape names.
    const path = resolveImagePath(name, this.dir);

    // Refuse to clobber or write through an existing entry. resolveImagePath has
    // already proven any existing leaf is contained, so this is a friendly guard.
    let taken = false;
    try {
      lstatSync(path);
      taken = true;
    } catch {
      taken = false;
    }
    if (taken) {
      throw new ImageStoreError(
        `An image named "${name}" already exists in the Image Store. Choose another name or remove it first.`,
      );
    }

    try {
      await this.#run(this.#binary, ['create', '-f', format, path, `${sizeGb}G`]);
    } catch (err) {
      throw new ImageStoreError(
        `Failed to create image "${name}": ${err instanceof Error ? err.message : String(err)}`,
      );
    }

    return { name, path, format, sizeGb };
  }
}

/**
 * Build an {@link ImageStore} from the environment (the Image Store dir and size
 * cap), sharing {@link resolveImageDir}/{@link resolveMaxDiskGb} with
 * {@link loadConfig}. Read at call time so the tools pick up the configured
 * directory. Defined as a function (not a top-level singleton) so importing this
 * module for {@link resolveImagePath} has no side effects.
 */
export function imageStoreFromEnv(env: NodeJS.ProcessEnv = process.env): ImageStore {
  return new ImageStore({ dir: resolveImageDir(env), maxDiskGb: resolveMaxDiskGb(env) });
}
