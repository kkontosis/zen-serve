// Fast indexes (spec/zendb.md §5.3): `D ‖ ("f", index_id, value, pk) → {1: pk}`.
// The server learns how many rows share each value.
import { bytes } from '@zen/client';
import type { IndexChange } from './types.js';
import { entry } from './unique.js';

/** Move a row's fast entry from `old` to `new`. */
export function fastChange(c: IndexChange): void {
  const { tx, index, old, new: neu } = c;
  if (old && neu && bytes.equal(old, neu)) return;
  if (old) tx.raw.delete(tx.keys.fast(index.id, old, c.pkElement));
  if (neu) tx.raw.set(tx.keys.fast(index.id, neu, c.pkElement), entry(c.pk));
}
