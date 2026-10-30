// The caches of a database on the sealed local store (spec/zendb.md §9.3):
// written behind (gathered and flushed every 100 ms and at close), so
// filling a cache costs no store round trip per entry. A write lost to a
// crash only leaves a colder cache: every entry is checked when used, by
// its version (rows, catalog, CRDT states) or its content (nodes, blobs).
import { bytes, type SealedStore, zw } from '@zen/client';
import type { ObjState } from '@zen/wasm';
import { decode, encode, isPlainObject, type Value } from './cbor.js';
import type { Entry } from './crdtcache.js';
import type { Fields } from './row.js';

const FLUSH_MS = 100;

/** One store of the local store, written behind. */
export class LocalCache {
  /** Writes not yet flushed: a value, or undefined for a delete. */
  private readonly pending = new Map<string, Uint8Array | undefined>();
  private timer: ReturnType<typeof setTimeout> | undefined;
  private flushing: Promise<void> = Promise.resolve();
  /** A `clear` not yet flushed: it goes before the pending writes. */
  private clearing = false;

  constructor(
    private readonly local: SealedStore,
    readonly store: string,
    private readonly warn: (msg: string) => void,
  ) {}

  put(key: Uint8Array, value: Uint8Array): void {
    this.pending.set(bytes.hex(key), value);
    this.later();
  }

  delete(key: Uint8Array): void {
    this.pending.set(bytes.hex(key), undefined);
    this.later();
  }

  /** Forget every record of the store. */
  clear(): void {
    this.pending.clear();
    this.clearing = true;
    this.later();
  }

  /** A record, the pending writes first. */
  async get(key: Uint8Array): Promise<Uint8Array | undefined> {
    const h = bytes.hex(key);
    if (this.pending.has(h)) return this.pending.get(h);
    if (this.clearing) return undefined;
    try {
      return await this.local.get(this.store, key);
    } catch (e) {
      this.failed(e);
      return undefined;
    }
  }

  /** Every stored record (at open, before any write). */
  async *list(): AsyncIterable<[Uint8Array, Uint8Array]> {
    try {
      yield* this.local.list(this.store);
    } catch (e) {
      this.failed(e);
    }
  }

  private later(): void {
    if (this.timer) return;
    this.timer = setTimeout(() => {
      this.timer = undefined;
      void this.flush();
    }, FLUSH_MS);
  }

  /** Write what is pending. */
  flush(): Promise<void> {
    if (this.timer) {
      clearTimeout(this.timer);
      this.timer = undefined;
    }
    const clearing = this.clearing;
    const writes = [...this.pending];
    this.clearing = false;
    this.pending.clear();
    if (!clearing && !writes.length) return this.flushing;
    this.flushing = this.flushing.then(async () => {
      try {
        if (clearing) await this.local.clear(this.store);
        const dels: Uint8Array[] = [];
        for (const [h, v] of writes) {
          if (v) await this.local.put(this.store, bytes.fromHex(h), v);
          else dels.push(bytes.fromHex(h));
        }
        if (dels.length) await this.local.delete(this.store, dels);
      } catch (e) {
        this.failed(e);
      }
    });
    return this.flushing;
  }

  private failed(e: unknown): void {
    this.warn(
      `zen-db: the local store ${this.store} failed: ${e instanceof Error ? e.message : String(e)}`,
    );
  }
}

// ------------------------------------------------------------ records
//   row          {1: version, 2: fields, 3: parts}
//   CRDT state   {1?: ObjState (wire encoding), 2: checked, 3: stale}
//   table        {1: TableRecord, 2?: version}
//   sealed blob  {1: digest, 2: Blob}

type M = Map<Value, Value>;

const asMap = (b: Uint8Array): M => {
  const v = decode(b);
  if (!(v instanceof Map)) throw new Error('not a map');
  return v;
};

const bin = (v: Value | undefined): Uint8Array => {
  if (!(v instanceof Uint8Array)) throw new Error('not bytes');
  return v;
};

/** A kept row; `vh` (authenticated databases, §14.6) is absent in older records. */
export function encodeCachedRow(r: {
  version: Uint8Array;
  fields: Fields;
  parts: number;
  vh?: Uint8Array;
}) {
  const m = new Map<Value, Value>([
    [1, r.version],
    [2, r.fields],
    [3, r.parts],
  ]);
  if (r.vh) m.set(4, r.vh);
  return encode(m);
}

export function decodeCachedRow(b: Uint8Array): {
  version: Uint8Array;
  fields: Fields;
  parts: number;
  vh?: Uint8Array;
} {
  const m = asMap(b);
  const fields = m.get(2);
  const parts = m.get(3);
  if (!isPlainObject(fields) || typeof parts !== 'number') throw new Error('a row');
  const vh = m.get(4);
  return {
    version: bin(m.get(1)),
    fields: fields as Fields,
    parts,
    ...(vh !== undefined ? { vh: bin(vh) } : {}),
  };
}

export function encodeEntry(e: Entry): Uint8Array {
  const m = new Map<Value, Value>([
    [2, e.checked],
    [3, e.stale],
  ]);
  if (e.state) m.set(1, zw.encodeObjState(e.state));
  return encode(m);
}

export function decodeEntry(b: Uint8Array): Entry {
  const m = asMap(b);
  const checked = m.get(2);
  const stale = m.get(3);
  if (typeof checked !== 'number' || typeof stale !== 'boolean') throw new Error('a state');
  const s = m.get(1);
  return {
    state: s === undefined ? undefined : (zw.decodeObjState(bin(s)) as ObjState),
    checked,
    stale,
  };
}

export const encodeTable = (record: Uint8Array, version: Uint8Array | undefined) =>
  encode(
    new Map<Value, Value>([[1, record], ...(version ? ([[2, version]] as [Value, Value][]) : [])]),
  );

export function decodeTable(b: Uint8Array): { record: Uint8Array; version?: Uint8Array } {
  const m = asMap(b);
  const v = m.get(2);
  return { record: bin(m.get(1)), ...(v === undefined ? {} : { version: bin(v) }) };
}

export const encodeSealed = (digest: Uint8Array, blob: Uint8Array) =>
  encode(
    new Map<Value, Value>([
      [1, digest],
      [2, blob],
    ]),
  );

export function decodeSealed(b: Uint8Array): { digest: Uint8Array; blob: Uint8Array } {
  const m = asMap(b);
  return { digest: bin(m.get(1)), blob: bin(m.get(2)) };
}
