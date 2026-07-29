/**
 * The ISO download catalog (ADR-0018): the operator-curated list of installable OS images
 * the `download_iso` tool can fetch, each keyed by a short `id` and carrying one or more
 * mirror URLs. The catalog is DATA, not code: the built-in list ships as
 * `assets/iso-catalog.json` (read at startup from a path relative to this module), and an
 * operator overrides it wholesale by pointing `QMP_MCP_ISO_CATALOG` at their own JSON.
 *
 * The security-relevant validation lives here: every entry's `filename` (the bare name the
 * image is saved as inside the ISO Store) is checked against the SAME shared store name
 * allowlist the Image/ISO stores use, so a catalog can never name a file that escapes the
 * Store. Mirrors `../../../rust/src/instance/catalog.rs`.
 */

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { assertValidStoreName, type StoreLabels } from './store-path.js';

/**
 * Raised when a catalog file cannot be read or fails validation (malformed JSON, empty,
 * duplicate id, unsafe filename, bad mirror URL). The message is always actionable and names
 * the offending entry. Mirrors the Rust `CatalogError`.
 */
export class CatalogError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'CatalogError';
  }
}

/** Store wording used to validate a catalog entry's `filename` as a safe ISO-Store leaf. */
const CATALOG_FILE_LABELS: StoreLabels = {
  store: 'ISO Store',
  entry: 'Catalog ISO filename',
  envVar: 'QMP_MCP_ISO_DIR',
  error: CatalogError,
};

/** Single-token id allowlist (same shape as a store name — no whitespace/injection chars). */
const ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._-]*$/;

/** One installable OS image in the catalog. */
export interface CatalogEntry {
  /** Short stable key the `download_iso` tool selects this image by (e.g. `ubuntu`). */
  id: string;
  /** Human-facing distro name (e.g. `Ubuntu`). */
  name: string;
  /** Optional edition/variant note (e.g. `24.04.2 LTS Desktop`). */
  edition?: string;
  /** Optional guest architecture the image targets (e.g. `x86_64`). Informational. */
  arch?: string;
  /** The bare name the image is saved as inside the ISO Store (validated store name). */
  filename: string;
  /** One or more download URLs, tried in order until one succeeds. */
  mirrors: string[];
}

/** A validated catalog plus a note of where it came from (`built-in` or a file path). */
export interface Catalog {
  /** Provenance: `built-in` for the bundled list, else the resolved file path. */
  source: string;
  /** The validated entries, in catalog order. */
  entries: CatalogEntry[];
}

/**
 * Path to the built-in bundled catalog, resolved relative to this module so it works both when
 * compiled (`dist/instance/catalog.js` → `<pkgroot>/assets/…`) and under vitest (the `.ts`
 * source → `<repo>/typescript/assets/…`). The Docker runtime image ships `assets/` alongside
 * `dist/` so this resolves in-container too.
 */
const BUILTIN_CATALOG_PATH = fileURLToPath(
  new URL('../../assets/iso-catalog.json', import.meta.url),
);

/** True when a mirror string is a clean `http(s)` URL (no whitespace or control characters). */
function isValidMirror(url: unknown): url is string {
  return (
    typeof url === 'string' &&
    (url.startsWith('http://') || url.startsWith('https://')) &&
    !/\s/.test(url) &&
    !Array.from(url).some((ch) => (ch.codePointAt(0) ?? 0) < 0x20)
  );
}

/**
 * Parse and validate a catalog from its JSON text, tagging it with `source` for reports. Fails
 * closed (with an actionable, entry-naming message) on malformed JSON, an empty list, a
 * duplicate/invalid id, a `filename` that is not a safe ISO-Store leaf, or a mirror that is not
 * an `http(s)` URL — so a bad catalog is rejected at load, never at download time. Extra
 * top-level keys (`$comment`, `version`) are ignored.
 */
export function parseCatalog(json: string, source: string): Catalog {
  let raw: unknown;
  try {
    raw = JSON.parse(json);
  } catch (err) {
    throw new CatalogError(
      `ISO catalog (${source}) is not valid JSON: ${err instanceof Error ? err.message : String(err)}`,
    );
  }
  const isos = (raw as { isos?: unknown }).isos;
  if (!Array.isArray(isos) || isos.length === 0) {
    throw new CatalogError(
      `ISO catalog (${source}) contains no entries (expected a non-empty "isos" array).`,
    );
  }
  const entries: CatalogEntry[] = [];
  const seen = new Set<string>();
  for (const item of isos) {
    const e = (item ?? {}) as Partial<CatalogEntry>;
    if (typeof e.id !== 'string' || !ID_PATTERN.test(e.id)) {
      throw new CatalogError(
        `ISO catalog (${source}) has an invalid id ${JSON.stringify(e.id)} — ids must be a single ` +
          'token of letters, digits, dot, underscore, or hyphen.',
      );
    }
    if (seen.has(e.id)) {
      throw new CatalogError(`ISO catalog (${source}) has a duplicate id "${e.id}".`);
    }
    seen.add(e.id);
    if (typeof e.name !== 'string' || e.name.trim() === '') {
      throw new CatalogError(`ISO catalog entry "${e.id}" (${source}) has an empty name.`);
    }
    if (typeof e.filename !== 'string') {
      throw new CatalogError(`ISO catalog entry "${e.id}" (${source}) is missing a filename.`);
    }
    // The filename must be a safe single-segment ISO-Store leaf (no traversal/injection), so a
    // downloaded image can never be written outside the Store.
    try {
      assertValidStoreName(e.filename, CATALOG_FILE_LABELS);
    } catch (err) {
      throw new CatalogError(
        `ISO catalog entry "${e.id}" (${source}) has an invalid filename: ${
          err instanceof Error ? err.message : String(err)
        }`,
      );
    }
    if (!Array.isArray(e.mirrors) || e.mirrors.length === 0) {
      throw new CatalogError(
        `ISO catalog entry "${e.id}" (${source}) has no mirrors (expected at least one URL).`,
      );
    }
    for (const url of e.mirrors) {
      if (!isValidMirror(url)) {
        throw new CatalogError(
          `ISO catalog entry "${e.id}" (${source}) has an invalid mirror URL ${JSON.stringify(url)} — ` +
            'mirrors must be http(s) URLs with no whitespace.',
        );
      }
    }
    entries.push({
      id: e.id,
      name: e.name,
      edition: e.edition,
      arch: e.arch,
      filename: e.filename,
      mirrors: e.mirrors,
    });
  }
  return { source, entries };
}

/** Find the entry with the given `id`, if any. */
export function findEntry(catalog: Catalog, id: string): CatalogEntry | undefined {
  return catalog.entries.find((e) => e.id === id);
}

/** A comma-separated list of the available ids, for an actionable "unknown id" message. */
export function idsHint(catalog: Catalog): string {
  return catalog.entries.map((e) => e.id).join(', ');
}

/**
 * Load the catalog from `path` (`QMP_MCP_ISO_CATALOG`) or fall back to the bundled built-in list
 * when it is undefined. A set-but-unreadable path fails closed with an actionable message naming
 * the variable, rather than silently falling back. Mirrors the Rust `load_catalog`.
 */
export function loadCatalog(path: string | undefined): Catalog {
  if (path === undefined) {
    let json: string;
    try {
      json = readFileSync(BUILTIN_CATALOG_PATH, 'utf8');
    } catch (err) {
      throw new CatalogError(
        `cannot read the built-in ISO catalog at ${BUILTIN_CATALOG_PATH}: ${
          err instanceof Error ? err.message : String(err)
        }`,
      );
    }
    return parseCatalog(json, 'built-in');
  }
  let json: string;
  try {
    json = readFileSync(path, 'utf8');
  } catch (err) {
    throw new CatalogError(
      `cannot read ISO catalog "${path}" (QMP_MCP_ISO_CATALOG): ${
        err instanceof Error ? err.message : String(err)
      }. Point it at a readable JSON file, or leave it unset to use the built-in list.`,
    );
  }
  return parseCatalog(json, path);
}
