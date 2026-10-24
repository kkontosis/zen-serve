// The chat app's logic in Node: members signed in by device key, with only
// the app's grants. Restarts are a new sign-in of the same device (a new
// session, the same receipts); offline is `net()` on a session.
import { bytes } from '@zen/client';
import { type Db, DirectSource, decodeMsg } from '@zen/db';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { net, sleep, startWorld, until, type World } from '../../shared/harness.js';
import { signInWorker, type WorkerContext } from '../../shared/node.js';
import {
  type ChatMsg,
  createRoom,
  DB_OPTIONS,
  join,
  leave,
  listRooms,
  markRead,
  NS,
  onTyping,
  type Presence,
  photosTree,
  react,
  reactions,
  readPhoto,
  readRoom,
  readRoomMarking,
  receipt,
  roomPhotos,
  sendMessage,
  sendPhoto,
  servePresence,
  showFor,
  TYPING_SHOW_MS,
  Typing,
  Typists,
  whoIsOnline,
} from '../src/chat.js';
import { GRANTS } from './grants.js';

let w: World;
const open = new Set<WorkerContext>();

interface Member {
  ctx: WorkerContext;
  db: Db;
}

/** Sign in as `name` (a new session: a restart of that device) and open the database. */
async function signIn(name: string): Promise<Member> {
  const ctx = await signInWorker({
    url: w.url,
    device: w.user(name).deviceFile,
    passphrase: w.passphrase,
  });
  open.add(ctx);
  return { ctx, db: await ctx.open(NS, { ...DB_OPTIONS, warn: () => {} }) };
}

async function signOut(m: Member): Promise<void> {
  open.delete(m.ctx);
  await m.ctx.close();
}

let n = 0;
const newRoom = () => `room-${++n}-${Date.now()}`;

beforeAll(async () => {
  w = await startWorld({ users: ['alice', 'bob', 'carol', 'dave'], grants: GRANTS });
});

afterAll(async () => {
  for (const c of open) await c.close();
  await w?.stop();
});

describe('messages', () => {
  it('a message, then a photo whose file, blobs row and msg commit together', async () => {
    const [a, b] = [await signIn('alice'), await signIn('bob')];
    const room = newRoom();
    await createRoom(a.db, room, 'Family', 'alice');
    const seen: ChatMsg[] = [];
    const sub = await readRoom(b.db, room, (m) => seen.push(m));
    const first = await sendMessage(a.db, room, 'alice', 'look at this');
    const data = new Uint8Array(150_000).map((_, i) => (i * 31) % 251);
    const photo = await sendPhoto(
      a.db,
      await photosTree(a.ctx.fs),
      room,
      'alice',
      { name: 'beach.jpg', data },
      'the beach',
    );
    await sendMessage(a.db, room, 'alice', 'nice?', { replyTo: photo.id });
    await until(() => seen.length === 3, 15_000, 'three messages');
    expect(seen.map((m) => [m.text, m.from])).toEqual([
      ['look at this', 'alice'],
      ['the beach', 'alice'],
      ['nice?', 'alice'],
    ]);
    expect(seen[0]!.id).toBe(first);
    expect(seen[1]).toMatchObject({ id: photo.id, photo: photo.blob });
    expect(seen[2]!.reply_to).toBe(photo.id);
    // Each carries when it was sent, on the sender's clock.
    for (const m of seen) expect(Math.abs(m.sentMs - Date.now())).toBeLessThan(60_000);
    // The room's photos, through the fast index, and the file's bytes.
    const photos = await roomPhotos(b.db, room);
    expect(photos.map((p) => [p.id, p.name, p.size])).toEqual([[photo.blob, 'beach.jpg', 150_000]]);
    expect(await roomPhotos(b.db, newRoom())).toEqual([]);
    const got = await readPhoto(b.db, await photosTree(b.ctx.fs), photo.blob);
    expect(bytes.equal(got, data)).toBe(true);
    await sub.close();
    await signOut(a);
    await signOut(b);
  });

  it('a reader resumes from its receipt after a restart: no gap, no duplicate', async () => {
    const a = await signIn('alice');
    let b = await signIn('bob');
    const room = newRoom();
    const seen: string[] = [];
    const offsets: string[] = [];
    // Receipt writes run behind the display; a restart waits for them, so no
    // transaction is left running on a closed fs.
    const pending = new Set<Promise<unknown>>();
    const settle = () => Promise.allSettled([...pending]);
    const show = (db: Db) => (m: ChatMsg) => {
      seen.push(m.text);
      offsets.push(bytes.hex(m.offset));
      const p = markRead(db, room, m.offset);
      pending.add(p);
      void p.finally(() => pending.delete(p)).catch(() => {});
    };
    let sub = await readRoom(b.db, room, show(b.db));
    for (let i = 1; i <= 5; i++) await sendMessage(a.db, room, 'alice', `m${i}`);
    await until(() => seen.length === 5, 15_000, 'the first five');
    // The receipt reaches the fifth message (markRead runs behind the display).
    await until(
      async () => bytes.hex(await receipt(b.db, room)) === offsets[4],
      15_000,
      'the receipt of the fifth',
    );
    await sub.close();
    await settle();
    await signOut(b);
    // Bob's device is off: Alice goes on.
    for (let i = 6; i <= 10; i++) await sendMessage(a.db, room, 'alice', `m${i}`);
    // Restart: a new session of the same device reads from its receipt.
    b = await signIn('bob');
    sub = await readRoom(b.db, room, show(b.db));
    await until(() => seen.length === 10, 15_000, 'the history after the receipt');
    await sendMessage(a.db, room, 'alice', 'm11');
    await until(() => seen.length === 11, 15_000, 'a live message');
    await sleep(300);
    expect(seen).toEqual(Array.from({ length: 11 }, (_, i) => `m${i + 1}`));
    await sub.close();
    await settle();
    await signOut(a);
    await signOut(b);
  });

  it('a durable reader marks as it reads: a restart in mid-stream loses and repeats nothing', async () => {
    const a = await signIn('alice');
    let b = await signIn('bob');
    const room = newRoom();
    const seen: string[] = [];
    let sub = await readRoomMarking(b.db, room, (m) => seen.push(m.text));
    // Alice sends ten, then ten more once Bob's device is down: Bob stops
    // while messages are still arriving, and some are sent while he is away.
    let down!: () => void;
    const away = new Promise<void>((r) => {
      down = r;
    });
    const sending = (async () => {
      for (let i = 1; i <= 10; i++) await sendMessage(a.db, room, 'alice', `m${i}`);
      await away;
      for (let i = 11; i <= 20; i++) await sendMessage(a.db, room, 'alice', `m${i}`);
    })();
    await until(() => seen.length >= 3, 15_000, 'some messages');
    await sub.close();
    await signOut(b);
    const before = seen.length;
    expect(before).toBeGreaterThanOrEqual(3);
    expect(before).toBeLessThanOrEqual(10);
    down();
    await sending;
    b = await signIn('bob');
    sub = await readRoomMarking(b.db, room, (m) => seen.push(m.text));
    await until(() => seen.length >= 20, 15_000, 'every message');
    await sleep(300);
    expect(seen).toEqual(Array.from({ length: 20 }, (_, i) => `m${i + 1}`));
    // The receipt is the cursor: the last message's offset.
    const events = [];
    for await (const e of a.ctx.fs.topic('chat', room).read()) events.push(e);
    expect(bytes.hex(await receipt(b.db, room))).toBe(bytes.hex(events.at(-1)!.offset));
    await sub.close();
    await signOut(a);
    await signOut(b);
  });

  it('a dropped connection resumes from the last offset received', async () => {
    const [a, b] = [await signIn('alice'), await signIn('bob')];
    const room = newRoom();
    const seen: string[] = [];
    // What happened, in order, for a failure: what arrived, and the reconnects.
    const events: string[] = [];
    const sub = await readRoom(b.db, room, (m) => {
      seen.push(m.text);
      events.push(`got ${m.text}`);
    });
    for (let i = 1; i <= 12; i++) {
      await sendMessage(a.db, room, 'alice', `m${i}`);
      events.push(`sent m${i}`);
      if (i % 4 === 0) {
        await (b.db.source as DirectSource).reconnect();
        events.push('reconnect');
      }
    }
    await until(() => seen.length >= 12, 15_000, 'every message').catch((e: Error) => {
      throw new Error(`${e.message}; in order: ${events.join(', ')}`);
    });
    await sleep(300);
    expect(seen).toEqual(Array.from({ length: 12 }, (_, i) => `m${i + 1}`));
    expect(b.db.source).toBeInstanceOf(DirectSource);
    await sub.close();
    await signOut(a);
    await signOut(b);
  });
});

describe('typing and presence', () => {
  it('typing reaches the others, rate-limited on the sender, and never the log', async () => {
    const [a, b] = [await signIn('alice'), await signIn('bob')];
    const room = newRoom();
    const typing: string[] = [];
    const sub = await onTyping(b.db, room, (from) => typing.push(from));
    const t = new Typing(a.db, room, 'alice', 1000);
    const sent: boolean[] = [];
    for (let i = 0; i < 10; i++) sent.push(await t.notify());
    expect(sent.filter(Boolean).length).toBe(1);
    await until(() => typing.length === 1, 15_000, 'a typing indicator');
    await sleep(1000);
    expect(await t.notify()).toBe(true);
    await until(() => typing.length === 2, 15_000, 'a second one');
    expect(typing).toEqual(['alice', 'alice']);
    await sendMessage(a.db, room, 'alice', 'done typing');
    // After a message, the next keystroke begins another: announced at once.
    t.reset();
    expect(await t.notify()).toBe(true);
    // The log: no event on the typing topic, and only `msg` on the room's.
    const typingLog = [];
    for await (const e of b.ctx.fs.topic('chat', room, 'typing').read()) typingLog.push(e);
    expect(typingLog).toEqual([]);
    const types: string[] = [];
    for await (const e of b.ctx.fs.topic('chat', room).read())
      types.push(decodeMsg(e.payload).type);
    expect(types).toEqual(['msg']);
    await sub.close();
    await signOut(a);
    await signOut(b);
  });

  it('a typing indicator older than its display time is dropped', async () => {
    const [a, b] = [await signIn('alice'), await signIn('bob')];
    const room = newRoom();
    const fresh: string[] = [];
    const sent: number[] = [];
    const strict: string[] = [];
    const subs = [
      await onTyping(b.db, room, (from, sentMs) => {
        fresh.push(from);
        sent.push(sentMs);
      }),
      // A bound a minute below zero: every indicator counts as too old,
      // whatever the two clocks say.
      await onTyping(b.db, room, (from) => strict.push(from), -60_000),
    ];
    expect(await new Typing(a.db, room, 'alice', 1000).notify()).toBe(true);
    await until(() => fresh.length === 1, 15_000, 'the indicator');
    await sleep(300);
    expect(fresh).toEqual(['alice']);
    expect(strict).toEqual([]);
    // It shows until its display time after it was sent, not after it arrived.
    const left = showFor(b.db, sent[0]!);
    expect(left).toBeLessThanOrEqual(TYPING_SHOW_MS - 300);
    expect(showFor(b.db, sent[0]! - TYPING_SHOW_MS)).toBeLessThanOrEqual(0);
    for (const s of subs) await s.close();
    await signOut(a);
    await signOut(b);
  });

  it("a message ends its sender's typing indicator, and one sent before it stays ended", () => {
    const t = new Typists();
    const now = 1_000_000;
    expect(t.typing('alice', now - 100, now)).toBe(now - 100 + TYPING_SHOW_MS);
    expect(t.typing('bob', now, now)).toBe(now + TYPING_SHOW_MS);
    expect(t.names).toEqual(['alice', 'bob']);
    // Alice's message, sent after her indicator, ends it.
    expect(t.sent('alice', now + 50)).toBe(true);
    expect(t.names).toEqual(['bob']);
    // Her indicator from before it, arriving late, doesn't bring it back;
    // one sent after it is for her next message.
    expect(t.typing('alice', now + 40, now + 60)).toBeUndefined();
    expect(t.typing('alice', now + 80, now + 90)).toBe(now + 80 + TYPING_SHOW_MS);
    // A message sent before Bob's newest indicator leaves it.
    expect(t.sent('bob', now - 10)).toBe(false);
    expect(t.names).toEqual(['bob', 'alice']);
    // An indicator already over, or older than the one shown, changes nothing.
    expect(t.typing('carol', now - TYPING_SHOW_MS, now)).toBeUndefined();
    expect(t.typing('bob', now - 5, now)).toBeUndefined();
    // The timer of a replaced indicator doesn't end the newer one.
    const later = t.typing('bob', now + 500, now + 500);
    expect(t.end('bob', now + TYPING_SHOW_MS)).toBe(false);
    expect(t.end('bob', later!)).toBe(true);
    expect(t.names).toEqual(['alice']);
  });

  it("who's online: every running instance answers, and a closed one no longer", async () => {
    // Four devices. (Two sessions of one device would not do: the stream
    // drops an ephemeral message whose sender's clock is not past the last
    // one it saw from that device, and each session has its own clock.)
    const ms = [await signIn('alice'), await signIn('bob'), await signIn('carol')];
    const fourth = await signIn('dave');
    const subs = await Promise.all([
      ...ms.map((m, i) => servePresence(m.db, ['alice', 'bob', 'carol'][i]!)),
      servePresence(fourth.db, 'dave'),
    ]);
    const names = (ps: Presence[]) => ps.map((p) => p.user).sort();
    const all = ['alice', 'bob', 'carol', 'dave'];
    const first = names(await whoIsOnline(ms[1]!.db));
    // For a failure: asked again with a longer wait, a late answer is a slow
    // reply; none is a ping or a reply lost.
    const again = first.length < all.length ? names(await whoIsOnline(ms[1]!.db, 10_000)) : first;
    expect(first, `asked again with 10 s: ${again.join(', ')}`).toEqual(all);
    await subs[3]!.close();
    await signOut(fourth);
    await subs[2]!.close();
    await signOut(ms[2]!);
    expect(names(await whoIsOnline(ms[0]!.db))).toEqual(['alice', 'bob']);
    for (const s of subs.slice(0, 2)) await s.close();
    await signOut(ms[0]!);
    await signOut(ms[1]!);
  });
});

describe('CRDT: reactions and membership', () => {
  it('reactions add up across installations, concurrent and offline', async () => {
    const [a, b] = [await signIn('alice'), await signIn('bob')];
    const [na, nb] = [net(a.ctx.session), net(b.ctx.session)];
    const room = newRoom();
    const msg = await sendMessage(a.db, room, 'alice', 'react to me');
    // Concurrent, online: 5 from each.
    await Promise.all([
      ...Array.from({ length: 5 }, () => react(a.db, msg, 'like')),
      ...Array.from({ length: 5 }, () => react(b.db, msg, 'like')),
    ]);
    await Promise.all([a.db.crdt.flush(), b.db.crdt.flush()]);
    expect((await reactions(a.db, msg, 'server')).like).toBe(10);
    expect((await reactions(b.db, msg, 'server')).like).toBe(10);
    // Offline, both: queued, shown over the cached state, summed once back.
    na.offline = true;
    nb.offline = true;
    await react(a.db, msg, 'like', 2);
    await react(a.db, msg, 'party');
    await react(b.db, msg, 'like', 3);
    await react(b.db, msg, 'love');
    await react(b.db, msg, 'love', -1);
    expect(a.db.crdt.pending).toBeGreaterThan(0);
    expect((await reactions(a.db, msg)).like).toBe(12);
    expect((await reactions(b.db, msg)).like).toBe(13);
    na.offline = false;
    nb.offline = false;
    await Promise.all([a.db.crdt.flush(), b.db.crdt.flush()]);
    const want = { like: 15, love: 0, laugh: 0, party: 1 };
    expect(await reactions(a.db, msg, 'server')).toEqual(want);
    expect(await reactions(b.db, msg, 'server')).toEqual(want);
    na.restore();
    nb.restore();
    await signOut(a);
    await signOut(b);
  });

  it('membership is a set: joins and leaves made offline merge, add wins', async () => {
    const [a, b] = [await signIn('alice'), await signIn('bob')];
    const [na, nb] = [net(a.ctx.session), net(b.ctx.session)];
    const room = newRoom();
    await createRoom(a.db, room, 'Cousins', 'alice');
    await a.db.crdt.flush();
    await until(
      async () => (await listRooms(b.db)).some((r) => r.id === room),
      15_000,
      'the room at bob',
    );
    na.offline = true;
    nb.offline = true;
    // Alice leaves; Bob joins and adds Alice back, which Alice never saw.
    await leave(a.db, room, 'alice');
    await join(b.db, room, 'bob');
    await join(b.db, room, 'carol');
    na.offline = false;
    nb.offline = false;
    await Promise.all([a.db.crdt.flush(), b.db.crdt.flush()]);
    const get = async (db: Db) =>
      (await db.table('rooms').get(room, { read: 'server' }))?.members as string[];
    expect((await get(a.db)).sort()).toEqual(['bob', 'carol']);
    // Add wins: Bob adds Carol again offline while Alice removes the copy
    // she saw. Bob's add is one Alice's remove never saw, so Carol stays.
    await get(a.db);
    nb.offline = true;
    await join(b.db, room, 'carol');
    await leave(a.db, room, 'carol');
    await a.db.crdt.flush();
    expect((await get(a.db)).sort()).toEqual(['bob']);
    nb.offline = false;
    await b.db.crdt.flush();
    expect((await get(a.db)).sort()).toEqual(['bob', 'carol']);
    expect((await get(b.db)).sort()).toEqual(['bob', 'carol']);
    na.restore();
    nb.restore();
    await signOut(a);
    await signOut(b);
  });
});
