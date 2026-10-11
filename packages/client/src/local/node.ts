// The Node disk backend of the local store (spec/zendb.md §9.3): one
// directory per store, one file per record. A write goes to a temporary
// file, is synced, then renamed over the record, so a crash leaves the old
// record or the new one, never half of one. The device key is
// `dir/device.key`, readable by its owner only.
import { randomBytes } from 'node:crypto';
import { mkdir, open, readdir, readFile, rename, rm, unlink } from 'node:fs/promises';
import { join } from 'node:path';
import { fromHex, hex } from '../bytes.js';
import { checkStore, type LocalStore } from './store.js';

const KEY_FILE = 'device.key';
const STORES = 'stores';

const missing = (e: unknown) => (e as { code?: string }).code === 'ENOENT';

/** Write `data` to `path` so a crash never leaves it half written. */
async function writeAtomic(path: string, data: Uint8Array, mode = 0o600): Promise<void> {
  const tmp = `${path}.${randomBytes(6).toString('hex')}.tmp`;
  const f = await open(tmp, 'w', mode);
  try {
    await f.writeFile(data);
    await f.sync();
  } finally {
    await f.close();
  }
  await rename(tmp, path);
}

/** A local store in directory `dir` (made if missing). */
export function nodeDiskLocalStore(dir: string): LocalStore {
  const storeDir = (store: string) => {
    checkStore(store);
    return join(dir, STORES, encodeURIComponent(store));
  };
  const made = new Set<string>();
  const ensure = async (d: string) => {
    if (made.has(d)) return;
    await mkdir(d, { recursive: true, mode: 0o700 });
    made.add(d);
  };
  let device: Promise<CryptoKey> | undefined;
  const importKey = (raw: Uint8Array) =>
    crypto.subtle.importKey('raw', raw as BufferSource, 'AES-GCM', false, ['encrypt', 'decrypt']);

  return {
    async get(store, key) {
      try {
        return new Uint8Array(await readFile(join(storeDir(store), hex(key))));
      } catch (e) {
        if (missing(e)) return undefined;
        throw e;
      }
    },
    async put(store, key, value) {
      const d = storeDir(store);
      await ensure(d);
      await writeAtomic(join(d, hex(key)), value);
    },
    async delete(store, keys) {
      const d = storeDir(store);
      for (const k of keys)
        await unlink(join(d, hex(k))).catch((e) => missing(e) || Promise.reject(e));
    },
    async *list(store) {
      const d = storeDir(store);
      let names: string[];
      try {
        names = await readdir(d);
      } catch (e) {
        if (missing(e)) return;
        throw e;
      }
      for (const n of names) {
        if (n.endsWith('.tmp')) continue; // a write a crash interrupted
        try {
          yield [fromHex(n), new Uint8Array(await readFile(join(d, n)))];
        } catch (e) {
          if (!missing(e)) throw e;
        }
      }
    },
    async clear(store) {
      const d = store === undefined ? join(dir, STORES) : storeDir(store);
      await rm(d, { recursive: true, force: true });
      made.clear();
    },
    deviceKey() {
      device ??= (async () => {
        const path = join(dir, KEY_FILE);
        try {
          const raw = new Uint8Array(await readFile(path));
          if (raw.length === 32) return importKey(raw);
        } catch (e) {
          if (!missing(e)) throw e;
        }
        await mkdir(dir, { recursive: true, mode: 0o700 });
        const raw = new Uint8Array(randomBytes(32));
        await writeAtomic(path, raw, 0o600);
        // Another process may have written one meanwhile: use what is there.
        const kept = new Uint8Array(await readFile(path));
        const k = await importKey(kept);
        raw.fill(0);
        kept.fill(0);
        return k;
      })();
      device.catch(() => {
        device = undefined;
      });
      return device;
    },
    close() {},
  };
}
