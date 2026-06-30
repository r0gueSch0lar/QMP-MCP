import { existsSync } from 'node:fs';
import { mkdtemp, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, sep } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { assertValidImageName, resolveImagePath } from './image-store.js';
import {
  assertValidIsoName,
  IsoStore,
  IsoStoreError,
  isoStoreFromEnv,
  resolveIsoPath,
} from './iso-store.js';

describe('resolveIsoPath (containment boundary, read-only ISO Store)', () => {
  let store: string;

  beforeEach(async () => {
    store = await mkdtemp(join(tmpdir(), 'iso-store-'));
  });

  afterEach(async () => {
    await rm(store, { recursive: true, force: true });
  });

  it('maps a valid bare name (debian.iso) to a path inside the Store', async () => {
    await writeFile(join(store, 'debian.iso'), '');
    const path = resolveIsoPath('debian.iso', store);
    expect(path.startsWith(store + sep)).toBe(true);
    expect(path).toBe(join(store, 'debian.iso'));
  });

  it('rejects an absolute path', () => {
    expect(() => resolveIsoPath('/etc/passwd', store)).toThrowError(IsoStoreError);
    expect(() => resolveIsoPath('/etc/passwd', store)).toThrowError(/absolute/);
  });

  it('rejects a `..` traversal name', () => {
    expect(() => resolveIsoPath('..', store)).toThrowError(/valid file name/);
    expect(() => resolveIsoPath('../../etc/passwd', store)).toThrowError(/separator/);
  });

  it('rejects a nested (subdirectory) name — the Store is flat', () => {
    expect(() => resolveIsoPath('sub/debian.iso', store)).toThrowError(/separator|subdirector/);
  });

  it('rejects a name carrying a comma or `=` (would inject a -drive property)', () => {
    expect(() => resolveIsoPath('debian.iso,media=disk', store)).toThrowError(/inject|must match/);
    expect(() => resolveIsoPath('boot=evil', store)).toThrowError(/inject|must match/);
  });

  it('rejects an empty name and a NUL byte', () => {
    expect(() => resolveIsoPath('', store)).toThrowError(/non-empty/);
    expect(() => resolveIsoPath('a\0b', store)).toThrowError(/NUL/);
  });

  it('rejects a symlink in the Store that points OUTSIDE it', async () => {
    await symlink('/etc/passwd', join(store, 'escape.iso'));
    expect(() => resolveIsoPath('escape.iso', store)).toThrowError(/symlink escape/);
  });

  it('rejects a dangling symlink rather than following it', async () => {
    await symlink(join(store, 'does-not-exist'), join(store, 'dangling.iso'));
    expect(() => resolveIsoPath('dangling.iso', store)).toThrowError(/dangling symlink/);
  });

  it('fails closed naming QMP_MCP_ISO_DIR when the Store directory is missing', () => {
    expect(() => resolveIsoPath('debian.iso', join(store, 'nope'))).toThrowError(/QMP_MCP_ISO_DIR/);
  });
});

describe('ISO and Image resolvers share ONE implementation (reuse-proof)', () => {
  // Both resolveImagePath and resolveIsoPath are thin wrappers over the single
  // shared resolveInStore boundary (store-path.ts). The duplicated-security-code
  // drift risk is closed by construction; this battery proves the two stores
  // accept/reject the SAME inputs (only the store-specific wording differs).
  let store: string;

  beforeEach(async () => {
    store = await mkdtemp(join(tmpdir(), 'store-reuse-'));
  });

  afterEach(async () => {
    await rm(store, { recursive: true, force: true });
  });

  const traversalCases = ['/etc/passwd', '..', '../escape', 'a/b', 'x,y=z', '', 'a\0b'];

  for (const bad of traversalCases) {
    it(`rejects ${JSON.stringify(bad)} in BOTH stores`, () => {
      expect(() => resolveImagePath(bad, store)).toThrow();
      expect(() => resolveIsoPath(bad, store)).toThrow();
    });
  }

  it('accepts the same valid bare name in BOTH stores, each rooted in its own dir', async () => {
    await writeFile(join(store, 'media.iso'), '');
    expect(resolveImagePath('media.iso', store)).toBe(join(store, 'media.iso'));
    expect(resolveIsoPath('media.iso', store)).toBe(join(store, 'media.iso'));
  });

  it('applies the same name allowlist in both assertValid* helpers', () => {
    expect(() => assertValidImageName('ok-name.iso')).not.toThrow();
    expect(() => assertValidIsoName('ok-name.iso')).not.toThrow();
    expect(() => assertValidImageName('bad,name')).toThrow();
    expect(() => assertValidIsoName('bad,name')).toThrow();
  });
});

describe('IsoStore.list (read-only)', () => {
  let store: string;

  beforeEach(async () => {
    store = await mkdtemp(join(tmpdir(), 'iso-list-'));
  });

  afterEach(async () => {
    await rm(store, { recursive: true, force: true });
  });

  it('lists regular files and skips symlinks (never follows them)', async () => {
    await writeFile(join(store, 'a.iso'), 'x');
    await writeFile(join(store, 'b.iso'), 'yy');
    await symlink('/etc/passwd', join(store, 'evil'));
    const sut = new IsoStore({ dir: store });
    const { isos } = await sut.list();
    expect(isos.map((i) => i.name)).toEqual(['a.iso', 'b.iso']);
  });

  it('fails closed naming QMP_MCP_ISO_DIR when the Store directory is missing', async () => {
    const sut = new IsoStore({ dir: join(store, 'absent') });
    await expect(sut.list()).rejects.toThrowError(/QMP_MCP_ISO_DIR/);
  });

  it('never creates the Store directory (read-only: no mkdir)', async () => {
    const absent = join(store, 'absent');
    const sut = new IsoStore({ dir: absent });
    await expect(sut.list()).rejects.toThrow();
    // Listing a missing ISO Store must not have created it.
    expect(existsSync(absent)).toBe(false);
  });
});

describe('isoStoreFromEnv', () => {
  it('reads the ISO Store dir from the environment', () => {
    const sut = isoStoreFromEnv({ QMP_MCP_ISO_DIR: '/srv/isos' });
    expect(sut.dir).toBe('/srv/isos');
  });

  it('has no create/write surface (read-only store)', () => {
    const sut = isoStoreFromEnv({ QMP_MCP_ISO_DIR: '/srv/isos' });
    expect((sut as unknown as { create?: unknown }).create).toBeUndefined();
  });
});
