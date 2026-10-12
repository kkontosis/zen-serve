// The kanban app's logic in Node: two members signed in by device key, with
// only the app's grants. The demo invariants of docs/EXAMPLES.md §1: moves,
// attachments that commit with their rows (and a writer killed mid-upload),
// deletes, live change events, and the CRDT variant merging offline edits.
import { bytes, type Tree, ZenError } from '@zen/client';
import type { Db, DbTransaction, DbTxnOptions } from '@zen/db';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import {
  net,
  spawnWorker,
  startWorld,
  until,
  type World,
  workerEnv,
} from '../../shared/harness.js';
import { signInWorker, type WorkerContext } from '../../shared/node.js';
import {
  crdtAddCard,
  crdtCard,
  crdtColumnCards,
  crdtEdit,
  crdtLabel,
  crdtMove,
  crdtUnlabel,
  crdtVote,
} from '../src/crdt.js';
import {
  addCard,
  attachFile,
  type Board,
  boardTree,
  boardView,
  type Card,
  type CardChange,
  columnCards,
  DB_OPTIONS,
  deleteCard,
  editCard,
  ensureBoard,
  listColumns,
  moveCard,
  NS,
  onCursors,
  openCardText,
  publishCursor,
  readAttachment,
  watchCards,
} from '../src/kanban.js';
import { GRANTS } from './grants.js';

/** Chunks no write refers to are collected this long after their upload. */
const GRACE_SECS = 2;

let w: World;
const workers: WorkerContext[] = [];
let a: Db;
let b: Db;
let board: Board;
let ta: Tree;
let tb: Tree;

beforeAll(async () => {
  w = await startWorld({
    users: ['alice', 'bob'],
    grants: GRANTS,
    server: { limits: { chunk_grace_secs: GRACE_SECS, sweep_interval_secs: 1 } },
  });
  const dbs: Db[] = [];
  for (const name of ['alice', 'bob']) {
    const c = await signInWorker({
      url: w.url,
      device: w.user(name).deviceFile,
      passphrase: w.passphrase,
    });
    workers.push(c);
    dbs.push(await c.open(NS, DB_OPTIONS));
  }
  [a, b] = dbs as [Db, Db];
  // Both create the board at once: one board, with one set of columns.
  const [ba, bb] = await Promise.all([ensureBoard(a, 'family'), ensureBoard(b, 'family')]);
  expect(ba).toEqual(bb);
  board = ba;
  ta = boardTree(workers[0]!.fs, board);
  tb = boardTree(workers[1]!.fs, board);
});

afterAll(async () => {
  for (const c of workers) await c.close();
  await w?.stop();
});

const cols = () => listColumns(a, board.id);
const titles = (cs: Card[]) => cs.map((c) => c.title);

/** Every card of the board, and in which column it shows (by the index). */
async function placements(db: Db): Promise<Map<string, string[]>> {
  const out = new Map<string, string[]>();
  for (const { column, cards } of await boardView(db, board.id)) {
    for (const c of cards) out.set(c.id, [...(out.get(c.id) ?? []), column.id]);
  }
  return out;
}

/**
 * Run `fn` with the transactions of `dbs` held at their end until each db
 * has run one (all their reads done), so they commit at the same moment and
 * conflict. Returns how many times each db ran a transaction function.
 */
async function racing<R>(dbs: Db[], fn: () => Promise<R>): Promise<number[]> {
  const runs = dbs.map(() => 0);
  let arrived = 0;
  let go!: () => void;
  const all = new Promise<void>((r) => {
    go = r;
  });
  const orig = dbs.map((d) => d.transaction.bind(d));
  dbs.forEach((d, i) => {
    d.transaction = ((f: (tx: DbTransaction) => Promise<unknown>, o?: DbTxnOptions) =>
      orig[i]!(async (tx) => {
        const r = await f(tx);
        if (++runs[i]! === 1) {
          if (++arrived === dbs.length) go();
          await all;
        }
        return r;
      }, o)) as Db['transaction'];
  });
  try {
    await fn();
  } finally {
    dbs.forEach((d, i) => {
      d.transaction = orig[i]! as Db['transaction'];
    });
  }
  return runs;
}

describe('the transactional board', () => {
  it('has the default columns once, though both members created it', async () => {
    expect((await cols()).map((c) => c.name)).toEqual(['To do', 'Doing', 'Done']);
  });

  it('orders cards by fractional keys; a move rewrites only the moved card', async () => {
    const [todo, doing] = await cols();
    const c1 = await addCard(a, ta, todo!, 'one');
    const c2 = await addCard(a, ta, todo!, 'two');
    const c3 = await addCard(a, ta, todo!, 'three');
    expect(titles(await columnCards(b, todo!.id))).toEqual(['one', 'two', 'three']);
    // Three to the front: only its row changes.
    await moveCard(b, c3.id, todo!.id, 0);
    expect(titles(await columnCards(a, todo!.id))).toEqual(['three', 'one', 'two']);
    expect(await a.table<Card>('cards').get(c1.id)).toEqual(c1);
    expect(await a.table<Card>('cards').get(c2.id)).toEqual(c2);
    // One to another column, between nothing; then two after it.
    await moveCard(a, c1.id, doing!.id);
    await moveCard(a, c2.id, doing!.id, 1);
    expect(titles(await columnCards(b, doing!.id))).toEqual(['one', 'two']);
    expect(titles(await columnCards(b, todo!.id))).toEqual(['three']);
  });

  it('two concurrent moves of one card give one card, in exactly one column', async () => {
    const [todo, doing, done] = await cols();
    for (let i = 0; i < 3; i++) {
      const card = await addCard(a, ta, todo!, `contested ${i}`);
      // Both read before either commits, so they really race.
      const runs = await racing([a, b], () =>
        Promise.all([moveCard(a, card.id, doing!.id, 0), moveCard(b, card.id, done!.id, 0)]),
      );
      // One conflicted and re-ran after the other committed: it is the one that stays.
      expect([...runs].sort()).toEqual([1, 2]);
      const row = await b.table<Card>('cards').get(card.id);
      expect(row!.column).toBe(runs.indexOf(2) === 0 ? doing!.id : done!.id);
      expect((await placements(a)).get(card.id)).toEqual([row!.column]);
    }
    // Two cards moved into one place at once: the second re-reads and lands next to the first.
    const x = await addCard(a, ta, todo!, 'x');
    const y = await addCard(b, tb, todo!, 'y');
    const runs = await racing([a, b], () =>
      Promise.all([moveCard(a, x.id, done!.id, 0), moveCard(b, y.id, done!.id, 0)]),
    );
    expect([...runs].sort()).toEqual([1, 2]);
    const ids = (await columnCards(a, done!.id)).map((c) => c.id);
    expect(new Set(ids).size).toBe(ids.length);
    expect(ids.slice(0, 2).sort()).toEqual([x.id, y.id].sort());
  });

  it('change events and cursors reach the other member live', async () => {
    const [todo, doing] = await cols();
    const seen: CardChange[] = [];
    const sub = await watchCards(b, board.id, (m) => seen.push(m));
    const cursors: string[] = [];
    const csub = await onCursors(b, board.id, (m) => cursors.push(`${m.body.user}:${m.body.card}`));
    const card = await addCard(a, ta, todo!, 'live');
    await until(
      async () => {
        await publishCursor(a, board.id, { user: 'alice', card: card.id });
        return cursors.length > 0;
      },
      15_000,
      'a cursor',
    );
    expect(cursors[0]).toBe(`alice:${card.id}`);
    await moveCard(a, card.id, doing!.id);
    await attachFile(a, ta, card.id, 'note.txt', new TextEncoder().encode('hi'));
    await until(() => seen.length >= 3, 15_000, 'three change events');
    expect(
      seen.map((m) => [m.body.op, m.body.row?.column, m.body.row?.attachments.length]),
    ).toEqual([
      ['put', todo!.id, 0],
      ['put', doing!.id, 0],
      ['put', doing!.id, 1],
    ]);
    // Resuming from the last offset seen replays nothing seen before.
    sub.close();
    const again: CardChange[] = [];
    const sub2 = await watchCards(b, board.id, (m) => again.push(m), seen.at(-1)!.offset);
    await editCard(a, card.id, { title: 'live (edited)' });
    await until(() => again.length >= 1, 15_000, 'the event after the offset');
    expect(again.map((m) => m.body.row?.title)).toEqual(['live (edited)']);
    sub2.close();
    csub.close();
  });

  it('an attachment commits with its card row and reads back on the other side', async () => {
    const [todo] = await cols();
    const card = await addCard(a, ta, todo!, 'with files');
    const data = new Uint8Array(200_000).map((_, i) => (i * 31) % 251);
    const att = await attachFile(a, ta, card.id, 'photo.jpg', data);
    expect(att).toMatchObject({ name: 'photo.jpg', size: 200_000 });
    const row = await b.table<Card>('cards').get(card.id);
    expect(row!.attachments).toEqual([att]);
    expect(bytes.equal(await readAttachment(tb, att), data)).toBe(true);
    // The file is in the card's directory, under its name.
    const [node] = await tb.list(bytes.fromHex(row!.dir));
    expect(bytes.hex(node!.id)).toBe(att.node);
    expect(node!.meta?.name).toBe('photo.jpg');
  });

  it('a writer killed mid-upload leaves no card pointing at a missing file; its chunks are collected', async () => {
    const [todo] = await cols();
    const card = await addCard(a, ta, todo!, 'abandoned upload');
    await attachFile(a, ta, card.id, 'kept.txt', new TextEncoder().encode('kept'));
    const p = spawnWorker(
      'kanban',
      'uploader',
      workerEnv(w, 'bob', { KANBAN_BOARD: board.id, KANBAN_CARD: card.id }),
    );
    let chunks: Uint8Array[];
    try {
      await p.waitFor('uploaded ', 60_000);
      const line = p.output.find((l) => l.startsWith('uploaded '))!;
      chunks = line.slice('uploaded '.length).split(',').map(bytes.fromHex);
    } finally {
      await p.kill();
    }
    expect(await p.exited).toBe('SIGKILL');
    expect(chunks.length).toBeGreaterThan(0);
    // The chunks are on the server, but nothing refers to them.
    expect((await ta.fetchChunks(chunks.slice(0, 1))).size).toBe(1);
    const row = await a.table<Card>('cards').get(card.id);
    expect(row!.attachments.map((x) => x.name)).toEqual(['kept.txt']);
    expect((await ta.list(bytes.fromHex(row!.dir))).map((n) => n.meta?.name)).toEqual(['kept.txt']);
    // Every attachment of every card reads.
    for (const { cards } of await boardView(b, board.id)) {
      for (const c of cards) {
        for (const x of c.attachments) {
          expect((await readAttachment(tb, x)).length).toBe(x.size);
        }
      }
    }
    // After the grace period, the sweeper collects them.
    await until(
      async () => {
        try {
          await ta.fetchChunks(chunks.slice(0, 1));
          return false;
        } catch (e) {
          if (e instanceof ZenError && e.code === 'not_found') return true;
          throw e;
        }
      },
      30_000,
      'the abandoned chunks to be collected',
    );
    // The kept attachment's chunks are referenced, so they stay.
    expect(new TextDecoder().decode(await readAttachment(tb, row!.attachments[0]!))).toBe('kept');
  });

  it('deleting a card moves its attachment directory to TRASH, in the same commit', async () => {
    const [todo] = await cols();
    const card = await addCard(a, ta, todo!, 'to delete');
    await attachFile(a, ta, card.id, 'a.txt', new TextEncoder().encode('a'));
    const seen: CardChange[] = [];
    const sub = await watchCards(b, board.id, (m) => {
      if (m.body.pk === card.id) seen.push(m);
    });
    expect(await deleteCard(b, tb, card.id)).toBe(true);
    expect(await a.table<Card>('cards').get(card.id)).toBeUndefined();
    expect((await ta.getOne(bytes.fromHex(card.dir)))?.parent).toEqual(
      new Uint8Array(16).fill(0xff),
    );
    expect(await deleteCard(a, ta, card.id)).toBe(false);
    await until(() => seen.length === 1, 15_000, 'the delete event');
    expect(seen[0]!.body.op).toBe('delete');
    sub.close();
    expect((await placements(a)).has(card.id)).toBe(false);
  });

  it("a card's long text (Loro) merges", async () => {
    const [todo] = await cols();
    const card = await addCard(a, ta, todo!, 'with text');
    const [da, db] = await Promise.all([openCardText(a, card.id), openCardText(b, card.id)]);
    await Promise.all([da.ready, db.ready]);
    da.doc.getText('text').insert(0, 'milk ');
    da.doc.commit();
    db.doc.getText('text').insert(0, 'eggs');
    db.doc.commit();
    const text = (d: typeof da) => d.doc.getText('text').toString();
    await until(() => text(da) === text(db) && text(da).length === 9, 15_000, 'convergence');
    expect(text(da)).toMatch(/milk/);
    expect(text(da)).toMatch(/eggs/);
    await da.close();
    await db.close();
  });
});

describe('the CRDT board (offline)', () => {
  it('offline edits merge field by field: both fields survive, counters sum, sets add-win', async () => {
    const [todo, doing] = await cols();
    const card = await crdtAddCard(a, board.id, todo!.id, 'groceries');
    await crdtLabel(a, card.id, 'shared');
    await a.crdt.flush();
    // Both have read it once, so it is cached for them offline.
    expect((await crdtCard(a, card.id, 'server'))?.labels).toEqual(['shared']);
    await until(async () => (await crdtCard(b, card.id, 'server'))?.labels.length === 1);
    const na = net(workers[0]!.session);
    const nb = net(workers[1]!.session);
    try {
      na.offline = true;
      nb.offline = true;
      // Alice: the title, a vote, a label, a move; drops "shared".
      await crdtEdit(a, card.id, { title: 'groceries (sat)' });
      await crdtVote(a, card.id);
      await crdtLabel(a, card.id, 'home');
      await crdtUnlabel(a, card.id, 'shared');
      await crdtMove(a, card.id, doing!.id);
      // Bob: the text, two votes, a label, and "shared" again (a new copy).
      await crdtEdit(b, card.id, { text: 'milk, eggs' });
      await crdtVote(b, card.id, 2);
      await crdtLabel(b, card.id, 'urgent');
      await crdtLabel(b, card.id, 'shared');
      // Each sees its own edits offline.
      expect((await crdtCard(a, card.id, 'cache'))?.title).toBe('groceries (sat)');
      expect((await crdtCard(b, card.id, 'cache'))?.text).toBe('milk, eggs');
      expect(a.crdt.pending).toBeGreaterThan(0);
      expect(b.crdt.pending).toBeGreaterThan(0);
      await expect(a.crdt.flush()).rejects.toThrow();
      // Back online.
      na.offline = false;
      nb.offline = false;
      await Promise.all([a.crdt.flush(), b.crdt.flush()]);
    } finally {
      na.restore();
      nb.restore();
    }
    const want = {
      id: card.id,
      board: board.id,
      column: doing!.id,
      title: 'groceries (sat)',
      text: 'milk, eggs',
      votes: 3,
    };
    for (const db of [a, b]) {
      const row = await crdtCard(db, card.id, 'server');
      expect(row).toMatchObject(want);
      expect([...row!.labels].sort()).toEqual(['home', 'shared', 'urgent']);
    }
    // The indexer moves it in the (column, pos) index.
    await until(
      async () =>
        (await crdtColumnCards(b, doing!.id, 'server')).some((c) => c.id === card.id) &&
        !(await crdtColumnCards(b, todo!.id, 'server')).some((c) => c.id === card.id),
      15_000,
      'the index to follow the move',
    );
  });

  it('two concurrent moves of a CRDT card end with one winner, never a duplicate', async () => {
    const [todo, doing, done] = await cols();
    const card = await crdtAddCard(a, board.id, todo!.id, 'contested');
    await a.crdt.flush();
    await Promise.all([crdtMove(a, card.id, doing!.id), crdtMove(b, card.id, done!.id)]);
    await Promise.all([a.crdt.flush(), b.crdt.flush()]);
    const row = await crdtCard(a, card.id, 'server');
    expect([doing!.id, done!.id]).toContain(row!.column);
    expect(await crdtCard(b, card.id, 'server')).toEqual(row);
    const where = async () => {
      const out: string[] = [];
      for (const col of [todo!, doing!, done!]) {
        const cs = await crdtColumnCards(b, col.id, 'server');
        out.push(...cs.filter((c) => c.id === card.id).map(() => col.id));
      }
      return out;
    };
    await until(async () => (await where()).join() === row!.column, 15_000, 'one column');
  });
});
