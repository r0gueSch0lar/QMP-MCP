import { mkdtemp, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, sep } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { assertValidStoreName, resolveInStore, type StoreLabels } from './store-path.js';

/** A throwaway store flavour, to exercise the generic boundary directly. */
class FakeStoreError extends Error {}
const LABELS: StoreLabels = {
  store: 'Test Store',
  entry: 'Widget',
  envVar: 'TEST_STORE_DIR',
  error: FakeStoreError,
};

describe('resolveInStore (the one shared boundary used by both stores)', () => {
  let store: string;

  beforeEach(async () => {
    store = await mkdtemp(join(tmpdir(), 'store-path-'));
  });

  afterEach(async () => {
    await rm(store, { recursive: true, force: true });
  });

  it('maps a valid bare name to a path inside the store', async () => {
    await writeFile(join(store, 'thing'), '');
    const path = resolveInStore('thing', store, LABELS);
    expect(path).toBe(join(store, 'thing'));
    expect(path.startsWith(store + sep)).toBe(true);
  });

  it('throws the caller-supplied error type, with store-specific wording', () => {
    // The error class and the labels are what each store specialises; the logic
    // is identical. This is what lets the Image/ISO stores keep distinct error
    // types over one implementation.
    expect(() => resolveInStore('/abs', store, LABELS)).toThrowError(FakeStoreError);
    expect(() => resolveInStore('..', store, LABELS)).toThrowError(/Widget name/);
    expect(() => resolveInStore('thing', join(store, 'gone'), LABELS)).toThrowError(
      /TEST_STORE_DIR/,
    );
  });

  it('rejects absolute, traversal, separator, and injection names', () => {
    expect(() => resolveInStore('/etc/passwd', store, LABELS)).toThrowError(/absolute/);
    expect(() => resolveInStore('..', store, LABELS)).toThrowError(/valid file name/);
    expect(() => resolveInStore('a/b', store, LABELS)).toThrowError(/separator/);
    expect(() => resolveInStore('x,y=z', store, LABELS)).toThrowError(/inject|must match/);
  });

  it('rejects a symlink that escapes the store', async () => {
    await symlink('/etc/passwd', join(store, 'escape'));
    expect(() => resolveInStore('escape', store, LABELS)).toThrowError(/symlink escape/);
  });
});

describe('assertValidStoreName', () => {
  it('accepts a single safe segment and rejects option-injection characters', () => {
    expect(() => assertValidStoreName('debian-12.iso', LABELS)).not.toThrow();
    expect(() => assertValidStoreName('a,b', LABELS)).toThrowError(FakeStoreError);
    expect(() => assertValidStoreName('-leading', LABELS)).toThrowError(/must match|inject/);
  });
});
