/**
 * The ISO Store (ADR-0006): a separate, *read-only* directory that holds
 * installation/boot ISO media, kept distinct from the read-write Image Store so
 * install media can never be modified. ISOs are referenced by *name* within it,
 * never by host path.
 *
 * This module is deliberately the read-only twin of `image-store.ts`: it lists
 * ISOs and resolves a name to a safe in-Store path, but has NO create operation
 * and never writes (no `mkdir`, no `qemu-img`). The containment boundary —
 * name-validation plus realpath-containment — is NOT re-implemented here; it is
 * the very same shared {@link resolveInStore} the Image Store uses, specialised
 * only by {@link ISO_LABELS}. Sharing one resolver is the whole point: the
 * security-critical code exists once and both stores move together.
 */

import type { Dirent } from 'node:fs';
import { readdir, stat } from 'node:fs/promises';
import { join } from 'node:path';
import { resolveIsoDir } from '../config.js';
import { assertValidStoreName, resolveInStore, type StoreLabels } from './store-path.js';

/**
 * Raised for any ISO Store violation: an invalid/traversing ISO name, a symlink
 * that escapes the Store, or a missing Store directory. The message is always
 * actionable. Distinct from `ImageStoreError` so callers can tell which store a
 * failure came from, even though both share one resolver.
 */
export class IsoStoreError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'IsoStoreError';
  }
}

/**
 * ISO-Store wording for the shared {@link resolveInStore} boundary. Supplying
 * {@link IsoStoreError} keeps ISO failures surfacing as an `IsoStoreError` while
 * reusing the Image Store's exact validation + containment.
 */
const ISO_LABELS: StoreLabels = {
  store: 'ISO Store',
  entry: 'ISO',
  envVar: 'QMP_MCP_ISO_DIR',
  error: IsoStoreError,
};

/**
 * Validate that an ISO name is a single, safe path segment — the
 * ISO-Store-flavoured view of the shared name allowlist. Throws an actionable
 * {@link IsoStoreError}.
 */
export function assertValidIsoName(name: string): void {
  assertValidStoreName(name, ISO_LABELS);
}

/**
 * Resolve an ISO name against the ISO Store directory and return its safe
 * absolute path, or throw {@link IsoStoreError}. A one-line delegation to the
 * shared {@link resolveInStore} containment boundary — the SAME implementation
 * the Image Store's {@link resolveImagePath} uses, specialised by
 * {@link ISO_LABELS}. Rejects absolute paths, `..`/separator traversal,
 * option-injection characters, and symlink escape out of the Store.
 */
export function resolveIsoPath(name: string, storeDir: string): string {
  return resolveInStore(name, storeDir, ISO_LABELS);
}

/** An ISO present in the Store, as reported by {@link IsoStore.list}. */
export interface IsoInfo {
  /** The bare name to reference this ISO by. */
  name: string;
  /** On-disk (host) size in bytes. */
  sizeBytes: number;
}

/** Options for an {@link IsoStore}. */
export interface IsoStoreOptions {
  /** Absolute path of the ISO Store directory. */
  dir: string;
}

/**
 * The read-only ISO Store: lists the ISO media present in the Store. There is
 * deliberately no create/write path — install media is fixed and the Store is
 * treated read-only (ADR-0006), so this class never calls `mkdir` or spawns
 * anything.
 */
export class IsoStore {
  readonly dir: string;

  constructor(options: IsoStoreOptions) {
    this.dir = options.dir;
  }

  /**
   * List the ISOs in the Store: regular files only (symlinks are skipped rather
   * than followed, so a planted symlink can never surface as a listable "ISO").
   * Fails closed with an actionable message naming `QMP_MCP_ISO_DIR` when the
   * Store is missing.
   */
  async list(): Promise<{ store: string; isos: IsoInfo[] }> {
    let entries: Dirent[];
    try {
      entries = await readdir(this.dir, { withFileTypes: true });
    } catch {
      throw new IsoStoreError(
        `ISO Store directory "${this.dir}" does not exist or is not accessible. ` +
          `Create it or set QMP_MCP_ISO_DIR to an existing directory.`,
      );
    }
    const isos: IsoInfo[] = [];
    for (const entry of entries) {
      if (!entry.isFile()) continue;
      let sizeBytes = 0;
      try {
        sizeBytes = (await stat(join(this.dir, entry.name))).size;
      } catch {
        // Raced away between readdir and stat; skip it.
        continue;
      }
      isos.push({ name: entry.name, sizeBytes });
    }
    isos.sort((a, b) => a.name.localeCompare(b.name));
    return { store: this.dir, isos };
  }
}

/**
 * Build an {@link IsoStore} from the environment (the ISO Store dir), sharing
 * {@link resolveIsoDir} with {@link loadConfig}. Read at call time so the tools
 * pick up the configured directory. Defined as a function (not a top-level
 * singleton) so importing this module for {@link resolveIsoPath} has no side
 * effects.
 */
export function isoStoreFromEnv(env: NodeJS.ProcessEnv = process.env): IsoStore {
  return new IsoStore({ dir: resolveIsoDir(env) });
}
