// The kanban app's offline variant (docs/EXAMPLES.md "CRDT tables in the
// examples"): cards in a CRDT table the server merges field by field.
// `title`, `text`, `column` and `pos` are last-writer-wins, `labels` an
// add-wins set and `votes` a counter. Writes are queued and work offline;
// the `(column, pos)` index is kept by the indexer (docs/DB.md §19), so a
// listing through it lags a moment behind the writes.
//
// A move patches `column` and `pos` in one edit with one timestamp, so of
// two concurrent moves one wins both fields: one card, in one column.
import type { ChangeBody, CrdtRead, Db, Message, OnSubscription } from '@zen/db';
import { keyBetween } from '../../shared/fractional.js';

export type CrdtCard = {
  id: string;
  board: string;
  column: string;
  pos: string;
  title: string;
  text: string;
  labels: string[];
  votes: number;
};

/** The table (created by `DB_OPTIONS`, kanban.ts). */
export const CRDT_CARDS = 'crdt_cards';

const table = (db: Db) => db.table<CrdtCard>(CRDT_CARDS);

/**
 * A column's cards in order, through the indexer's `(column, pos)` index.
 * Every predicate is re-checked on the merged row, so a card that moved
 * away never shows here; one that just arrived may be missing for a moment.
 */
export function crdtColumnCards(db: Db, column: string, read?: CrdtRead): Promise<CrdtCard[]> {
  const q = table(db).query().where('column', '=', column).orderBy('pos');
  return (read ? q.read(read) : q).all();
}

/** One card, merged (with this instance's queued writes). */
export function crdtCard(db: Db, id: string, read?: CrdtRead): Promise<CrdtCard | undefined> {
  return table(db).get(id, read ? { read } : {});
}

/**
 * A new card after `after` (a position key; the start when absent). Queued:
 * it works offline.
 */
export async function crdtAddCard(
  db: Db,
  board: string,
  column: string,
  title: string,
  after?: string,
): Promise<CrdtCard> {
  const card: CrdtCard = {
    id: crypto.randomUUID(),
    board,
    column,
    pos: keyBetween(after),
    title,
    text: '',
    labels: [],
    votes: 0,
  };
  await table(db).insert(card);
  return card;
}

/** Set `title` or `text` (last writer wins, per field). */
export function crdtEdit(
  db: Db,
  id: string,
  fields: Partial<Pick<CrdtCard, 'title' | 'text'>>,
): Promise<void> {
  return table(db).patch(id, fields);
}

/** Add a vote (or `n`, negative to take some back). Votes from every device add up. */
export function crdtVote(db: Db, id: string, n = 1): Promise<void> {
  return table(db).incr(id, 'votes', n);
}

/** Add a label (add-wins against a concurrent remove). */
export function crdtLabel(db: Db, id: string, label: string): Promise<void> {
  return table(db).add(id, 'labels', label);
}

/** Remove a label: the copies seen now. */
export function crdtUnlabel(db: Db, id: string, label: string): Promise<void> {
  return table(db).remove(id, 'labels', label);
}

/**
 * Move a card between `before` and `after` (position keys in the target
 * column; absent: the start or the end). `column` and `pos` go in one
 * patch, so they carry one timestamp and win or lose together.
 */
export function crdtMove(
  db: Db,
  id: string,
  column: string,
  before?: string,
  after?: string,
): Promise<void> {
  return table(db).patch(id, { column, pos: keyBetween(before, after) });
}

/** Delete a card (only an `insert` brings it back). */
export function crdtDelete(db: Db, id: string): Promise<boolean> {
  return table(db).delete(id);
}

/** Change events of the CRDT cards (keys only: read the row). */
export function watchCrdtCards(
  db: Db,
  fn: (m: Message<ChangeBody<CrdtCard>>) => void,
  after?: Uint8Array,
): Promise<OnSubscription> {
  return table(db).on(fn, after ? { after } : {});
}
