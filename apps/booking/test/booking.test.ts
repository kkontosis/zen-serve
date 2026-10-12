// The booking app's logic in Node (EXAMPLES.md §3, the milestone 5 demo):
// each Db is its own signed-in session, with only the app's grants.
import type { Db, DbError, DbOptions, Message } from '@zen/db';
import { afterAll, beforeAll, expect, it } from 'vitest';
import { sleep, startWorld, until, type World } from '../../shared/harness.js';
import { signInWorker, type WorkerContext } from '../../shared/node.js';
import {
  addSlot,
  book,
  bookingOf,
  cancel,
  DB_OPTIONS,
  HOUR,
  joinWaitlist,
  listResources,
  listSlots,
  type Notice,
  NS,
  onNotify,
  startPromotion,
  waitlistOf,
} from '../src/booking.js';
import { GRANTS } from './grants.js';

const USERS = ['alice', 'bob', 'carol'];
/** A fast scheduler: a 1 s lease, reads every 100 ms. */
const FAST = { schedulerOptions: { pollMs: 100, ttlMs: 1000 } };

let w: World;
let open: WorkerContext[] = [];

beforeAll(async () => {
  w = await startWorld({
    users: USERS,
    grants: GRANTS,
    server: { limits: { claim_ttl_ms: 1000 } },
  });
});

afterAll(async () => {
  await closeAll();
  await w?.stop();
});

/** Close every session opened so far (their schedulers and consumers stop). */
async function closeAll(): Promise<void> {
  const cs = open;
  open = [];
  await Promise.all(cs.map((c) => c.close()));
}

/** A Db in a session of its own, signed in as `user`. */
async function dbAs(user: string, extra: DbOptions = {}): Promise<Db> {
  const c = await signInWorker({
    url: w.url,
    device: w.user(user).deviceFile,
    passphrase: w.passphrase,
  });
  open.push(c);
  return c.open(NS, { ...DB_OPTIONS, warn: () => {}, ...extra });
}

const now = (db: Db) => db.fs.session.clock.now();

/** A slot of `resource` starting `inMs` from now, for an hour. */
const slotIn = (db: Db, resource: string, inMs: number) => {
  const start = Math.ceil(now(db) + inMs);
  return addSlot(db, resource, start, start + HOUR);
};

it('slots by resource, in start order', async () => {
  try {
    const db = await dbAs('alice', { scheduler: false });
    const t = now(db) + 2 * HOUR;
    await addSlot(db, 'list-car', t + 2 * HOUR, t + 3 * HOUR);
    await addSlot(db, 'list-car', t, t + HOUR);
    await addSlot(db, 'list-court', t, t + HOUR);
    expect((await listSlots(db, 'list-car')).map((s) => s.start)).toEqual([t, t + 2 * HOUR]);
    expect(await listResources(db)).toEqual(expect.arrayContaining(['list-car', 'list-court']));
  } finally {
    await closeAll();
  }
});

it('twenty concurrent bookings of one slot: exactly one succeeds', async () => {
  try {
    const first = await dbAs('alice', { scheduler: false });
    const rest = await Promise.all(
      Array.from({ length: 19 }, (_, i) =>
        dbAs(USERS[(i + 1) % USERS.length]!, { scheduler: false }),
      ),
    );
    const dbs = [first, ...rest];
    const slot = await slotIn(first, 'race-court', 3 * HOUR);
    const results = await Promise.allSettled(dbs.map((db, i) => book(db, slot.id, `p${i}`)));
    const won = results.flatMap((r) => (r.status === 'fulfilled' ? [r.value] : []));
    const lost = results.flatMap((r) => (r.status === 'rejected' ? [r.reason as DbError] : []));
    expect(won).toHaveLength(1);
    expect(lost.map((e) => e.code)).toEqual(Array(19).fill('unique_violation'));
    // Every session sees the one booking.
    const seen = await Promise.all(dbs.map((db) => bookingOf(db, slot.id)));
    expect(new Set(seen.map((b) => b?.id))).toEqual(new Set([won[0]!.id]));
  } finally {
    await closeAll();
  }
});

it('a cancel promotes the head of the waitlist exactly once, with two consumers', async () => {
  try {
    const a = await dbAs('alice', { scheduler: false });
    const b = await dbAs('bob', { scheduler: false });
    const c = await dbAs('carol', { scheduler: false });
    const slot = await slotIn(a, 'promo-court', 3 * HOUR);
    await book(a, slot.id, 'alice');
    await expect(book(b, slot.id, 'bob')).rejects.toMatchObject({ code: 'unique_violation' });
    await joinWaitlist(b, slot.id, 'bob');
    await sleep(5);
    await joinWaitlist(c, slot.id, 'carol');
    expect((await waitlistOf(a, slot.id)).map((e) => e.who)).toEqual(['bob', 'carol']);

    const notices: Message<Notice>[] = [];
    await onNotify(a, (m) => {
      if (m.body.slot === slot.id) notices.push(m);
    });
    // Two instances of the promotion consumer (one group).
    startPromotion(b);
    startPromotion(c);

    expect(await cancel(a, slot.id, 'alice')).toBe(true);
    await until(async () => (await bookingOf(a, slot.id))?.who === 'bob', 20_000, 'bob promoted');
    await until(() => notices.length >= 1, 10_000, 'the notice');
    // Time for a duplicate to show, if there were one.
    await sleep(2000);
    expect(notices.map((m) => [m.type, m.body.who])).toEqual([['promoted', 'bob']]);
    expect((await waitlistOf(a, slot.id)).map((e) => e.who)).toEqual(['carol']);

    // The next release promotes the next in line, once.
    await expect(cancel(a, slot.id, 'alice')).rejects.toMatchObject({ code: 'bad_value' });
    expect(await cancel(b, slot.id, 'bob')).toBe(true);
    await until(
      async () => (await bookingOf(a, slot.id))?.who === 'carol',
      20_000,
      'carol promoted',
    );
    await until(() => notices.length >= 2, 10_000, 'the second notice');
    await sleep(2000);
    expect(notices.map((m) => [m.type, m.body.who])).toEqual([
      ['promoted', 'bob'],
      ['promoted', 'carol'],
    ]);
    expect(await waitlistOf(a, slot.id)).toEqual([]);

    // A release with an empty waitlist promotes no one.
    expect(await cancel(c, slot.id, 'carol')).toBe(true);
    await sleep(2000);
    expect(await bookingOf(a, slot.id)).toBeUndefined();
    expect(notices).toHaveLength(2);
  } finally {
    await closeAll();
  }
});

it('a cancel cancels the reminder', async () => {
  try {
    const db = await dbAs('alice', FAST);
    const notices: Message<Notice>[] = [];
    await onNotify(db, (m) => notices.push(m));
    const kept = await slotIn(db, 'remind-court', HOUR + 800);
    const dropped = await slotIn(db, 'remind-court', HOUR + 800);
    await book(db, kept.id, 'alice');
    await book(db, dropped.id, 'alice');
    expect(await cancel(db, dropped.id)).toBe(true);
    await until(() => notices.length >= 1, 10_000, 'the reminder');
    await sleep(1500);
    expect(notices.map((m) => [m.type, m.body.slot])).toEqual([['reminder', kept.id]]);
    expect(notices[0]!.deliverAt).toBe(kept.start - HOUR);
  } finally {
    await closeAll();
  }
});

it('the scheduler leader killed before a reminder is due: another sends it late, once', async () => {
  try {
    const a = await dbAs('alice', FAST);
    const b = await dbAs('bob', FAST);
    const watcher = await dbAs('carol', { scheduler: false });
    await until(() => a.scheduler!.leading || b.scheduler!.leading, 10_000, 'a leader');
    const [lead, other] = a.scheduler!.leading ? [a, b] : [b, a];

    const got: { m: Message<Notice>; at: number }[] = [];
    await onNotify(watcher, (m) => got.push({ m, at: now(watcher) }));
    const slot = await slotIn(lead, 'failover-car', HOUR + 2500);
    const due = slot.start - HOUR;
    const booking = await book(lead, slot.id, 'alice');

    // The leader dies shortly before the reminder is due: its lease is kept
    // (not released), so the other instance takes over only once it lapses.
    await sleep(Math.max(0, due - 300 - now(lead)));
    expect(lead.scheduler!.leading).toBe(true);
    expect(got).toEqual([]);
    await lead.scheduler!.stop({ release: false });

    await until(() => got.length >= 1, 15_000, 'the reminder');
    await sleep(1500);
    expect(got.map((g) => [g.m.type, g.m.body.booking])).toEqual([['reminder', booking.id]]);
    expect(other.scheduler!.leading).toBe(true);
    expect(got[0]!.m.deliverAt).toBe(due);
    // Late: the dead leader's lease (1 s, renewed every third of it) had to
    // lapse first, at least ~0.35 s after the due time; a live leader is late
    // by at most `pollMs` (0.1 s).
    expect(got[0]!.at - due).toBeGreaterThan(200);
    // Delivered: nothing left to cancel.
    expect(await cancel(other, slot.id)).toBe(true);
    expect(await other.cancel(booking.reminder)).toBe(false);
  } finally {
    await closeAll();
  }
});
