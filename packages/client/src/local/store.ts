// The local store (spec/zendb.md §9.3): records kept on this device, in
// named stores, by byte keys. A `LocalStore` is the raw backend; a
// `SealedStore` (sealed.ts) seals what goes in.
import { fromHex, hex } from '../bytes.js';

/** A raw backend: bytes in, bytes out. Not sealed. */
export interface LocalStore {
  get(store: string, key: Uint8Array): Promise<Uint8Array | undefined>;
  put(store: string, key: Uint8Array, value: Uint8Array): Promise<void>;
  delete(store: string, keys: Uint8Array[]): Promise<void>;
  /** Every record of a store, in no particular order. */
  list(store: string): AsyncIterable<[Uint8Array, Uint8Array]>;
  /** Delete every record of a store, or of every store. */
  clear(store?: string): Promise<void>;
  /**
   * This device's key for the device layer (§9.3): made once and kept by
   * the backend, never extractable.
   */
  deviceKey(): Promise<CryptoKey>;
  close(): void;
}

/** A fresh, non-extractable AES-256-GCM key. */
export async function newDeviceKey(): Promise<CryptoKey> {
  return crypto.subtle.generateKey({ name: 'AES-GCM', length: 256 }, false, ['encrypt', 'decrypt']);
}

/** Store names are short and printable; keys are any bytes. */
export function checkStore(store: string): void {
  if (!store || store.length > 200 || [...store].some((c) => c.charCodeAt(0) < 0x20)) {
    throw new TypeError(`bad local store name: ${JSON.stringify(store)}`);
  }
}

/** A store in memory: it lasts as long as the process, and so does its device key. */
export function memoryLocalStore(): LocalStore {
  const stores = new Map<string, Map<string, Uint8Array>>();
  let key: Promise<CryptoKey> | undefined;
  const of = (s: string) => {
    checkStore(s);
    let m = stores.get(s);
    if (!m) {
      m = new Map();
      stores.set(s, m);
    }
    return m;
  };
  return {
    get: async (s, k) => of(s).get(hex(k))?.slice(),
    put: async (s, k, v) => {
      of(s).set(hex(k), v.slice());
    },
    delete: async (s, ks) => {
      const m = of(s);
      for (const k of ks) m.delete(hex(k));
    },
    async *list(s) {
      for (const [k, v] of [...of(s)]) yield [fromHex(k), v.slice()];
    },
    clear: async (s) => {
      if (s === undefined) stores.clear();
      else stores.delete(s);
    },
    deviceKey: () => {
      key ??= newDeviceKey();
      return key;
    },
    close: () => {},
  };
}
