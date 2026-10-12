// The hello app's logic, over a `Db` and no DOM: notes in a table with a
// private index, an event per note on a topic, and one shared Loro text.
// The UI (ui/main.ts) and the tests (test/, e2e/) both call these.
import type { Db, DbOptions, Message, OnSubscription } from '@zen/db';
import { type LoroHandle, openLoro } from '@zen/loro';

/** A note row. */
export interface Note {
  [k: string]: string | number;
  id: string;
  text: string;
  /** Unix ms. */
  at: number;
}

/** The database namespace. */
export const NS = 'hello';

/** Schema and migrations, for `Db.open` (or `boot`). */
export const DB_OPTIONS: DbOptions = {
  schema: 1,
  migrations: {
    1: async (m) => {
      await m.createTable('notes', { pk: ['id'] });
      await m.createIndex('notes', 'by_at', { fields: [['at', 'int', 'desc']] });
    },
  },
};

/** The topic a note's event goes to. */
export const notesTopic = (db: Db) => db.topic('hello', 'notes');

/** Insert a note and emit its event, in one transaction. */
export async function addNote(db: Db, text: string): Promise<Note> {
  const note: Note = { id: crypto.randomUUID(), text, at: Date.now() };
  await db.transaction(async (tx) => {
    await tx.table<Note>('notes').insert(note);
    await tx.emit(notesTopic(db), 'note.added', { id: note.id, text });
  });
  return note;
}

/** The newest notes first. */
export async function listNotes(db: Db, limit = 50): Promise<Note[]> {
  const { rows } = await db.table<Note>('notes').query().orderBy('at', 'desc').limit(limit).page();
  return rows;
}

/** Call `fn` for each note event from now on. */
export function onNotes(
  db: Db,
  fn: (m: Message<{ id: string; text: string }>) => void,
): Promise<OnSubscription> {
  return db.on<{ id: string; text: string }>(notesTopic(db), (m) => {
    fn(m);
  });
}

/** The shared Loro document; its text is `doc.getText('text')`. */
export function openText(db: Db): Promise<LoroHandle> {
  return openLoro(db, 'hello/text');
}
