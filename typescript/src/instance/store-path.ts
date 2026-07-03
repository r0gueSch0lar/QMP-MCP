/**
 * The shared allowlisted-store boundary (ADR-0006). Both the read-write Image
 * Store and the read-only ISO Store reference their contents by *name* only —
 * never by host path — and both must reject the exact same family of attacks:
 * absolute paths, `..`/path-separator traversal, QemuOpts option-injection
 * characters in the name, and symlink escape out of the store.
 *
 * Rather than duplicate that security-critical logic per store (duplicated
 * security code was flagged as a drift risk: the two copies inevitably diverge
 * and one is hardened while the other rots), the name-validation and the
 * realpath-containment live HERE, once. {@link resolveImagePath} and
 * {@link resolveIsoPath} are thin wrappers over {@link resolveInStore} that only
 * supply store-specific {@link StoreLabels}. Touch the boundary here and both
 * stores move together.
 *
 * Subdirectory policy: a store name is a SINGLE path segment — no `/` or `\`, and
 * never `.`/`..`. Nested names are rejected. This keeps every store flat, makes
 * the traversal analysis trivial (the only path component below the store is the
 * leaf itself), and is the simplest thing that is obviously correct.
 */

import { lstatSync, realpathSync } from 'node:fs';
import { isAbsolute, join, sep } from 'node:path';

/**
 * Per-store wording (and error type) that turns this generic boundary's messages
 * into actionable, store-specific ones. The `error` constructor lets each store
 * keep raising its own error class (e.g. `ImageStoreError`, `IsoStoreError`)
 * while sharing one implementation — so existing `instanceof` checks keep working.
 */
export interface StoreLabels {
  /** The store's display name, e.g. `Image Store` or `ISO Store`. */
  store: string;
  /** The noun for a single entry, e.g. `Disk image` or `ISO`. */
  entry: string;
  /** The env var that configures the store directory, e.g. `QMP_MCP_IMAGE_DIR`. */
  envVar: string;
  /** Error constructor the store raises (so callers can `instanceof` it). */
  error: new (
    message: string,
  ) => Error;
}

/**
 * Conservative single-segment allowlist for store names: a leading alphanumeric
 * followed by alphanumerics, dot, underscore, or hyphen. This is the
 * security-critical rule — it excludes the comma, `=`, `:`, space, and leading
 * `-` that would otherwise let a name inject extra `-drive`/QemuOpts properties
 * (QEMU parses `-drive` values as comma-separated key=value props).
 */
export const VALID_STORE_NAME = /^[A-Za-z0-9][A-Za-z0-9._-]*$/;

/**
 * Validate that a store name is a single, safe path segment. Pure and
 * filesystem-free: rejects empties, NUL bytes, absolute paths, `.`/`..`, and any
 * name containing a path separator with their own actionable messages, then
 * enforces the {@link VALID_STORE_NAME} allowlist (no comma/`=`/`:`/space/leading
 * dash — values that would inject QemuOpts properties downstream). Throws the
 * store's `error` type.
 */
export function assertValidStoreName(name: string, labels: StoreLabels): void {
  const Err = labels.error;
  if (typeof name !== 'string' || name.trim() === '') {
    throw new Err(`${labels.entry} name must be a non-empty string.`);
  }
  if (name.includes('\0')) {
    throw new Err(`${labels.entry} name "${name}" contains a NUL byte.`);
  }
  if (isAbsolute(name)) {
    throw new Err(
      `${labels.entry} name "${name}" must be a bare name inside the ${labels.store}, not an absolute path.`,
    );
  }
  if (name === '.' || name === '..') {
    throw new Err(`${labels.entry} name "${name}" is not a valid file name.`);
  }
  if (name.includes('/') || name.includes('\\')) {
    throw new Err(
      `${labels.entry} name "${name}" must not contain a path separator; subdirectories are not allowed in the ${labels.store}.`,
    );
  }
  if (!VALID_STORE_NAME.test(name)) {
    throw new Err(
      `${labels.entry} name "${name}" must match ${VALID_STORE_NAME.source} — a single ` +
        `segment of letters, digits, dot, underscore, or hyphen, with no leading hyphen ` +
        `and no comma, '=', ':', or spaces (these could inject QEMU -drive properties).`,
    );
  }
}

/**
 * Resolve a name against a store directory and return its safe absolute path, or
 * throw the store's `error` type. The containment guarantee:
 *
 *  1. The name passes {@link assertValidStoreName} (single safe segment).
 *  2. The store directory's real (symlink-resolved) path is computed; a missing
 *     store fails closed with an actionable message naming the store's env var.
 *  3. If the target already exists, its real path must stay within the store's
 *     real path — so a symlink (or dangling symlink) at the leaf that points
 *     outside the store is rejected rather than followed.
 *
 * Because the name is a single non-`..` segment joined onto the *canonical* store
 * path, a not-yet-existing target cannot escape; the realpath check closes the
 * symlink-at-the-leaf hole for targets that do exist.
 */
export function resolveInStore(name: string, storeDir: string, labels: StoreLabels): string {
  // NOTE: residual resolve->use TOCTOU and hardlink-to-target hardening (O_NOFOLLOW
  // / fd plumbing) is out of the agent threat model — the agent cannot plant
  // symlinks/hardlinks; it only writes via `qemu-img create` (Image Store only).
  const Err = labels.error;
  assertValidStoreName(name, labels);

  let realStore: string;
  try {
    realStore = realpathSync(storeDir);
  } catch {
    throw new Err(
      `${labels.store} directory "${storeDir}" does not exist or is not accessible. ` +
        `Create it or set ${labels.envVar} to an existing directory.`,
    );
  }

  const candidate = join(realStore, name);

  // Only existing leaves can introduce a symlink escape; a missing leaf is safe
  // by construction (single segment under the canonical store path).
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
      throw new Err(
        `${labels.entry} "${name}" is a dangling symlink; refusing to follow it out of the ${labels.store}.`,
      );
    }
    if (real !== candidate && !real.startsWith(realStore + sep)) {
      throw new Err(
        `${labels.entry} "${name}" resolves outside the ${labels.store} (symlink escape); refusing.`,
      );
    }
  }

  return candidate;
}
