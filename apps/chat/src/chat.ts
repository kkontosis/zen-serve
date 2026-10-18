// The chat app's logic (docs/EXAMPLES.md §2), over a `Db` and a `Tree`, no
// DOM: rooms (a CRDT table, members a set), messages on a durable topic
// per room read from each device's receipt, typing indicators as ephemeral
// messages, "who's online" by scatter-gather, photos as fs files written in
// the transaction that announces them, and reactions as CRDT counters.
// The UI (ui/main.ts) and the tests (test/, e2e/) both call these.
import { bytes, ROOT, type Topic, type Tree, type UnlockedFs } from '@zen/client';
import type { Db, DbOptions, EphemeralMsg, Message, OnSubscription, SourceSub } from '@zen/db';

/** The database namespace. */
export const NS = 'chat';

/** The reactions, each a counter field of a `reactions` row. */
export const EMOJIS = { like: '👍', love: '❤️', laugh: '😂', party: '🎉' } as const;
export type Emoji = keyof typeof EMOJIS;
const EMOJI_NAMES = Object.keys(EMOJIS) as Emoji[];

/** A room: a CRDT row, `name` last-writer-wins, `members` an add-wins set. */
export interface Room {
  [k: string]: string | string[] | undefined;
  id: string;
  name?: string;
  members?: string[];
}

/** A device's read receipt in a room: the offset of the last message it read. */
export interface Receipt {
  [k: string]: string | Uint8Array;
  room: string;
  device: string;
  offset: Uint8Array;
}

/** A shared photo: an fs file in the photos tree, listed by room. */
export interface Blob {
  [k: string]: string | number | Uint8Array;
  id: string;
  room: string;
  /** The file's node in the photos tree. */
  node: Uint8Array;
  name: string;
  size: number;
}

/** The reactions of one message: a counter per emoji. */
export type Reactions = { [k: string]: string | number | undefined; message_id: string } & Partial<
  Record<Emoji, number>
>;

/** The body of a `msg` event. */
export interface MsgBody {
  [k: string]: string | undefined;
  text: string;
  /** The sender's display name (the event's `sender` is a device fingerprint). */
  from: string;
  /** The id (hex) of the message this one answers. */
  reply_to?: string;
  /** The `blobs` row of an attached photo. */
  photo?: string;
}

/** A message as the app shows it. */
export interface ChatMsg {
  text: string;
  from: string;
  reply_to?: string;
  photo?: string;
  /** The event's message id, hex: the key of its `reactions` row. */
  id: string;
  room: string;
  /** The event offset (for receipts). */
  offset: Uint8Array;
}

/** The offset before every event: `after` it is the whole history. */
export const START: Uint8Array = new Uint8Array(12);

/** Schema and migrations, for `Db.open` (or `boot`). */
export const DB_OPTIONS: DbOptions = {
  schema: 1,
  // No scheduled messages and no index on a CRDT table: neither the
  // scheduler nor the indexer is needed, nor the `consume` right they use.
  scheduler: false,
  indexer: false,
  migrations: {
    1: async (m) => {
      await m.createTable('rooms', {
        pk: ['id'],
        merge: 'crdt',
        fields: { members: 'set' },
        changes: {},
      });
      await m.createTable('receipts', { pk: ['room', 'device'] });
      await m.createTable('blobs', { pk: ['id'] });
      // Fast: lists a room's photos in one lookup, and shows the server how
      // many photos each room has (an accepted leak, EXAMPLES.md §2).
      await m.createIndex('blobs', 'by_room', { fields: [['room', 'text']], kind: 'fast' });
      await m.createTable('reactions', {
        pk: ['message_id'],
        merge: 'crdt',
        fields: Object.fromEntries(EMOJI_NAMES.map((e) => [e, 'counter' as const])),
        changes: {},
      });
    },
  },
};

// ------------------------------------------------------------ topics

/** A room's messages: durable. */
export const roomTopic = (db: Db, room: string): Topic => db.topic('chat', room);
/** A room's typing indicators: ephemeral, never in the log. */
export const typingTopic = (db: Db, room: string): Topic => db.topic('chat', room, 'typing');
/** Presence requests. */
export const presenceTopic = (db: Db): Topic => db.topic('presence');

/** This device, as receipts name it: the hex of the device fingerprint. */
export const deviceOf = (db: Db): string => bytes.hex(db.device);

// ------------------------------------------------------------ rooms

/**
 * Create a room (or join it, if it exists): a CRDT insert, so it works
 * offline and needs no transaction.
 */
export async function createRoom(db: Db, id: string, name: string, member: string): Promise<void> {
  await db.table<Room>('rooms').insert({ id, name, members: [member] });
}

/** Every room, by name. */
export async function listRooms(db: Db): Promise<Room[]> {
  const rows = await db.table<Room>('rooms').query().all();
  return rows.sort((a, b) => (a.name ?? a.id).localeCompare(b.name ?? b.id));
}

/** Add a member to a room (offline too). */
export function join(db: Db, room: string, member: string): Promise<void> {
  return db.table<Room>('rooms').add(room, 'members', member);
}

/** Remove a member from a room (offline too; an add it hasn't seen stays). */
export function leave(db: Db, room: string, member: string): Promise<void> {
  return db.table<Room>('rooms').remove(room, 'members', member);
}

/** Call `fn` when a room row changes, here or elsewhere. */
export function onRooms(db: Db, fn: () => void): Promise<OnSubscription> {
  return db.table('rooms').on(() => fn());
}

// ------------------------------------------------------------ messages

/** Send a message, alone: a transaction of its own. Returns its id (hex). */
export async function sendMessage(
  db: Db,
  room: string,
  from: string,
  text: string,
  opts: { replyTo?: string } = {},
): Promise<string> {
  const body: MsgBody = { text, from, ...(opts.replyTo ? { reply_to: opts.replyTo } : {}) };
  return bytes.hex(await db.emit(roomTopic(db, room), 'msg', body));
}

/** The tree photos are kept in: one per fs, the same id for every member. */
export async function photosTree(fs: UnlockedFs): Promise<Tree> {
  const d = await crypto.subtle.digest('SHA-256', new TextEncoder().encode('zen-apps/chat/photos'));
  return fs.tree(new Uint8Array(d).slice(0, 16));
}

/**
 * Send a photo with a message: the chunks are uploaded first (in commits
 * of their own, so a re-run costs only the operations), then one
 * transaction carries the file's creation and write, its `blobs` row and
 * the `msg` event. Either all of them are there, or none.
 */
export async function sendPhoto(
  db: Db,
  tree: Tree,
  room: string,
  from: string,
  file: { name: string; data: Uint8Array },
  text = '',
): Promise<{ id: string; blob: string }> {
  const blob = crypto.randomUUID();
  const b = tree.batch();
  const node = b.create(ROOT, `${blob}-${file.name}`);
  const up = await tree.upload(node, file.data);
  const id = bytes.randomBytes(16);
  await db.transaction(async (tx) => {
    const f = tx.fs(tree);
    await f.ops([...b.ops, tree.writeOpOf(up, [])]);
    await tx.table<Blob>('blobs').insert({
      id: blob,
      room,
      node,
      name: file.name,
      size: file.data.length,
    });
    const body: MsgBody = { text, from, photo: blob };
    await tx.emit(roomTopic(db, room), 'msg', body, { id });
  });
  return { id: bytes.hex(id), blob };
}

/** A room's photos, through the fast index on `blobs.room`. */
export async function roomPhotos(db: Db, room: string): Promise<Blob[]> {
  return db.table<Blob>('blobs').query().where('room', '=', room).all();
}

/** A photo's bytes. */
export async function readPhoto(db: Db, tree: Tree, blob: string): Promise<Uint8Array> {
  const b = await db.table<Blob>('blobs').get(blob);
  if (!b) throw new Error(`no photo ${blob}`);
  return tree.readFile(b.node);
}

/** A device's receipt in a room: the offset of the last message it read. */
export async function receipt(db: Db, room: string, device = deviceOf(db)): Promise<Uint8Array> {
  const r = await db.table<Receipt>('receipts').get([room, device]);
  return r?.offset ?? START;
}

/**
 * Mark a room read up to `offset`: one transaction on the device's
 * `receipts` row. A receipt never moves back.
 */
export async function markRead(
  db: Db,
  room: string,
  offset: Uint8Array,
  device = deviceOf(db),
): Promise<void> {
  await db.transaction(async (tx) => {
    const t = tx.table<Receipt>('receipts');
    const r = await t.get([room, device]);
    if (r && bytes.compare(r.offset, offset) >= 0) return;
    await t.put({ room, device, offset });
  });
}

function chatMsg(room: string, m: Message<MsgBody>): ChatMsg | undefined {
  if (m.type !== 'msg' || typeof m.body?.text !== 'string') return undefined;
  return { ...m.body, id: bytes.hex(m.id), room, offset: m.offset };
}

/**
 * Read a room: history after `after` (default: this device's receipt),
 * then live messages, in order. A dropped connection resumes from the last
 * offset received. The caller marks what it showed as read (`markRead`).
 */
export async function readRoom(
  db: Db,
  room: string,
  fn: (m: ChatMsg) => void,
  opts: { after?: Uint8Array; device?: string } = {},
): Promise<OnSubscription> {
  const after = opts.after ?? (await receipt(db, room, opts.device));
  return db.on<MsgBody>(
    roomTopic(db, room),
    (m) => {
      const c = chatMsg(room, m);
      if (c) fn(c);
    },
    { after },
  );
}

/**
 * Read a room and mark each message read as it is handled: a durable `on`
 * (`cursor`) whose transaction also moves the device's `receipts` row, so
 * the receipt and the cursor move together, exactly once. `fn` runs after
 * each commit (UI only). A first run starts after the receipt.
 */
export async function readRoomMarking(
  db: Db,
  room: string,
  fn: (m: ChatMsg) => void,
  opts: { device?: string } = {},
): Promise<OnSubscription> {
  const device = opts.device ?? deviceOf(db);
  const after = await receipt(db, room, device);
  return db.on<MsgBody>(
    roomTopic(db, room),
    async (m, tx) => {
      const c = chatMsg(room, m);
      await tx.table<Receipt>('receipts').put({ room, device, offset: m.offset });
      if (c) tx.after(() => fn(c));
    },
    { after, cursor: `read/${room}/${device}` },
  );
}

// ------------------------------------------------------------ typing

/** A typing indicator's body. */
export interface TypingBody {
  [k: string]: string;
  from: string;
}

/**
 * The sender of a room's typing indicators: `notify()` on every keystroke
 * publishes at most one ephemeral `typing` message per `intervalMs`.
 */
export class Typing {
  private last = Number.NEGATIVE_INFINITY;

  constructor(
    private readonly db: Db,
    readonly room: string,
    private readonly from: string,
    readonly intervalMs = 1000,
  ) {}

  /** Publish now unless one went out less than `intervalMs` ago; whether it did. */
  async notify(now = Date.now()): Promise<boolean> {
    if (now - this.last < this.intervalMs) return false;
    this.last = now;
    const body: TypingBody = { from: this.from };
    await this.db.publishEphemeral(typingTopic(this.db, this.room), 'typing', body);
    return true;
  }
}

/** How long a typing indicator shows after it arrives. */
export const TYPING_SHOW_MS = 3000;

/**
 * Call `fn` with the name of each member typing in a room, from now on.
 * An indicator sent more than `maxAgeMs` ago (by its sender's clock,
 * against this session's corrected clock) is dropped: one that arrives
 * late, after a reconnect, would show a member typing who no longer is.
 */
export function onTyping(
  db: Db,
  room: string,
  fn: (from: string) => void,
  maxAgeMs = TYPING_SHOW_MS,
): Promise<SourceSub> {
  return db.onEphemeral<TypingBody>(typingTopic(db, room), (m: EphemeralMsg<TypingBody>) => {
    if (m.type !== 'typing' || typeof m.body?.from !== 'string') return;
    const sentMs = Number(m.hlc >> 16n);
    if (db.fs.session.clock.now() - sentMs > maxAgeMs) return;
    fn(m.body.from);
  });
}

// ------------------------------------------------------------ presence

/** What a running instance answers a presence ping with. */
export interface Presence {
  [k: string]: string;
  user: string;
  device: string;
}

/**
 * Answer presence pings while this instance runs: a plain `on` from now,
 * replying ephemerally (the reply is never stored).
 */
export function servePresence(db: Db, user: string): Promise<OnSubscription> {
  const me: Presence = { user, device: deviceOf(db) };
  return db.on(presenceTopic(db), async (m) => {
    if (m.type === 'ping') await db.reply(m, me, { ephemeral: true });
  });
}

/** Who is online: every running instance's answer within `timeoutMs`. */
export async function whoIsOnline(db: Db, timeoutMs = 1500): Promise<Presence[]> {
  const replies = await db.requestAll<Presence>(presenceTopic(db), 'ping', {}, { timeoutMs });
  return replies.map((r) => r.body);
}

// ------------------------------------------------------------ reactions

/**
 * React to a message (`n` = -1 takes a reaction back): a CRDT counter, so
 * it works offline, and concurrent reactions from every installation add up.
 */
export async function react(db: Db, messageId: string, emoji: Emoji, n = 1): Promise<void> {
  await db.table<Reactions>('reactions').insert({ message_id: messageId, [emoji]: n });
}

/** A message's reaction counts (zeros for none). */
export async function reactions(
  db: Db,
  messageId: string,
  read?: 'auto' | 'cache' | 'server',
): Promise<Record<Emoji, number>> {
  const r = await db.table<Reactions>('reactions').get(messageId, read ? { read } : {});
  return Object.fromEntries(EMOJI_NAMES.map((e) => [e, Number(r?.[e] ?? 0)])) as Record<
    Emoji,
    number
  >;
}

/** Call `fn` with a message id whenever its reactions change. */
export function onReactions(db: Db, fn: (messageId: string) => void): Promise<OnSubscription> {
  return db.table<Reactions>('reactions').on((m) => {
    const pk = m.body.pk;
    const id = Array.isArray(pk) ? pk[0] : pk;
    if (typeof id === 'string') fn(id);
  });
}
