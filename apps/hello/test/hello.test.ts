// The hello app's logic in Node, with two members signed in by device key,
// and only the grants the app needs.
import type { Db } from '@zen/db';
import { afterAll, beforeAll, expect, it } from 'vitest';
import { spawnWorker, startWorld, until, type World, workerEnv } from '../../shared/harness.js';
import { signInWorker, type WorkerContext } from '../../shared/node.js';
import { addNote, DB_OPTIONS, listNotes, NS, onNotes, openText } from '../src/hello.js';
import { GRANTS } from './grants.js';

let w: World;
const workers: WorkerContext[] = [];
const dbs: Db[] = [];

beforeAll(async () => {
  w = await startWorld({ users: ['alice', 'bob'], grants: GRANTS });
  for (const name of ['alice', 'bob']) {
    const c = await signInWorker({
      url: w.url,
      device: w.user(name).deviceFile,
      passphrase: w.passphrase,
    });
    workers.push(c);
    dbs.push(await c.open(NS, DB_OPTIONS));
  }
});

afterAll(async () => {
  for (const c of workers) await c.close();
  await w?.stop();
});

it('a note and its event reach the other member', async () => {
  const [a, b] = dbs as [Db, Db];
  const seen: string[] = [];
  const sub = await onNotes(b, (m) => seen.push(m.body.text));
  await addNote(a, 'first');
  await addNote(a, 'second');
  await until(() => seen.length === 2, 15_000, 'two events');
  expect(seen).toEqual(['first', 'second']);
  expect((await listNotes(b)).map((n) => n.text)).toEqual(['second', 'first']);
  sub.close();
});

it('the shared text converges', async () => {
  const [a, b] = dbs as [Db, Db];
  const [da, db] = await Promise.all([openText(a), openText(b)]);
  await Promise.all([da.ready, db.ready]);
  da.doc.getText('text').insert(0, 'hello ');
  da.doc.commit();
  db.doc.getText('text').insert(0, 'world');
  db.doc.commit();
  const text = (d: typeof da) => d.doc.getText('text').toString();
  await until(() => text(da) === text(db) && text(da).length === 11, 15_000, 'convergence');
  da.close();
  db.close();
});

it('a Node worker follows the notes, and dies when killed', async () => {
  const p = spawnWorker('hello', 'listener', workerEnv(w, 'bob'));
  try {
    await p.waitFor('listening');
    await addNote(dbs[0]!, 'to the worker');
    await p.waitFor('note: to the worker');
  } finally {
    await p.kill();
  }
  expect(await p.exited).toBe('SIGKILL');
});
