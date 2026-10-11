// Unique indexes (spec/zendb.md §5.2): `D ‖ ("u", index_id, value) → {1: pk}`.
import { bytes } from '@zen/client';
import { decode, encode, type Value } from '../cbor.js';
import { DbError } from '../errors.js';
import type { IndexChange } from './types.js';

/** The pk an index entry `{1: pk}` names. */
export function entryPk(b: Uint8Array): Value {
  const m = decode(b);
  if (!(m instanceof Map) || !m.has(1)) throw new DbError('corrupt', 'a malformed index entry');
  return m.get(1)!;
}

/** An index entry naming `pk`. */
export const entry = (pk: Value): Uint8Array => encode(new Map([[1, pk]]));

/**
 * Move a row's unique entry from `old` to `new`. The new entry must be
 * absent or name the same row; reading it puts it into the read set (a
 * conflict in short mode, an `expect` in long mode).
 */
export async function uniqueChange(c: IndexChange): Promise<void> {
  const { tx, index, old, new: neu } = c;
  if (old && neu && bytes.equal(old, neu)) return;
  if (old) tx.raw.delete(tx.keys.unique(index.id, old));
  if (!neu) return;
  const k = tx.keys.unique(index.id, neu);
  const cur = await tx.raw.get(k);
  if (cur) {
    const holder = entryPk(cur);
    if (!bytes.equal(encode(holder), c.pkElement)) {
      throw new DbError('unique_violation', `index ${index.name} already holds this value`, {
        index: index.name,
        pk: holder,
      });
    }
    return;
  }
  tx.raw.set(k, entry(c.pk));
}
