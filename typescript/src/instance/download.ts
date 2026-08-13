/**
 * The ISO downloader (ADR-0018): fetch an OS image named in the catalog into the ISO Store,
 * on demand, over the network. It is an operator opt-in (`QMP_MCP_ALLOW_DOWNLOAD`) because it
 * is outbound egress that writes files; when disabled, `download_iso` fails closed.
 *
 * Shape (mirrors `../../../rust/src/instance/download.rs`):
 *   - ONE download runs at a time (a single slot — the same "one managed thing" discipline as
 *     the single-instance Orchestrator). `download_iso` starts it and returns immediately;
 *     `get_download` reports live progress. Multi-GB images never block a tool call.
 *   - The image streams to a `<name>.part` temp file in the ISO Store, then is atomically
 *     renamed onto the final `<name>` on success. Mirrors are tried in order; the first that
 *     succeeds wins.
 *   - Downloads are APPEND-ONLY: an existing Store file is never overwritten (the one
 *     relaxation of ADR-0006's read-only ISO Store — see ADR-0018), and the filename is the
 *     catalog's validated store-name leaf, so a download can never escape the Store.
 *
 * The network I/O is behind the {@link Fetcher} seam so the manager's state machine is
 * unit-tested against a fake, exactly as the Orchestrator is tested against a fake QEMU driver.
 */

import { once } from 'node:events';
import { createWriteStream } from 'node:fs';
import { rename, rm, stat } from 'node:fs/promises';
import { Readable } from 'node:stream';
import { resolveAllowDownload, resolveIsoCatalog, resolveIsoDir } from '../config.js';
import { getLogger } from '../logger.js';
import { type Catalog, findEntry, idsHint, loadCatalog } from './catalog.js';
import { resolveInStore, type StoreLabels } from './store-path.js';

const logger = getLogger('download');

/**
 * Raised for a `download_iso` request that cannot be started (disabled, unknown id, target
 * already present, or a download already in progress) — and reused as the ISO-Store label's
 * error type so a bad target surfaces as a `DownloadError`. Mirrors the Rust `DownloadError`.
 */
export class DownloadError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'DownloadError';
  }
}

/** ISO-Store wording for resolving the download target against the shared containment boundary. */
const ISO_LABELS: StoreLabels = {
  store: 'ISO Store',
  entry: 'ISO',
  envVar: 'QMP_MCP_ISO_DIR',
  error: DownloadError,
};

/** Abort a mirror if no bytes arrive within this window and try the next, so a stalled connection can never wedge the slot. */
export const STALL_TIMEOUT_MS = 60_000;

/** Progress callbacks the {@link Fetcher} invokes while streaming. */
export interface FetchCallbacks {
  /** Called once when the total size is known (from `Content-Length`), else with undefined. */
  onTotal(total: number | undefined): void;
  /** Called with the cumulative bytes written so far. */
  onProgress(downloaded: number): void;
}

/**
 * The network transport seam: stream `url` into the file at `dest`, reporting size/progress via
 * `cb`. Resolves with the number of bytes written, or rejects (the mirror failed). The default
 * implementation is {@link realFetcher}; tests inject a fake.
 */
export type Fetcher = (url: string, dest: string, cb: FetchCallbacks) => Promise<number>;

/** `err.message` for an `Error`, else its string form. */
function errMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

/**
 * The real {@link Fetcher}: `fetch` (follows redirects — distros redirect heavily) streaming the
 * body to disk with backpressure and a per-chunk stall timeout. Bounded memory: chunks are
 * written straight to the file, never buffered whole.
 */
export const realFetcher: Fetcher = async (url, dest, cb) => {
  const controller = new AbortController();
  let stallTimer: NodeJS.Timeout | undefined;
  const armStall = (): void => {
    if (stallTimer) clearTimeout(stallTimer);
    stallTimer = setTimeout(
      () => controller.abort(new Error(`stalled (no data for ${STALL_TIMEOUT_MS / 1000}s)`)),
      STALL_TIMEOUT_MS,
    );
  };

  let response: Response;
  try {
    armStall();
    response = await fetch(url, { redirect: 'follow', signal: controller.signal });
  } catch (err) {
    if (stallTimer) clearTimeout(stallTimer);
    throw new DownloadError(`request to ${url} failed: ${errMessage(err)}`);
  }
  if (!response.ok || response.body === null) {
    if (stallTimer) clearTimeout(stallTimer);
    throw new DownloadError(
      `${url} returned an error status: ${response.status} ${response.statusText}`,
    );
  }
  // Guard against a portal/login/error page being saved as an ISO: a real image is never served
  // as text/html (e.g. an auth-gated distro returns its download page). Reject it so the mirror
  // fails cleanly rather than writing an HTML file under an .iso name.
  const contentType = response.headers.get('content-type') ?? '';
  if (contentType.trimStart().toLowerCase().startsWith('text/html')) {
    if (stallTimer) clearTimeout(stallTimer);
    throw new DownloadError(
      `${url} returned an HTML page (content-type ${contentType}), not a file — likely a ` +
        'login/error page, not the image',
    );
  }
  const lengthHeader = response.headers.get('content-length');
  const total =
    lengthHeader !== null && /^\d+$/.test(lengthHeader) ? Number(lengthHeader) : undefined;
  cb.onTotal(total);

  const file = createWriteStream(dest);
  let downloaded = 0;
  try {
    const nodeStream = Readable.fromWeb(response.body as Parameters<typeof Readable.fromWeb>[0]);
    for await (const chunk of nodeStream) {
      armStall();
      const buf = chunk as Uint8Array;
      downloaded += buf.length;
      if (!file.write(buf)) await once(file, 'drain');
      cb.onProgress(downloaded);
    }
    await new Promise<void>((resolve, reject) => {
      file.once('error', reject);
      file.end(() => resolve());
    });
  } catch (err) {
    file.destroy();
    throw new DownloadError(`download from ${url} failed: ${errMessage(err)}`);
  } finally {
    if (stallTimer) clearTimeout(stallTimer);
  }
  return downloaded;
};

/** The lifecycle state of a download (reported by `get_download`). */
export type DownloadState = 'downloading' | 'completed' | 'failed';

/** A snapshot of a single download's progress/outcome. Mirrors the Rust `DownloadStatus`. */
export interface DownloadStatus {
  /** The catalog id being downloaded. */
  id: string;
  /** The distro's human-facing name. */
  name: string;
  /** The bare name the image is saved as in the ISO Store. */
  filename: string;
  /** Lifecycle state. */
  state: DownloadState;
  /** Bytes written so far (of the current mirror attempt). */
  bytesDownloaded: number;
  /** Total size in bytes when the server advertised a `Content-Length`, else absent. */
  totalBytes?: number;
  /** Integer percent complete (0..100) when the total size is known, else absent. */
  percent?: number;
  /** The mirror URL currently being tried (or the one that succeeded/failed). */
  mirror?: string;
  /** Zero-based index of `mirror` within the entry's mirror list. */
  mirrorIndex: number;
  /** Total number of mirrors for the entry. */
  mirrorsTotal: number;
  /** The final ISO-Store path, set once the download completes. */
  path?: string;
  /** Why the download failed, set when `state` is `failed`. */
  error?: string;
}

/** The `get_download` report: the downloader capability + the current-or-last download. */
export interface DownloadReport {
  /** Whether downloading is enabled (`QMP_MCP_ALLOW_DOWNLOAD=true`). */
  available: boolean;
  /** Why downloading is unavailable, or a confirming note when it is. */
  reason: string;
  /** Where the catalog came from (`built-in` or a file path). */
  catalogSource: string;
  /** Number of entries in the catalog. */
  catalogCount: number;
  /** Whether a download is currently in progress. */
  active: boolean;
  /** The current (if active) or most recent download's status, if any. */
  download?: DownloadStatus;
  /** The configured ISO Store directory downloads land in. */
  isoDir: string;
}

/** Options for constructing a {@link DownloadManager}. */
export interface DownloadManagerOptions {
  /** The ISO Store directory downloads are written into. */
  isoDir: string;
  /** Whether downloading is enabled (`QMP_MCP_ALLOW_DOWNLOAD`). */
  allowDownload: boolean;
  /** The validated catalog. */
  catalog: Catalog;
  /** The network transport (defaults to {@link realFetcher}; tests inject a fake). */
  fetcher?: Fetcher;
}

/**
 * The ISO downloader. Holds the single download slot; every tool call drives the same instance
 * (the server keeps one). Mirrors the Rust `DownloadManager`.
 */
export class DownloadManager {
  private readonly isoDir: string;
  private readonly allowDownload: boolean;
  private readonly catalog: Catalog;
  private readonly fetcher: Fetcher;
  /** The in-flight download's live-mutated status, or undefined when idle. */
  private activeStatus?: DownloadStatus;
  /** The most recently finished (completed/failed) snapshot, for `get_download` when idle. */
  private lastStatus?: DownloadStatus;

  constructor(options: DownloadManagerOptions) {
    this.isoDir = options.isoDir;
    this.allowDownload = options.allowDownload;
    this.catalog = options.catalog;
    this.fetcher = options.fetcher ?? realFetcher;
  }

  /** The catalog, for the `list_iso_catalog` tool. */
  getCatalog(): Catalog {
    return this.catalog;
  }

  /** The `get_download` report: capability + the current-or-last download (a snapshot copy). */
  describe(): DownloadReport {
    const current = this.activeStatus ?? this.lastStatus;
    const [available, reason]: [boolean, string] = this.allowDownload
      ? [
          true,
          `Downloading is available (${this.catalog.entries.length} entries in the catalog from ${this.catalog.source}).`,
        ]
      : [
          false,
          'Downloading is disabled. Set QMP_MCP_ALLOW_DOWNLOAD=true to enable download_iso (it ' +
            'fetches an OS image over the network into the ISO Store).',
        ];
    return {
      available,
      reason,
      catalogSource: this.catalog.source,
      catalogCount: this.catalog.entries.length,
      active: this.activeStatus !== undefined,
      download: current === undefined ? undefined : { ...current },
      isoDir: this.isoDir,
    };
  }

  /**
   * Start downloading the catalog entry with the given `id` into the ISO Store, returning the
   * initial (downloading) status immediately. The actual fetch runs in the background; poll
   * `get_download` for progress. Fails closed when downloading is disabled, the id is unknown,
   * the target file already exists (append-only), or a download is already in progress.
   */
  async start(id: string): Promise<DownloadStatus> {
    if (!this.allowDownload) {
      throw new DownloadError(
        'Downloading is disabled. Set QMP_MCP_ALLOW_DOWNLOAD=true to enable download_iso (it ' +
          'fetches an OS image over the network into the ISO Store).',
      );
    }
    const entry = findEntry(this.catalog, id);
    if (entry === undefined) {
      throw new DownloadError(
        `unknown ISO id "${id}". Use list_iso_catalog to see the available ids (${idsHint(this.catalog)}).`,
      );
    }
    // Resolve (and validate) the target inside the ISO Store: fails closed when the Store dir does
    // not exist, and rejects symlink escape. Throws DownloadError via ISO_LABELS.error.
    const dest = resolveInStore(entry.filename, this.isoDir, ISO_LABELS);
    // Append-only (ADR-0018 relaxes ADR-0006 to "add new media", never overwrite existing).
    let exists = true;
    try {
      await stat(dest);
    } catch {
      exists = false;
    }
    if (exists) {
      throw new DownloadError(
        `"${entry.filename}" is already present in the ISO Store; downloading would overwrite it. ` +
          'Delete it first, or it is already available (see list_isos).',
      );
    }
    // Claim the single slot. The check-and-set below has NO await between them, so two concurrent
    // starts can never both pass it (the double-start guard is race-safe on the single event loop).
    if (this.activeStatus !== undefined) {
      throw new DownloadError(
        `a download is already in progress ("${this.activeStatus.filename}", id "${this.activeStatus.id}"). ` +
          'Only one download runs at a time — wait for it to finish (poll get_download) or let it fail.',
      );
    }
    const status: DownloadStatus = {
      id: entry.id,
      name: entry.name,
      filename: entry.filename,
      state: 'downloading',
      bytesDownloaded: 0,
      mirror: entry.mirrors[0],
      mirrorIndex: 0,
      mirrorsTotal: entry.mirrors.length,
    };
    this.activeStatus = status;

    logger.info(`download started: ${entry.id} -> ${dest} (${entry.mirrors.length} mirror(s))`);
    // Run the fetch off the request path (fire-and-forget; progress is polled via get_download).
    void this.run(entry.mirrors, dest, status);
    return { ...status };
  }

  /**
   * The background download loop: try each mirror in turn, streaming into the temp file; on the
   * first success atomically rename it onto the final path and mark `completed`; if every mirror
   * fails, remove the temp file and mark `failed`. Always releases the slot at the end.
   */
  private async run(mirrors: string[], dest: string, status: DownloadStatus): Promise<void> {
    const temp = `${dest}.part`;
    await rm(temp, { force: true }).catch(() => {});
    let lastError = 'no mirrors were configured for this entry';
    for (const [index, url] of mirrors.entries()) {
      logger.debug(`trying mirror ${index + 1}/${mirrors.length}: ${url}`);
      status.mirror = url;
      status.mirrorIndex = index;
      status.bytesDownloaded = 0;
      status.totalBytes = undefined;
      status.percent = undefined;
      try {
        await this.fetcher(url, temp, {
          onTotal: (t) => {
            status.totalBytes = t;
          },
          onProgress: (n) => {
            status.bytesDownloaded = n;
            if (status.totalBytes !== undefined && status.totalBytes > 0) {
              status.percent = Math.floor(
                (Math.min(n, status.totalBytes) * 100) / status.totalBytes,
              );
            }
          },
        });
        try {
          await rename(temp, dest);
        } catch (err) {
          await rm(temp, { force: true }).catch(() => {});
          lastError = `failed to finalize ${dest}: ${errMessage(err)}`;
          break;
        }
        status.state = 'completed';
        status.path = dest;
        if (status.totalBytes !== undefined) status.percent = 100;
        logger.info(`download complete: ${status.id} -> ${dest}`);
        this.finish(status);
        return;
      } catch (err) {
        await rm(temp, { force: true }).catch(() => {});
        lastError = errMessage(err);
        logger.warning(`download mirror failed (${url}): ${lastError}`);
      }
    }
    logger.warning(`download failed: ${status.id}: ${lastError}`);
    status.state = 'failed';
    status.error = lastError;
    this.finish(status);
  }

  /** Release the download slot: snapshot the terminal status into `last` and clear `active`. */
  private finish(status: DownloadStatus): void {
    this.lastStatus = { ...status };
    this.activeStatus = undefined;
  }
}

/**
 * The process-wide downloader singleton, built once from the environment (memoized so the single
 * download slot's state persists across tool calls, like the Orchestrator singleton). The catalog
 * is loaded lazily here — a bad `QMP_MCP_ISO_CATALOG` surfaces as an actionable tool error rather
 * than crashing tool discovery. Mirrors how the tools reach `isoStoreFromEnv`.
 */
let singleton: DownloadManager | undefined;
export function downloadManagerFromEnv(env: NodeJS.ProcessEnv = process.env): DownloadManager {
  if (singleton === undefined) {
    singleton = new DownloadManager({
      isoDir: resolveIsoDir(env),
      allowDownload: resolveAllowDownload(env),
      catalog: loadCatalog(resolveIsoCatalog(env)),
      fetcher: realFetcher,
    });
  }
  return singleton;
}
