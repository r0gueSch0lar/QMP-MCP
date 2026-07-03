/**
 * The Image Store (ADR-0006): a single configured, read-write directory that
 * holds guest disk images. Disks are referenced by *name* within it, never by
 * host path; new blank images may be created inside it via `qemu-img create`.
 *
 * This module is the read-write half of the security boundary. The airtight
 * containment check — name validation plus realpath-containment — lives in the
 * shared {@link resolveInStore} (see `store-path.ts`), which the read-only ISO
 * Store reuses verbatim so the two stores cannot drift. {@link resolveImagePath}
 * and {@link assertValidImageName} are thin Image-Store-flavoured wrappers over
 * it, kept exported so every traversal case stays unit testable without spawning
 * anything.
 *
 * Subdirectory policy: disk names are a SINGLE path segment — no `/` or `\`, and
 * never `.`/`..`. Nested names are rejected. This keeps the Store flat, makes the
 * traversal analysis trivial (the only path component below the Store is the leaf
 * itself), and is the simplest thing that is obviously correct.
 */

import { type ChildProcess, spawn } from 'node:child_process';
import { type Dirent, lstatSync } from 'node:fs';
import { readdir, stat } from 'node:fs/promises';
import { join } from 'node:path';
import { resolveImageDir, resolveMaxDiskGb } from '../config.js';
import { assertValidStoreName, resolveInStore, type StoreLabels } from './store-path.js';

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
 * Image-Store wording for the shared {@link resolveInStore} boundary. Supplying
 * {@link ImageStoreError} keeps every traversal/injection failure surfacing as an
 * `ImageStoreError`, exactly as before the resolver was factored out.
 */
const IMAGE_LABELS: StoreLabels = {
  store: 'Image Store',
  entry: 'Disk image',
  envVar: 'QMP_MCP_IMAGE_DIR',
  error: ImageStoreError,
};

/**
 * Validate that a disk name is a single, safe path segment — the
 * Image-Store-flavoured view of the shared {@link assertValidStoreName}. Rejects
 * empties, NUL bytes, absolute paths, `.`/`..`, path separators, and any name
 * outside the option-injection-safe allowlist, throwing an actionable
 * {@link ImageStoreError}.
 */
export function assertValidImageName(name: string): void {
  assertValidStoreName(name, IMAGE_LABELS);
}

/**
 * Resolve a disk name against the Image Store directory and return its safe
 * absolute path, or throw {@link ImageStoreError}. A one-line delegation to the
 * shared {@link resolveInStore} containment boundary (same logic the ISO Store
 * uses), specialised only by {@link IMAGE_LABELS}.
 */
export function resolveImagePath(name: string, storeDir: string): string {
  return resolveInStore(name, storeDir, IMAGE_LABELS);
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
