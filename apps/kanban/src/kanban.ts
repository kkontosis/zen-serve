// The kanban app's logic, over a `Db` and a board's fs `Tree`, no DOM
// (docs/EXAMPLES.md §1). Boards, columns and cards are transactional rows;
// a card's position is a fractional key, so a move rewrites one card. Each
// board has an fs tree with a directory per card for its attachments: the
// chunks are uploaded first, then the card row, the file's create and write
// operations and the change event commit together.
//
// The offline variant, cards as a CRDT table, is in crdt.ts.
import {
  bytes,
  type PendingOp,
  ROOT,
  TRASH,
  Tree,
  type UnlockedFs,
  type UploadOptions,
} from '@zen/client';
import {
  type ChangeBody,
  type Db,
  DbError,
  type DbOptions,
  type DbTransaction,
  type EphemeralMsg,
  type Message,
  type OnSubscription,
  type SourceSub,
} from '@zen/db';
import { type LoroHandle, openLoro } from '@zen/loro';
import { keyBetween } from '../../shared/fractional.js';

/** The database namespace. */
export const NS = 'kanban';

export type Board = {
  id: string;
  name: string;
  /** The board's fs tree id (hex). */
  tree: string;
};

export type Column = {
  id: string;
  board: string;
  name: string;
  /** A fractional key (apps/shared/fractional.ts). */
  pos: string;
};

/** A file attached to a card: its node in the board's tree. */
export type Attachment = {
  /** Node id (hex). */
  node: string;
  name: string;
  size: number;
};

export type Card = {
  id: string;
  board: string;
  column: string;
  pos: string;
  title: string;
  /** A short description; the card's long text is a Loro document (`openCardText`). */
  text: string;
  /** The card's attachment directory in the board's tree (hex). */
  dir: string;
  attachments: Attachment[];
};

/** Schema and migrations, for `Db.open` (or `boot`). Version 2 adds the CRDT variant. */
export const DB_OPTIONS: DbOptions = {
  schema: 2,
  migrations: {
    1: async (m) => {
      await m.createTable('boards', { pk: ['id'] });
      await m.createTable('columns', { pk: ['id'] });
      await m.createIndex('columns', 'by_board_pos', {
        fields: [
          ['board', 'text'],
          ['pos', 'text'],
        ],
      });
      await m.createTable('cards', { pk: ['id'], changes: { image: 'full' } });
      await m.createIndex('cards', 'by_column_pos', {
        fields: [
          ['column', 'text'],
          ['pos', 'text'],
        ],
      });
    },
    2: async (m) => {
      // crdt.ts: the same cards, merged field by field by the server.
      await m.createTable('crdt_cards', {
        pk: ['id'],
        merge: 'crdt',
        fields: { labels: 'set', votes: 'counter' },
        changes: {},
      });
      await m.createIndex('crdt_cards', 'by_column_pos', {
        fields: [
          ['column', 'text'],
          ['pos', 'text'],
        ],
      });
    },
  },
};

/** The default columns of a new board. */
export const DEFAULT_COLUMNS = ['To do', 'Doing', 'Done'];

const notFound = (what: string) => new DbError('not_found', `no ${what}`);

// ------------------------------------------------------------------ boards

/**
 * The board `id`, created with its default columns if it doesn't exist.
 * One transaction: two members creating it at once end with one board.
 */
export function ensureBoard(db: Db, id: string, name = 'Board'): Promise<Board> {
  return db.transaction(async (tx) => {
    const boards = tx.table<Board>('boards');
    const had = await boards.get(id);
    if (had) return had;
    const board: Board = { id, name, tree: bytes.hex(Tree.newId()) };
    await boards.insert(board);
    let pos: string | undefined;
    for (const [i, n] of DEFAULT_COLUMNS.entries()) {
      pos = keyBetween(pos);
      await tx.table<Column>('columns').insert({ id: `${id}/${i}`, board: id, name: n, pos });
    }
    return board;
  });
}

export async function getBoard(db: Db, id: string): Promise<Board> {
  const b = await db.table<Board>('boards').get(id);
  if (!b) throw notFound(`board ${id}`);
  return b;
}

/** The board's fs tree (one per board). */
export function boardTree(fs: UnlockedFs, board: Board): Tree {
  return fs.tree(bytes.fromHex(board.tree));
}

/** The board's columns, in order. */
export function listColumns(db: Db, board: string): Promise<Column[]> {
  return db.table<Column>('columns').query().where('board', '=', board).orderBy('pos').all();
}

/** Add a column at the end. */
export function addColumn(db: Db, board: string, name: string): Promise<Column> {
  return db.transaction(async (tx) => {
    const cols = await tx
      .table<Column>('columns')
      .query()
      .where('board', '=', board)
      .orderBy('pos')
      .all();
    const col: Column = {
      id: crypto.randomUUID(),
      board,
      name,
      pos: keyBetween(cols.at(-1)?.pos),
    };
    await tx.table<Column>('columns').insert(col);
    return col;
  });
}

// ------------------------------------------------------------------ cards

/** A column's cards in order, through the `(column, pos)` index. */
export function columnCards(db: Db, column: string): Promise<Card[]> {
  return db.table<Card>('cards').query().where('column', '=', column).orderBy('pos').all();
}

/** Every column of a board with its cards. */
export async function boardView(
  db: Db,
  board: string,
): Promise<{ column: Column; cards: Card[] }[]> {
  const cols = await listColumns(db, board);
  return Promise.all(
    cols.map(async (column) => ({ column, cards: await columnCards(db, column.id) })),
  );
}

/** The key that puts a card at `index` among `others` (in order); the end by default. */
function posAt(others: { pos: string }[], index = others.length): string {
  const i = Math.max(0, Math.min(index, others.length));
  return keyBetween(others[i - 1]?.pos, others[i]?.pos);
}

/**
 * A new card at the end of a column, with its attachment directory: the
 * row, the `mkdir` and the change event in one transaction.
 */
export function addCard(
  db: Db,
  tree: Tree,
  column: Column,
  title: string,
  text = '',
): Promise<Card> {
  const id = crypto.randomUUID();
  return db.transaction(async (tx) => {
    const others = await tx
      .table<Card>('cards')
      .query()
      .where('column', '=', column.id)
      .orderBy('pos')
      .all();
    const b = tree.batch();
    const dir = b.mkdir(ROOT, id);
    await tx.fs(tree).ops(b);
    const card: Card = {
      id,
      board: column.board,
      column: column.id,
      pos: posAt(others),
      title,
      text,
      dir: bytes.hex(dir),
      attachments: [],
    };
    await tx.table<Card>('cards').insert(card);
    return card;
  });
}

/** Edit a card's title or short text. */
export function editCard(
  db: Db,
  id: string,
  fields: Partial<Pick<Card, 'title' | 'text'>>,
): Promise<Card> {
  return db.table<Card>('cards').update(id, (c) => ({ ...c, ...fields }));
}

/**
 * Move a card to `column`, at `index` among the column's other cards (the
 * end by default). The transaction reads the card and the column's index
 * range (its root) and rewrites only the card: a concurrent move of the
 * same card, or into the same place, conflicts and re-runs after it.
 */
export function moveCard(db: Db, id: string, column: string, index?: number): Promise<Card> {
  return db.transaction(async (tx) => {
    const cards = tx.table<Card>('cards');
    const card = await cards.get(id);
    if (!card) throw notFound(`card ${id}`);
    const others = (await cards.query().where('column', '=', column).orderBy('pos').all()).filter(
      (c) => c.id !== id,
    );
    const moved: Card = { ...card, column, pos: posAt(others, index) };
    await cards.put(moved);
    return moved;
  });
}

/**
 * Delete a card: the row goes, its attachment directory moves to TRASH
 * (one `move`), and the change event goes out, in one transaction.
 */
export function deleteCard(db: Db, tree: Tree, id: string): Promise<boolean> {
  return db.transaction(async (tx) => {
    const cards = tx.table<Card>('cards');
    const card = await cards.get(id);
    if (!card) return false;
    await cards.delete(id);
    if (card.dir) {
      // Read it first: that observes its timestamps, so this move orders
      // after its creation even when another device's clock runs ahead.
      await tree.getOne(bytes.fromHex(card.dir));
      await tx.fs(tree).ops([{ op: 'move', node: bytes.fromHex(card.dir), parent: TRASH }]);
    }
    return true;
  });
}

// ------------------------------------------------------------------ attachments

/** Options of `attachFile`. */
export interface AttachOptions {
  /** Passed to `tree.upload` (progress reports, chunk size). */
  upload?: Pick<UploadOptions, 'onProgress' | 'chunkSize'>;
}

/**
 * Attach a file to a card. The chunks are uploaded first, in commits of
 * their own (unreferenced until the write; collected after the grace period
 * if the tab dies now). Then one transaction: the card row with the new
 * attachment, the `move` creating the file node in the card's directory,
 * the `write` of its version, and the change event.
 */
export async function attachFile(
  db: Db,
  tree: Tree,
  cardId: string,
  name: string,
  data: Uint8Array | AsyncIterable<Uint8Array>,
  opts: AttachOptions = {},
): Promise<Attachment> {
  const card = await db.table<Card>('cards').get(cardId);
  if (!card) throw notFound(`card ${cardId}`);
  // The create op is built now, so the upload is sealed for its node id.
  const b = tree.batch();
  const node = b.create(bytes.fromHex(card.dir), name);
  const create: PendingOp[] = [...b.ops];
  const up = await tree.upload(node, data, opts.upload ?? {});
  const att: Attachment = { node: bytes.hex(node), name, size: up.manifest.size };
  await db.transaction(async (tx: DbTransaction) => {
    const cards = tx.table<Card>('cards');
    const c = await cards.get(cardId);
    if (!c) throw notFound(`card ${cardId}`);
    await tx.fs(tree).ops(create);
    await tx.fs(tree).write(up, { replaces: [] });
    await cards.put({ ...c, attachments: [...c.attachments, att] });
  });
  return att;
}

/** An attachment's bytes. */
export function readAttachment(tree: Tree, a: Attachment): Promise<Uint8Array> {
  return tree.readFile(bytes.fromHex(a.node));
}

// ------------------------------------------------------------------ live

/** The `cards` change topic (`zen.change` events, the row in full). */
export type CardChange = Message<ChangeBody<Card>>;

/**
 * Follow a board's card changes (`cards.on`), from `after` (the last
 * offset this device saw; absent: from now). Changes of other boards are
 * skipped; a delete carries no row, so it is always passed on.
 */
export function watchCards(
  db: Db,
  board: string,
  fn: (m: CardChange) => void,
  after?: Uint8Array,
): Promise<OnSubscription> {
  return db.table<Card>('cards').on(
    (m) => {
      const row = m.body.row;
      if (!row || row.board === board) fn(m);
    },
    after ? { after } : {},
  );
}

/** A cursor message on `("board", id)`: who is dragging what. */
export type Cursor = { user: string; card: string | null };

/** The board's ephemeral topic. */
export const boardTopic = (db: Db, board: string) => db.topic('board', board);

/** Say "I'm dragging `card`" (or `null`: stopped). Best effort, never stored. */
export function publishCursor(db: Db, board: string, c: Cursor): Promise<void> {
  return db.publishEphemeral(boardTopic(db, board), 'cursor', c);
}

/** Other members' cursors. */
export function onCursors(
  db: Db,
  board: string,
  fn: (m: EphemeralMsg<Cursor>) => void,
): Promise<SourceSub> {
  return db.onEphemeral<Cursor>(boardTopic(db, board), fn);
}

// ------------------------------------------------------------------ card text

/** A card's long text, a Loro document; the text is `doc.getText('text')`. */
export function openCardText(db: Db, card: string): Promise<LoroHandle> {
  return openLoro(db, `card/${card}`);
}
