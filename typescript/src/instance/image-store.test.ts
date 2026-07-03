import { accessSync, constants, existsSync } from 'node:fs';
import { mkdtemp, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { delimiter, join, sep } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import {
  assertValidImageName,
  type ImageFormat,
  ImageStore,
  ImageStoreError,
  imageStoreFromEnv,
  resolveImagePath,
} from './image-store.js';

/** True when `qemu-img` is resolvable on PATH (executable). */
function qemuImgOnPath(): boolean {
  for (const dir of (process.env.PATH ?? '').split(delimiter)) {
    if (!dir) continue;
    try {
      accessSync(join(dir, 'qemu-img'), constants.X_OK);
      return true;
    } catch {
      // try the next PATH entry
    }
  }
  return false;
}

describe('resolveImagePath (containment boundary)', () => {
  let store: string;

  beforeEach(async () => {
    store = await mkdtemp(join(tmpdir(), 'img-store-'));
  });

  afterEach(async () => {
    await rm(store, { recursive: true, force: true });
  });

  it('maps a valid bare name to a path inside the Store', async () => {
    await writeFile(join(store, 'root.qcow2'), '');
    const path = resolveImagePath('root.qcow2', store);
    expect(path.startsWith(store + sep)).toBe(true);
    expect(path).toBe(join(store, 'root.qcow2'));
  });

  it('resolves a not-yet-existing name (for create) safely inside the Store', () => {
    const path = resolveImagePath('new.qcow2', store);
    expect(path).toBe(join(store, 'new.qcow2'));
  });

  it('rejects an absolute path', () => {
    expect(() => resolveImagePath('/etc/passwd', store)).toThrowError(ImageStoreError);
    expect(() => resolveImagePath('/etc/passwd', store)).toThrowError(/absolute/);
  });

  it('rejects a `..` traversal name', () => {
    expect(() => resolveImagePath('..', store)).toThrowError(/valid file name/);
    expect(() => resolveImagePath('../../etc/passwd', store)).toThrowError(/separator/);
  });

  it('rejects a nested (subdirectory) name — the Store is flat', () => {
    expect(() => resolveImagePath('sub/disk.qcow2', store)).toThrowError(/separator|subdirector/);
  });

  it('rejects an empty name and a NUL byte', () => {
    expect(() => resolveImagePath('', store)).toThrowError(/non-empty/);
    expect(() => resolveImagePath('a\0b', store)).toThrowError(/NUL/);
  });

  it('rejects a symlink in the Store that points OUTSIDE it', async () => {
    await symlink('/etc/passwd', join(store, 'escape'));
    expect(() => resolveImagePath('escape', store)).toThrowError(/symlink escape/);
  });

  it('rejects a dangling symlink rather than following it', async () => {
    await symlink(join(store, 'does-not-exist'), join(store, 'dangling'));
    expect(() => resolveImagePath('dangling', store)).toThrowError(/dangling symlink/);
  });

  it('allows a symlink that stays INSIDE the Store (non-escaping)', async () => {
    await writeFile(join(store, 'real.qcow2'), '');
    await symlink(join(store, 'real.qcow2'), join(store, 'alias.qcow2'));
    expect(resolveImagePath('alias.qcow2', store)).toBe(join(store, 'alias.qcow2'));
  });

  it('fails closed with an actionable message when the Store directory is missing', () => {
    expect(() => resolveImagePath('disk.qcow2', join(store, 'nope'))).toThrowError(
      /QMP_MCP_IMAGE_DIR/,
    );
  });
});

describe('assertValidImageName (QemuOpts option-injection allowlist)', () => {
  it('accepts an ordinary single-segment image name', () => {
    expect(() => assertValidImageName('debian12.qcow2')).not.toThrow();
    expect(() => assertValidImageName('data_disk-01.raw')).not.toThrow();
  });

  it('rejects a name containing a comma (would inject a -drive property)', () => {
    expect(() => assertValidImageName('disk,readonly=off')).toThrowError(ImageStoreError);
    // The message names the rule so the caller can fix the name.
    expect(() => assertValidImageName('disk,readonly=off')).toThrowError(/inject|must match/);
  });

  it('rejects an "=", a space, and a leading hyphen', () => {
    expect(() => assertValidImageName('disk=evil')).toThrowError(/must match|inject/);
    expect(() => assertValidImageName('disk readonly')).toThrowError(/must match|inject/);
    expect(() => assertValidImageName('-drive')).toThrowError(/must match|inject/);
    expect(() => assertValidImageName('foo:bar')).toThrowError(/must match|inject/);
  });
});

describe('ImageStore.create (validation)', () => {
  let store: string;

  beforeEach(async () => {
    store = await mkdtemp(join(tmpdir(), 'img-create-'));
  });

  afterEach(async () => {
    await rm(store, { recursive: true, force: true });
  });

  it('rejects an over-cap size, naming QMP_MCP_MAX_DISK_GB and the values', async () => {
    const sut = new ImageStore({ dir: store, maxDiskGb: 8, run: async () => undefined });
    await expect(
      sut.create({ name: 'big.qcow2', sizeGb: 16, format: 'qcow2' }),
    ).rejects.toThrowError(/QMP_MCP_MAX_DISK_GB.*16.*8|16 GiB exceeds the maximum allowed 8 GiB/);
  });

  it('rejects an unsupported format before spawning anything', async () => {
    let spawned = false;
    const sut = new ImageStore({
      dir: store,
      maxDiskGb: 64,
      run: async () => {
        spawned = true;
      },
    });
    await expect(
      sut.create({ name: 'd.vmdk', sizeGb: 1, format: 'vmdk' as ImageFormat }),
    ).rejects.toThrowError(/Unsupported image format/);
    expect(spawned).toBe(false);
  });

  it('rejects an escaping name before spawning anything', async () => {
    let spawned = false;
    const sut = new ImageStore({
      dir: store,
      maxDiskGb: 64,
      run: async () => {
        spawned = true;
      },
    });
    await expect(
      sut.create({ name: '../evil.qcow2', sizeGb: 1, format: 'qcow2' }),
    ).rejects.toThrowError(ImageStoreError);
    expect(spawned).toBe(false);
  });

  it('refuses to clobber an existing image', async () => {
    await writeFile(join(store, 'taken.qcow2'), '');
    const sut = new ImageStore({ dir: store, maxDiskGb: 64, run: async () => undefined });
    await expect(
      sut.create({ name: 'taken.qcow2', sizeGb: 1, format: 'qcow2' }),
    ).rejects.toThrowError(/already exists/);
  });

  it('invokes qemu-img with an explicit -f format and the resolved in-store path', async () => {
    const calls: Array<{ bin: string; args: string[] }> = [];
    const sut = new ImageStore({
      dir: store,
      maxDiskGb: 64,
      run: async (bin, args) => {
        calls.push({ bin, args });
      },
    });
    const result = await sut.create({ name: 'root.qcow2', sizeGb: 4, format: 'qcow2' });
    expect(result.path).toBe(join(store, 'root.qcow2'));
    expect(calls).toHaveLength(1);
    expect(calls[0]?.args).toEqual(['create', '-f', 'qcow2', join(store, 'root.qcow2'), '4G']);
  });
});

describe.skipIf(!qemuImgOnPath())('ImageStore.create (real qemu-img)', () => {
  let store: string;

  beforeEach(async () => {
    store = await mkdtemp(join(tmpdir(), 'img-real-'));
  });

  afterEach(async () => {
    await rm(store, { recursive: true, force: true });
  });

  it('creates a real qcow2 image that exists on disk and is listable', async () => {
    const sut = new ImageStore({ dir: store, maxDiskGb: 64 });
    const result = await sut.create({ name: 'root.qcow2', sizeGb: 1, format: 'qcow2' });
    expect(existsSync(result.path)).toBe(true);

    const listed = await sut.list();
    expect(listed.images.map((i) => i.name)).toContain('root.qcow2');
  });

  it('creates a real raw image that exists on disk', async () => {
    const sut = new ImageStore({ dir: store, maxDiskGb: 64 });
    const result = await sut.create({ name: 'data.img', sizeGb: 1, format: 'raw' });
    expect(existsSync(result.path)).toBe(true);
    expect(result.format).toBe('raw');
  });
});

describe('ImageStore.list', () => {
  let store: string;

  beforeEach(async () => {
    store = await mkdtemp(join(tmpdir(), 'img-list-'));
  });

  afterEach(async () => {
    await rm(store, { recursive: true, force: true });
  });

  it('lists regular files and skips symlinks (never follows them)', async () => {
    await writeFile(join(store, 'a.qcow2'), 'x');
    await writeFile(join(store, 'b.raw'), 'yy');
    await symlink('/etc/passwd', join(store, 'evil'));
    const sut = new ImageStore({ dir: store, maxDiskGb: 64 });
    const { images } = await sut.list();
    expect(images.map((i) => i.name)).toEqual(['a.qcow2', 'b.raw']);
  });

  it('fails closed when the Store directory is missing', async () => {
    const sut = new ImageStore({ dir: join(store, 'absent'), maxDiskGb: 64 });
    await expect(sut.list()).rejects.toThrowError(/QMP_MCP_IMAGE_DIR/);
  });
});

describe('imageStoreFromEnv', () => {
  it('reads the Image Store dir and size cap from the environment', () => {
    const sut = imageStoreFromEnv({ QMP_MCP_IMAGE_DIR: '/srv/imgs', QMP_MCP_MAX_DISK_GB: '32' });
    expect(sut.dir).toBe('/srv/imgs');
    expect(sut.maxDiskGb).toBe(32);
  });
});
