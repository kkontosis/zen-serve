// The booking app's logic, over a `Db` and no DOM (EXAMPLES.md §3): slots
// of shared resources, one booking per slot (a unique index of kind `none`),
// a waitlist, reminders (delayed messages) and the waitlist promotion (a
// `per_key` consumer of `released`). The UI (ui/main.ts) and the tests
// (test/, e2e/) both call these.
import type { Topic } from '@zen/client';
import {
  type ConsumeOptions,
  type ConsumerHandle,
  type Db,
  DbError,
  type DbOptions,
  type DbTransaction,
  type Message,
  type OnSubscription,
} from '@zen/db';

/** The database namespace. */
export const NS = 'booking';

/** An hour, in ms: a reminder is due this long before the slot starts. */
export const HOUR = 3_600_000;

/** A bookable slot of a resource (a court, a car). */
export interface Slot {
  [k: string]: string | number;
  id: string;
  resource: string;
  /** Unix ms. */
  start: number;
  /** Unix ms. */
  end: number;
}

/** A booking: at most one per slot. */
export interface Booking {
  [k: string]: string | number | Uint8Array;
  id: string;
  slot: string;
  who: string;
  /** Unix ms. */
  created: number;
  /** The id of its scheduled reminder, which `cancel` takes. */
  reminder: Uint8Array;
}

/** A place on a slot's waitlist; the earliest `created` is the head. */
export interface WaitEntry {
  [k: string]: string | number;
  id: string;
  slot: string;
  who: string;
  /** Unix ms. */
  created: number;
}

/** The body of a `released` message. */
export interface Released {
  [k: string]: string;
  slot: string;
}

/** The body of a `notify` message (types `reminder` and `promoted`). */
export interface Notice {
  [k: string]: string | number;
  slot: string;
  who: string;
  booking: string;
  resource: string;
  start: number;
}

/** Schema and migrations, for `Db.open` (or `boot`). */
export const DB_OPTIONS: DbOptions = {
  schema: 1,
  migrations: {
    1: async (m) => {
      await m.createTable('slots', { pk: ['id'] });
      await m.createIndex('slots', 'by_resource_start', {
        fields: [
          ['resource', 'text'],
          ['start', 'int'],
        ],
      });
      await m.createTable('bookings', { pk: ['id'] });
      await m.createIndex('bookings', 'by_slot', {
        fields: [['slot', 'text']],
        unique: true,
        kind: 'none',
      });
      await m.createTable('waitlist', { pk: ['id'] });
      await m.createIndex('waitlist', 'by_slot_created', {
        fields: [
          ['slot', 'text'],
          ['created', 'int'],
        ],
      });
    },
  },
};

/** `("booking", "released")`, keyed by slot. */
export const releasedTopic = (db: Db): Topic => db.topic('booking', 'released');
/** `("booking", "notify")`: reminders and promotions. */
export const notifyTopic = (db: Db): Topic => db.topic('booking', 'notify');

// ------------------------------------------------------------ slots

/** Add a slot of `resource` from `start` to `end` (unix ms). */
export async function addSlot(db: Db, resource: string, start: number, end: number): Promise<Slot> {
  if (!resource) throw new DbError('bad_value', 'a slot needs a resource');
  if (!(end > start)) throw new DbError('bad_value', 'a slot ends after it starts');
  const slot: Slot = { id: crypto.randomUUID(), resource, start, end };
  await db.table<Slot>('slots').insert(slot);
  return slot;
}

/** A resource's slots, by start time (through the private index). */
export async function listSlots(db: Db, resource: string, limit = 200): Promise<Slot[]> {
  return db
    .table<Slot>('slots')
    .query()
    .where('resource', '=', resource)
    .orderBy('start')
    .limit(limit)
    .all();
}

/** The resources that have slots, in name order (one index range read). */
export async function listResources(db: Db, limit = 1000): Promise<string[]> {
  const rows = await db.table<Slot>('slots').query().orderBy('resource').limit(limit).all();
  return [...new Set(rows.map((s) => s.resource))];
}

// ------------------------------------------------------------ bookings

/** A slot's booking in `tx`, if any (a unique lookup: the server sees existence only). */
async function bookingIn(tx: DbTransaction, slot: string): Promise<Booking | undefined> {
  const [b] = await tx.table<Booking>('bookings').query().where('slot', '=', slot).limit(1).all();
  return b;
}

/** A slot's booking, if any. */
export function bookingOf(db: Db, slot: string): Promise<Booking | undefined> {
  return db.transaction((tx) => bookingIn(tx, slot));
}

/** The bookings of these slots (one unique lookup each), by slot id. */
export function bookingsOf(db: Db, slots: string[]): Promise<Map<string, Booking>> {
  return db.transaction(async (tx) => {
    const out = new Map<string, Booking>();
    for (const b of await Promise.all(slots.map((s) => bookingIn(tx, s)))) {
      if (b) out.set(b.slot, b);
    }
    return out;
  });
}

/**
 * Insert a booking of `slot` for `who` and schedule its reminder at
 * `start − 1h`, in `tx`. The unique entry of `slot` is read (into the read
 * set) and must be absent: otherwise `unique_violation`.
 */
async function bookIn(tx: DbTransaction, slot: Slot, who: string): Promise<Booking> {
  const reminder = crypto.getRandomValues(new Uint8Array(16));
  const b: Booking = { id: crypto.randomUUID(), slot: slot.id, who, created: Date.now(), reminder };
  // The insert reads the unique entry `("u", by_slot, slot)` (into the read
  // set) and fails with `unique_violation` if it names another booking.
  await tx.table<Booking>('bookings').insert(b);
  const notice: Notice = {
    slot: slot.id,
    who,
    booking: b.id,
    resource: slot.resource,
    start: slot.start,
  };
  await tx.schedule(notifyTopic(tx.db), 'reminder', notice, {
    id: reminder,
    at: Math.max(0, slot.start - HOUR),
    key: slot.id,
  });
  return b;
}

async function slotIn(tx: DbTransaction, slot: string): Promise<Slot> {
  const s = await tx.table<Slot>('slots').get(slot);
  if (!s) throw new DbError('not_found', `no slot ${slot}`);
  return s;
}

/**
 * Book `slot` for `who`. Two people booking one slot both read its unique
 * entry absent; the first commit wins, the other conflicts, runs again,
 * finds the entry and fails with `unique_violation`.
 */
export function book(db: Db, slot: string, who: string): Promise<Booking> {
  return db.transaction(async (tx) => bookIn(tx, await slotIn(tx, slot), who));
}

/**
 * Cancel the booking of `slot` (only `who`'s, if given): delete it (and its
 * unique entry), cancel its reminder and emit `released` keyed by the slot,
 * in one commit. False if there was none.
 */
export function cancel(db: Db, slot: string, who?: string): Promise<boolean> {
  return db.transaction(async (tx) => {
    const b = await bookingIn(tx, slot);
    if (!b) return false;
    if (who !== undefined && b.who !== who) {
      throw new DbError('bad_value', `the booking of ${slot} is not ${who}'s`);
    }
    await tx.table('bookings').delete(b.id);
    await tx.cancel(b.reminder);
    tx.emit(releasedTopic(tx.db), 'released', { slot } satisfies Released, { key: slot });
    return true;
  });
}

// ------------------------------------------------------------ the waitlist

/** Join `slot`'s waitlist. */
export async function joinWaitlist(db: Db, slot: string, who: string): Promise<WaitEntry> {
  const e: WaitEntry = { id: crypto.randomUUID(), slot, who, created: Date.now() };
  await db.transaction(async (tx) => {
    await slotIn(tx, slot);
    await tx.table<WaitEntry>('waitlist').insert(e);
  });
  return e;
}

function waitlistIn(tx: DbTransaction, slot: string, limit: number): Promise<WaitEntry[]> {
  return tx
    .table<WaitEntry>('waitlist')
    .query()
    .where('slot', '=', slot)
    .orderBy('created')
    .limit(limit)
    .all();
}

/** `slot`'s waitlist, head first (the private index `(slot, created)`). */
export function waitlistOf(db: Db, slot: string, limit = 100): Promise<WaitEntry[]> {
  return db.transaction((tx) => waitlistIn(tx, slot, limit));
}

/**
 * The waitlist promotion: a `per_key` consumer of `released` (one key per
 * slot, so the promotions of one slot run one at a time). Each delivery, in
 * one commit with the consume step: the head of the slot's waitlist gets the
 * booking (and its reminder), leaves the waitlist, and is notified. A slot
 * booked again meanwhile, or an empty waitlist, promotes no one.
 */
export function startPromotion(
  db: Db,
  opts: Partial<Omit<ConsumeOptions, 'topic' | 'mode'>> = {},
): ConsumerHandle {
  return db.consume<Released>(
    { group: 'promote', maxInflight: 4, ...opts, topic: releasedTopic(db), mode: 'per_key' },
    async (msg, tx) => {
      const slotId = msg.body.slot;
      if (await bookingIn(tx, slotId)) return;
      const [head] = await waitlistIn(tx, slotId, 1);
      if (!head) return;
      const slot = await tx.table<Slot>('slots').get(slotId);
      await tx.table('waitlist').delete(head.id);
      if (!slot) return;
      const b = await bookIn(tx, slot, head.who);
      const notice: Notice = {
        slot: slotId,
        who: head.who,
        booking: b.id,
        resource: slot.resource,
        start: slot.start,
      };
      tx.emit(notifyTopic(tx.db), 'promoted', notice, { key: slotId });
    },
  );
}

// ------------------------------------------------------------ notifications

/** Call `fn` for each notification (`reminder`, `promoted`) from now on; `who` filters. */
export function onNotify(
  db: Db,
  fn: (m: Message<Notice>) => void,
  who?: string,
): Promise<OnSubscription> {
  return db.on<Notice>(notifyTopic(db), (m) => {
    if (who === undefined || m.body.who === who) fn(m);
  });
}

/** Call `fn` for each `released` event from now on (to refresh a view). */
export function onReleased(db: Db, fn: (m: Message<Released>) => void): Promise<OnSubscription> {
  return db.on<Released>(releasedTopic(db), (m) => {
    fn(m);
  });
}
