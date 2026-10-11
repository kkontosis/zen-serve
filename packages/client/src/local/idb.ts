// The IndexedDB backend of the local store (spec/zendb.md §9.3). One
// database per app: records in one object store, keyed by
// `[store, hex(key)]`, so a new store needs no version change (which would
// wait for every other tab to close its connection); the device key, a
// non-extractable CryptoKey, in another.
import { fromHex, hex } from '../bytes.js';
import { checkStore, type LocalStore, newDeviceKey } from './store.js';

const RECORDS = 'records';
const KEYS = 'keys';
const DEVICE = 'device';

const req = <T>(r: IDBRequest<T>): Promise<T> =>
  new Promise((ok, fail) => {
    r.onsuccess = () => ok(r.result);
    r.onerror = () => fail(r.error);
  });

const done = (t: IDBTransaction): Promise<void> =>
  new Promise((ok, fail) => {
    t.oncomplete = () => ok();
    t.onerror = () => fail(t.error);
    t.onabort = () => fail(t.error ?? new Error('IndexedDB transaction aborted'));
  });

/**
 * A local store in IndexedDB database `name`, shared by the tabs of one
 * origin. `factory` and `keyRange` default to the globals `indexedDB` and
 * `IDBKeyRange` (tests pass fake-indexeddb's).
 */
export function indexedDbLocalStore(
  name: string,
  opts: { factory?: IDBFactory; keyRange?: typeof IDBKeyRange } = {},
): LocalStore {
  const g = globalThis as { indexedDB?: IDBFactory; IDBKeyRange?: typeof IDBKeyRange };
  const factory = opts.factory ?? g.indexedDB;
  const KeyRange = opts.keyRange ?? g.IDBKeyRange;
  if (!factory || !KeyRange) throw new TypeError('IndexedDB is not available here');
  /** The records of one store: `[store, ""]` up to `[store, []]` (arrays sort after strings). */
  const rangeOf = (store: string) => KeyRange.bound([store, ''], [store, []]);
  let opened: Promise<IDBDatabase> | undefined;
  const open = () => {
    opened ??= new Promise<IDBDatabase>((ok, fail) => {
      const r = factory.open(name, 1);
      r.onupgradeneeded = () => {
        r.result.createObjectStore(RECORDS);
        r.result.createObjectStore(KEYS);
      };
      r.onsuccess = () => {
        // A later version (another build) asks to upgrade: let it.
        r.result.onversionchange = () => {
          r.result.close();
          opened = undefined;
        };
        ok(r.result);
      };
      r.onerror = () => fail(r.error);
    });
    return opened;
  };
  const tx = async (stores: string[], mode: IDBTransactionMode) =>
    (await open()).transaction(stores, mode);
  let device: Promise<CryptoKey> | undefined;

  return {
    async get(store, key) {
      checkStore(store);
      const t = await tx([RECORDS], 'readonly');
      const v = await req(t.objectStore(RECORDS).get([store, hex(key)]));
      return v instanceof Uint8Array ? v : undefined;
    },
    async put(store, key, value) {
      checkStore(store);
      const t = await tx([RECORDS], 'readwrite');
      t.objectStore(RECORDS).put(value.slice(), [store, hex(key)]);
      await done(t);
    },
    async delete(store, keys) {
      checkStore(store);
      if (!keys.length) return;
      const t = await tx([RECORDS], 'readwrite');
      const os = t.objectStore(RECORDS);
      for (const k of keys) os.delete([store, hex(k)]);
      await done(t);
    },
    async *list(store) {
      checkStore(store);
      const t = await tx([RECORDS], 'readonly');
      const os = t.objectStore(RECORDS);
      const range = rangeOf(store);
      const [ks, vs] = await Promise.all([req(os.getAllKeys(range)), req(os.getAll(range))]);
      for (let i = 0; i < ks.length; i++) {
        const v = vs[i];
        if (v instanceof Uint8Array) yield [fromHex((ks[i] as [string, string])[1]), v];
      }
    },
    async clear(store) {
      const t = await tx([RECORDS], 'readwrite');
      const os = t.objectStore(RECORDS);
      if (store === undefined) os.clear();
      else {
        checkStore(store);
        os.delete(rangeOf(store));
      }
      await done(t);
    },
    deviceKey() {
      device ??= (async () => {
        const t = await tx([KEYS], 'readonly');
        const have = await req(t.objectStore(KEYS).get(DEVICE));
        if (have) return have as CryptoKey;
        const k = await newDeviceKey();
        // Another tab may have made one meanwhile: the first one stays.
        const w = await tx([KEYS], 'readwrite');
        const os = w.objectStore(KEYS);
        const again = await req(os.get(DEVICE));
        if (again) return again as CryptoKey;
        os.put(k, DEVICE);
        await done(w);
        return k;
      })();
      device.catch(() => {
        device = undefined;
      });
      return device;
    },
    close() {
      const o = opened;
      opened = undefined;
      void o?.then((d) => d.close()).catch(() => {});
    },
  };
}
