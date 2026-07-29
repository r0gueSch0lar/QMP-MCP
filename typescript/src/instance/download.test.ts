import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { parseCatalog } from './catalog.js';
import { DownloadManager, type DownloadStatus, type Fetcher } from './download.js';

const CATALOG_JSON =
  '{"isos":[{"id":"one","name":"One","filename":"one.iso",' +
  '"mirrors":["https://a/one.iso","https://b/one.iso"]}]}';

/** A fake {@link Fetcher} driven by an exact url→outcome map. `bytes` writes them; `error` throws. */
function fakeFetcher(
  outcomes: Record<string, { bytes?: string; error?: string }>,
  gate?: Promise<void>,
): Fetcher {
  return async (url, dest, cb) => {
    if (gate) await gate;
    const outcome = outcomes[url];
    if (outcome === undefined || outcome.error !== undefined) {
      throw new Error(outcome?.error ?? `no fake outcome for ${url}`);
    }
    const buf = Buffer.from(outcome.bytes ?? '');
    cb.onTotal(buf.length);
    writeFileSync(dest, buf);
    cb.onProgress(buf.length);
    return buf.length;
  };
}

/** Poll `describe()` until the download reaches a terminal state (bounded). */
async function awaitTerminal(mgr: DownloadManager): Promise<DownloadStatus> {
  for (let i = 0; i < 200; i++) {
    const download = mgr.describe().download;
    if (download !== undefined && download.state !== 'downloading') return download;
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  throw new Error('download did not reach a terminal state');
}

describe('ISO download manager (ADR-0018)', () => {
  let isoDir: string;

  beforeEach(() => {
    isoDir = mkdtempSync(join(tmpdir(), 'qmp-dl-'));
  });
  afterEach(() => {
    rmSync(isoDir, { recursive: true, force: true });
  });

  function manager(allowDownload: boolean, fetcher: Fetcher): DownloadManager {
    return new DownloadManager({
      isoDir,
      allowDownload,
      catalog: parseCatalog(CATALOG_JSON, 'test'),
      fetcher,
    });
  }

  it('fails closed naming the gate when downloading is disabled', async () => {
    const mgr = manager(false, fakeFetcher({}));
    await expect(mgr.start('one')).rejects.toThrow(/QMP_MCP_ALLOW_DOWNLOAD/);
    expect(mgr.describe().available).toBe(false);
  });

  it('rejects an unknown id with the available ids', async () => {
    const mgr = manager(true, fakeFetcher({}));
    await expect(mgr.start('nope')).rejects.toThrow(/unknown ISO id.*one/s);
  });

  it('does not overwrite an existing ISO', async () => {
    writeFileSync(join(isoDir, 'one.iso'), 'existing');
    const mgr = manager(true, fakeFetcher({ 'https://a/one.iso': { bytes: 'new' } }));
    await expect(mgr.start('one')).rejects.toThrow(/already present/);
  });

  it('downloads the first working mirror and renames atomically', async () => {
    const mgr = manager(true, fakeFetcher({ 'https://a/one.iso': { bytes: 'ISOBYTES' } }));
    const initial = await mgr.start('one');
    expect(initial.state).toBe('downloading');
    const done = await awaitTerminal(mgr);
    expect(done.state).toBe('completed');
    expect(done.bytesDownloaded).toBe(8);
    expect(done.percent).toBe(100);
    expect(readFileSync(join(isoDir, 'one.iso'), 'utf8')).toBe('ISOBYTES');
    expect(existsSync(join(isoDir, 'one.iso.part'))).toBe(false);
  });

  it('falls over to the second mirror when the first fails', async () => {
    const mgr = manager(
      true,
      fakeFetcher({
        'https://a/one.iso': { error: '503 from a' },
        'https://b/one.iso': { bytes: 'FROM_B' },
      }),
    );
    await mgr.start('one');
    const done = await awaitTerminal(mgr);
    expect(done.state).toBe('completed');
    expect(done.mirrorIndex).toBe(1);
    expect(readFileSync(join(isoDir, 'one.iso'), 'utf8')).toBe('FROM_B');
  });

  it('marks failed and leaves no partial when every mirror fails', async () => {
    const mgr = manager(
      true,
      fakeFetcher({
        'https://a/one.iso': { error: 'boom a' },
        'https://b/one.iso': { error: 'boom b' },
      }),
    );
    await mgr.start('one');
    const done = await awaitTerminal(mgr);
    expect(done.state).toBe('failed');
    expect(done.error).toContain('boom b');
    expect(existsSync(join(isoDir, 'one.iso'))).toBe(false);
    expect(existsSync(join(isoDir, 'one.iso.part'))).toBe(false);
  });

  it('rejects a second download while one is in progress', async () => {
    let release: () => void = () => {};
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    const mgr = manager(true, fakeFetcher({ 'https://a/one.iso': { bytes: 'X' } }, gate));
    await mgr.start('one');
    expect(mgr.describe().active).toBe(true);
    await expect(mgr.start('one')).rejects.toThrow(/already in progress/);
    release();
    const done = await awaitTerminal(mgr);
    expect(done.state).toBe('completed');
    expect(mgr.describe().active).toBe(false);
  });
});
