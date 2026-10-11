// Helpers shared by the server tests of @zen/db.
import { bytes } from '@zen/client';
import { expect } from 'vitest';
import type { IndexDef } from '../src/catalog.js';
import type { Value } from '../src/cbor.js';
import { decodeNode } from '../src/index/node.js';
import { sortKeyOf } from '../src/index/private.js';
import type { Db, Fields } from '../src/index.js';

/** The stored tree of a private index (one shard) against zen-core's builder. */
export async function expectCanonical(db: Db, ix: IndexDef, rows: Fields[], shard = 0) {
  const keys = rows.map((r) =>
    sortKeyOf(
      ix,
      ix.fields.map(([f]) => r[f] ?? null),
      [r.id as Value],
    ),
  );
  const ref = db.indexKeys(ix.id).build(keys, ix.fanout ?? 64);
  const want = new Map<string, string>();
  for (let i = 0; i + 1 < ref.length; i += 2) want.set(bytes.hex(ref[i]!), bytes.hex(ref[i + 1]!));
  const got = await db.transaction(async (tx) => {
    const root = await tx.raw.get(db.keys.root(ix.id, shard));
    const nodes = await tx.raw.rangeStored(...db.keys.range('n', ix.id));
    return { root, nodes };
  });
  expect(got.root && bytes.hex(got.root)).toBe(ref.length % 2 ? bytes.hex(ref.at(-1)!) : undefined);
  const stored = new Map<string, string>();
  for (const e of got.nodes) {
    const node = decodeNode(e.value);
    // The stored key is a token; find the node by its id.
    const id = bytes.hex(db.indexKeys(ix.id).nodeId(e.value));
    stored.set(id, bytes.hex(e.value));
    expect(node.entries.length).toBeGreaterThan(0);
  }
  expect(stored).toEqual(want);
}
