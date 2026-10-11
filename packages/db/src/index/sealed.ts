// Sealed indexes (spec/zendb.md §5.7): the whole entry list in one blob,
// cut into parts padded to a power of two, behind a head with its digest.
// Every write rewrites it whole; the server learns only its size class.
import { bytes, ZenError } from '@zen/client';
import { decode, encode, type Value } from '../cbor.js';
import { DbError } from '../errors.js';
import { joinParts, partsDigest } from '../row.js';
import { decodeSortKey } from '../sortkey.js';
import type { IndexChange, IndexTx } from './types.js';

/** Default `max_bytes` of a sealed index's blob. */
export const SEALED_MAX_BYTES = 1_048_576;

/** `{1: parts, 2: digest, 3: count}`. */
export interface SealedHead {
  parts: number;
  digest: Uint8Array;
  count: number;
}

export function encodeHead(h: SealedHead): Uint8Array {
  return encode(
    new Map<Value, Value>([
      [1, h.parts],
      [2, h.digest],
      [3, h.count],
    ]),
  );
}

const corrupt = (what: string) => new DbError('corrupt', `a malformed sealed index ${what}`);

export function decodeHead(b: Uint8Array): SealedHead {
  const m = decode(b);
  if (!(m instanceof Map)) throw corrupt('head');
  const [parts, digest, count] = [m.get(1), m.get(2), m.get(3)];
  if (
    typeof parts !== 'number' ||
    parts < 1 ||
    !(digest instanceof Uint8Array) ||
    digest.length !== 32 ||
    typeof count !== 'number'
  ) {
    throw corrupt('head');
  }
  return { parts, digest, count };
}

/** `Blob = {1: entries}`. */
export const encodeBlob = (keys: Uint8Array[]): Uint8Array => encode(new Map([[1, keys]]));

export function decodeBlob(v: Value): Uint8Array[] {
  const es = v instanceof Map ? v.get(1) : undefined;
  if (!Array.isArray(es) || !es.every((e) => e instanceof Uint8Array)) throw corrupt('blob');
  return es as Uint8Array[];
}

/** Cut a blob into parts of `size`, their number rounded up to a power of two, zero-padded. */
export function blobParts(blob: Uint8Array, size: number): Uint8Array[] {
  let n = 1;
  while (n * size < blob.length) n *= 2;
  const all = new Uint8Array(n * size);
  all.set(blob);
  return Array.from({ length: n }, (_, i) => all.slice(i * size, (i + 1) * size));
}

/** The entries of a sealed index in a transaction, read once and changed in place. */
export class SealedState {
  dirty = false;

  constructor(
    readonly tx: IndexTx,
    readonly index: Uint8Array,
    readonly maxBytes: number,
    /** Sorted. */
    readonly keys: Uint8Array[],
    /** Parts of the stored blob (0: none). */
    readonly storedParts: number,
  ) {}

  /** Position of the first entry ≥ `k`. */
  find(k: Uint8Array): number {
    let lo = 0;
    let hi = this.keys.length;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (bytes.compare(this.keys[mid]!, k) < 0) lo = mid + 1;
      else hi = mid;
    }
    return lo;
  }

  remove(k: Uint8Array): void {
    const i = this.find(k);
    if (i < this.keys.length && bytes.equal(this.keys[i]!, k)) {
      this.keys.splice(i, 1);
      this.dirty = true;
    }
  }

  add(k: Uint8Array): void {
    const i = this.find(k);
    if (i < this.keys.length && bytes.equal(this.keys[i]!, k)) return;
    this.keys.splice(i, 0, k);
    this.dirty = true;
  }

  /** The entry other than `own` whose field part is `prefix`, if any. */
  holder(prefix: Uint8Array, own: Uint8Array): Uint8Array | undefined {
    for (let i = this.find(prefix); i < this.keys.length; i++) {
      const k = this.keys[i]!;
      if (k.length < prefix.length || !bytes.equal(k.subarray(0, prefix.length), prefix)) return;
      if (!bytes.equal(k, own)) return k;
    }
    return undefined;
  }

  /** Write the blob, its parts and its head (§5.7). */
  write(): Uint8Array {
    const { tx, index } = this;
    const blob = encodeBlob(this.keys);
    if (blob.length > this.maxBytes) {
      throw new DbError(
        'too_large',
        `a sealed index of ${blob.length} bytes; its max_bytes is ${this.maxBytes}`,
      );
    }
    const parts = blobParts(blob, tx.partSize);
    parts.forEach((p, i) => {
      tx.raw.set(tx.keys.sealedPart(index, i), p);
    });
    for (let i = parts.length; i < this.storedParts; i++) {
      tx.raw.delete(tx.keys.sealedPart(index, i));
    }
    const digest = partsDigest(blob);
    tx.raw.set(
      tx.keys.sealedHead(index),
      encodeHead({ parts: parts.length, digest, count: this.keys.length }),
    );
    return digest;
  }
}

/**
 * Read a sealed index: the head into the read set, then the parts unless
 * the cached blob has the head's digest. Returns the entries and the
 * number of stored parts.
 */
export async function readSealed(
  tx: IndexTx,
  index: Uint8Array,
  cached: (digest: string) => Uint8Array[] | undefined,
): Promise<{ keys: Uint8Array[]; parts: number; digest?: Uint8Array }> {
  const b = await tx.raw.get(tx.keys.sealedHead(index));
  if (!b) return { keys: [], parts: 0 };
  const head = decodeHead(b);
  const hit = cached(bytes.hex(head.digest));
  if (hit) return { keys: [...hit], parts: head.parts, digest: head.digest };
  const got = await tx.raw.snapshotGet(
    Array.from({ length: head.parts }, (_, i) => tx.keys.sealedPart(index, i)),
  );
  // A head and parts that disagree: in long mode the index changed between reads.
  const torn = () =>
    tx.raw.mode === 'long'
      ? new ZenError(409, 'conflict', 'a sealed index changed while it was read')
      : corrupt('blob');
  if (got.some((e) => !e)) throw torn();
  let keys: Uint8Array[];
  try {
    keys = decodeBlob(
      joinParts(
        got.map((e) => e!.value),
        head.digest,
      ),
    );
  } catch (e) {
    if (e instanceof DbError && e.code === 'corrupt') throw torn();
    throw e;
  }
  if (keys.length !== head.count) throw corrupt('head count');
  return { keys, parts: head.parts, digest: head.digest };
}

/**
 * Move a row's entry in a sealed index. A unique index is checked against
 * the list: no other row may hold the same fields (values with a null are
 * not checked, as for unique entries).
 */
export function sealedChange(
  s: SealedState,
  c: IndexChange,
  oldKey: Uint8Array | undefined,
  newKey: Uint8Array | undefined,
  fieldPart: Uint8Array | undefined,
): void {
  if (oldKey && newKey && bytes.equal(oldKey, newKey)) return;
  if (oldKey) s.remove(oldKey);
  if (!newKey) return;
  if (c.index.unique && fieldPart) {
    const other = s.holder(fieldPart, newKey);
    if (other) {
      const { pk } = decodeSortKey(
        c.index.fields.map(([, , desc]) => ({ desc })),
        other,
      );
      throw new DbError('unique_violation', `index ${c.index.name} already holds this value`, {
        index: c.index.name,
        pk: pk.length === 1 ? pk[0]! : pk,
      });
    }
  }
  s.add(newKey);
}
