// The sealed local store (spec/zendb.md §9.3): what goes into a
// `LocalStore` is sealed in up to two layers, and a record that doesn't
// open reads as absent.
//
//   record = u8 version (1) ‖ u8 layers (bit 0 device, bit 1 fs) ‖ body
//
// * fs layer (default off): a kind-8 sealed object (formats.md §4) under the
//   fs keys, bound to the fs, the store name and the key.
// * device layer (default on): AES-256-GCM under this device's key, nonce(12)
//   ‖ ciphertext, AAD = "zen/v1/device" ‖ 0x00 ‖ version ‖ layers ‖
//   lp(store) ‖ lp(key). It wraps the fs layer when both are on.
//
// Neither layer is the app's choice too, but only with `plaintext: true`:
// the records then sit in the clear, with no tamper check.
import { concat, utf8 } from '../bytes.js';
import type { UnlockedFs } from '../fs.js';
import type { LocalStore } from './store.js';

const VERSION = 1;
const DEVICE = 1;
const FS = 2;
const NONCE = 12;
const LABEL = utf8('zen/v1/device');

/** Options of `sealedStore`. */
export interface SealedStoreOptions {
  /**
   * The fs layer: seal under this fs's keys too (default: off). Records
   * then open only for someone who can unlock the fs.
   */
  fs?: UnlockedFs;
  /**
   * The device layer (default: on, with the backend's device key). A
   * `CryptoKey` (AES-GCM, 256 bits) uses that key instead; `false` turns
   * the layer off.
   */
  device?: CryptoKey | false;
  /**
   * Store records in the clear: required when neither layer is on, so it is
   * never an accident. Fits a disk the OS already encrypts, data that isn't
   * sensitive, or a trusted host.
   */
  plaintext?: boolean;
  /** Where warnings go (records that don't open). Default `console.warn`. */
  warn?: (msg: string) => void;
}

/** A local store whose records are sealed (or, by choice, plaintext). */
export interface SealedStore {
  /** The backend underneath (shared, unsealed). */
  readonly raw: LocalStore;
  /** The layers this store writes: bit 0 device, bit 1 fs. */
  readonly layers: number;
  get(store: string, key: Uint8Array): Promise<Uint8Array | undefined>;
  put(store: string, key: Uint8Array, value: Uint8Array): Promise<void>;
  delete(store: string, keys: Uint8Array[]): Promise<void>;
  /** Every record that opens; the others are deleted. */
  list(store: string): AsyncIterable<[Uint8Array, Uint8Array]>;
  clear(store?: string): Promise<void>;
}

function lp(b: Uint8Array): Uint8Array {
  const out = new Uint8Array(4 + b.length);
  new DataView(out.buffer).setUint32(0, b.length);
  out.set(b, 4);
  return out;
}

const aad = (layers: number, store: string, key: Uint8Array) =>
  concat(LABEL, new Uint8Array([0, VERSION, layers]), lp(utf8(store)), lp(key));

/** A record that doesn't open (§9.3): read as absent. */
class Unreadable extends Error {}

/** Seal what goes into `raw` (§9.3). */
export function sealedStore(raw: LocalStore, opts: SealedStoreOptions = {}): SealedStore {
  const device = opts.device ?? undefined;
  const layers = (device === false ? 0 : DEVICE) | (opts.fs ? FS : 0);
  if (!layers && !opts.plaintext) {
    throw new TypeError(
      'a local store with neither the device nor the fs layer needs plaintext: true',
    );
  }
  const warn = opts.warn ?? ((m: string) => console.warn(m));
  const fs = opts.fs;
  let key: Promise<CryptoKey> | undefined;
  const deviceKey = () => {
    key ??= device instanceof CryptoKey ? Promise.resolve(device) : raw.deviceKey();
    return key;
  };

  async function seal(store: string, k: Uint8Array, value: Uint8Array): Promise<Uint8Array> {
    let body = value;
    if (layers & FS) body = fs!.keys.sealLocal(store, k, body);
    if (layers & DEVICE) {
      const nonce = crypto.getRandomValues(new Uint8Array(NONCE));
      const ct = await crypto.subtle.encrypt(
        { name: 'AES-GCM', iv: nonce, additionalData: aad(layers, store, k) as BufferSource },
        await deviceKey(),
        body as BufferSource,
      );
      body = concat(nonce, new Uint8Array(ct));
    }
    return concat(new Uint8Array([VERSION, layers]), body);
  }

  async function open(store: string, k: Uint8Array, rec: Uint8Array): Promise<Uint8Array> {
    if (rec.length < 2 || rec[0] !== VERSION) throw new Unreadable('an unknown record version');
    if (rec[1] !== layers) throw new Unreadable(`sealed with layers ${rec[1]}, not ${layers}`);
    let body = rec.subarray(2);
    if (layers & DEVICE) {
      if (body.length < NONCE + 16) throw new Unreadable('a short record');
      try {
        body = new Uint8Array(
          await crypto.subtle.decrypt(
            {
              name: 'AES-GCM',
              iv: body.subarray(0, NONCE) as BufferSource,
              additionalData: aad(layers, store, k) as BufferSource,
            },
            await deviceKey(),
            body.subarray(NONCE) as BufferSource,
          ),
        );
      } catch {
        throw new Unreadable('the device layer does not open');
      }
    }
    if (layers & FS) {
      try {
        body = fs!.keysFor(body).openLocal(store, k, body);
      } catch {
        throw new Unreadable('the fs layer does not open');
      }
    }
    return body.slice();
  }

  /** Open, or drop a record that doesn't open (with a warning). */
  async function opened(
    store: string,
    k: Uint8Array,
    rec: Uint8Array,
  ): Promise<Uint8Array | undefined> {
    try {
      return await open(store, k, rec);
    } catch (e) {
      if (!(e instanceof Unreadable)) throw e;
      warn(`zen local store: a record of ${store} does not open (${e.message}); dropped`);
      await raw.delete(store, [k]);
      return undefined;
    }
  }

  return {
    raw,
    layers,
    async get(store, k) {
      const rec = await raw.get(store, k);
      return rec && opened(store, k, rec);
    },
    async put(store, k, value) {
      await raw.put(store, k, await seal(store, k, value));
    },
    delete: (store, keys) => raw.delete(store, keys),
    async *list(store) {
      for await (const [k, rec] of raw.list(store)) {
        const v = await opened(store, k, rec);
        if (v) yield [k, v];
      }
    },
    clear: (store) => raw.clear(store),
  };
}
