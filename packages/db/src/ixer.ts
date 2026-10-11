// The indexer of CRDT tables (spec/zendb.md §19.6). Writers can't maintain
// indexes on CRDT tables, since the merged value exists only on the server:
// a consumer of each table's change topic reads the changed row's merged
// state and moves its index entries from the values last indexed
// (`$ixrows`) to the new ones.
import { bytes } from '@zen/client';
import type { IndexDef, TableRecord } from './catalog.js';
import { encode, same, type Value } from './cbor.js';
import type { Db } from './db.js';
import type { Fields } from './row.js';
import { fitsType } from './sortkey.js';
import { changeIndex, type DbTransaction, indexedValue, maintained } from './txn.js';

/** The indexer's group on each change topic (§19.6). */
export const INDEX_GROUP = '$index';

/** Change events the indexer handles at once (`per_key`). */
const INFLIGHT = 8;

/** An `$ixrows` row (§11.5): the values last indexed, by index id (hex). */
interface IxRow extends Fields {
  table_id: Uint8Array;
  pk: Value;
  values: { [index: string]: Value[] };
}

/**
 * Bring one CRDT row's index entries up to date (§19.6): its merged state
 * (`row`, if the caller has it; `null` for none), against the values its
 * `$ixrows` row holds. With `only`, just that index, and nothing if the
 * row is indexed there already (a backfill: the indexer keeps it current).
 * Returns false when `only` was skipped.
 */
export async function reindex(
  tx: DbTransaction,
  t: TableRecord,
  pk: Value,
  pkEl: Uint8Array,
  opts: { only?: IndexDef; row?: Fields | null } = {},
): Promise<boolean> {
  const ixt = tx.table<IxRow>('$ixrows');
  const ixPk = [t.id, pk];
  const had = await ixt.get(ixPk);
  const old = new Map<string, Value[]>();
  for (const [k, v] of Object.entries(had?.values ?? {})) if (Array.isArray(v)) old.set(k, v);
  const { only } = opts;
  if (only && old.has(bytes.hex(only.id))) return false;
  const row =
    opts.row !== undefined
      ? (opts.row ?? undefined)
      : (await tx.table(t.name).crdtRows(t, [pkEl], 'server'))[0];

  // `only` changes one entry; otherwise the maintained indexes are all
  // there is, so values of dropped indexes go.
  const next = only ? new Map(old) : new Map<string, Value[]>();
  for (const ix of only ? [only] : maintained(t)) {
    const h = bytes.hex(ix.id);
    const oldV = old.get(h);
    let newV = row ? indexedValue(ix, row) : undefined;
    if (newV && !ix.fields.every(([, type], i) => fitsType(newV![i], type))) {
      // The write happened: it can't be refused, so the row stays out of the index.
      tx.db.warn(`zen-db: a row of ${t.name} has values index ${ix.name} can't hold; left out`);
      newV = undefined;
    }
    if (!(oldV && newV ? same(oldV, newV) : oldV === newV)) {
      await changeIndex(tx, t, ix, pk, pkEl, oldV, newV);
    }
    if (newV) next.set(h, newV);
    else next.delete(h);
  }

  if (!next.size) {
    if (had) await ixt.delete(ixPk);
  } else if (!had || !sameValues(old, next)) {
    const values = Object.fromEntries(next);
    await (had ? ixt.put : ixt.insert).call(ixt, { table_id: t.id, pk, values });
  }
  return true;
}

function sameValues(a: Map<string, Value[]>, b: Map<string, Value[]>): boolean {
  if (a.size !== b.size) return false;
  for (const [k, v] of a) {
    const w = b.get(k);
    if (!w || !same(v, w)) return false;
  }
  return true;
}

/**
 * Start the indexers (§19.6): one `per_key` consumer group `$index` per
 * change topic of a CRDT table with indexes. They run in the owner, like
 * every consumer. Tables that gain their first index while this instance
 * is open are indexed from its next open.
 */
export function startIndexers(db: Db, tables: TableRecord[]): void {
  const topics = new Map<string, (string | Uint8Array)[]>();
  for (const t of tables) {
    if (t.merge !== 'crdt' || !t.changes || !maintained(t).length) continue;
    topics.set(t.changes.topic.map((p) => bytes.hex(p)).join('/'), t.changes.topic);
  }
  for (const path of topics.values()) {
    db.consume<{ table?: unknown; pk?: Value }>(
      {
        group: INDEX_GROUP,
        topic: db.topic(...path),
        mode: 'per_key',
        maxInflight: INFLIGHT,
        start: 'earliest',
      },
      async (msg, tx) => {
        const b = msg.body;
        if (msg.type !== 'zen.change' || typeof b?.table !== 'string' || b.pk === undefined) {
          return;
        }
        // A dropped (or renamed) table: nothing left to index.
        const t = await tx.tableRecordOrNone(b.table);
        if (t?.state !== 'active' || t.merge !== 'crdt') return;
        await reindex(tx, t, b.pk, encode(b.pk));
      },
    );
  }
}
