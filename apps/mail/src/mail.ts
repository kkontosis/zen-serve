// The mail app: inbound SMTP into zen-db (smtp.ts), and what reads it back
// and changes it: IMAP (imap.ts) and the inbox page share the helpers
// here. The SMTP service is `svc/smtp`, served with `starttls()` and the
// certificate of `certs`; with a sealed inbox (svc.md §9), the seed is in
// `certs` too.
import type { Db, DbTransaction, OnSubscription } from '@zen/db';
import { type Layer, type ServeHandle, type ServeOptions, serve, starttls } from '@zen/svc';
import { scanHeaderEnd } from './header.js';
import {
  type AppPassword,
  type Cert,
  type Changed,
  type Counter,
  changedTopic,
  type Draft,
  type Mailbox,
  type Message,
  PART_BYTES,
  type Part,
  receivedTopic,
  TLS_CERT,
} from './schema.js';
import { type SmtpOptions, smtp } from './smtp.js';

export { headerLines, parseFields, scanHeaderEnd } from './header.js';
export * from './schema.js';
export { decodeData, MAX_LINE, MAX_RCPTS, MAX_SIZE, type SmtpOptions, smtp } from './smtp.js';

/** The inbound SMTP service. */
export const SMTP_SERVICE = ['svc', 'smtp'];

/** `starttls()` with the certificate of `certs`. */
export async function smtpLayer(db: Db): Promise<Layer> {
  const c = await db.table<Cert>('certs').get(TLS_CERT);
  if (!c) throw new Error(`no certificate ${TLS_CERT} in certs`);
  return starttls({ cert: c.chain, key: c.key });
}

/** Serve inbound SMTP from this database. */
export async function serveSmtp(
  db: Db,
  opts: Partial<ServeOptions> & { smtp: SmtpOptions },
): Promise<ServeHandle> {
  const { smtp: o, ...rest } = opts;
  return serve(db, {
    service: SMTP_SERVICE,
    layer: await smtpLayer(db),
    ...rest,
    handler: smtp(o),
  });
}

/** A message's bodies' parts, in order. */
async function body(db: Db, draft: string): Promise<Uint8Array[]> {
  const parts = await db
    .table<Part>('parts')
    .query()
    .where('draft', '=', draft)
    .orderBy('seq')
    .all();
  return parts.map((p) => p.data);
}

/** A stored message as received: its `Received:` header, then the body. */
export async function messageBytes(db: Db, user: string, uid: number): Promise<Uint8Array> {
  const m = await db.table<Message>('messages').get([user, uid]);
  if (!m) throw new Error(`no message ${uid} for ${user}`);
  const pieces = [new TextEncoder().encode(m.head), ...(await body(db, m.draft))];
  const out = new Uint8Array(pieces.reduce((a, p) => a + p.length, 0));
  let at = 0;
  for (const p of pieces) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

/** A user's messages, by uid. */
export function messages(db: Db, user: string): Promise<Message[]> {
  return db.table<Message>('messages').query().where('user', '=', user).orderBy('uid').all();
}

/**
 * Delete the drafts older than `olderThanMs`, with their parts: what a
 * connection that died mid-DATA left (a peer that went away cleanly gets
 * its draft deleted at once). Returns how many it deleted.
 */
export async function sweepDrafts(db: Db, olderThanMs: number): Promise<number> {
  const before = Date.now() - olderThanMs;
  const old = (await db.table<Draft>('drafts').query().all()).filter((d) => d.started <= before);
  for (const d of old) {
    const seqs = (
      await db.table<Part>('parts').query().where('draft', '=', d.draft).orderBy('seq').all()
    ).map((p) => p.seq);
    await db.transaction(async (tx) => {
      for (const seq of seqs) await tx.table<Part>('parts').delete([d.draft, seq]);
      await tx.table<Draft>('drafts').delete(d.draft);
    });
  }
  return old.length;
}

// ------------------------------------------------------------ flags and expunge

/** The system flags IMAP keeps (RFC 3501 §2.3.2); `\\Recent` isn't kept. */
export const SYSTEM_FLAGS = ['\\Seen', '\\Answered', '\\Flagged', '\\Deleted', '\\Draft'];

/** How `setFlags` changes a message's flags: replace them, add or remove some. */
export type FlagOp = 'set' | 'add' | 'remove';

/** A message's flags after a change, and the change counter it took. */
export interface FlagChange {
  uid: number;
  flags: string[];
  modseq: number;
}

/** Take the next value of a user's change counter. */
async function nextModseq(tx: DbTransaction, user: string): Promise<number> {
  const counters = tx.table<Counter>('counters');
  const c = await counters.get(user);
  const modseq = (c?.modseq ?? 0) + 1;
  await counters.put({ ...(c ?? { user, next: 1 }), modseq });
  return modseq;
}

/**
 * Change the flags of a user's messages: each message whose flags change
 * takes the next change counter (`modseq`), and `mail/changed` announces
 * it. Missing uids are skipped. Returns the messages changed.
 */
export async function setFlags(
  tx: DbTransaction,
  user: string,
  uids: number[],
  op: FlagOp,
  flags: string[],
): Promise<FlagChange[]> {
  const table = tx.table<Message>('messages');
  const out: FlagChange[] = [];
  for (const uid of uids) {
    const m = await table.get([user, uid]);
    if (!m) continue;
    const now = new Set(op === 'set' ? [] : m.flags);
    for (const f of flags) {
      if (op === 'remove') now.delete(f);
      else now.add(f);
    }
    const next = SYSTEM_FLAGS.filter((f) => now.has(f));
    if (next.length === m.flags.length && next.every((f, i) => f === m.flags[i])) continue;
    const modseq = await nextModseq(tx, user);
    await table.put({ ...m, flags: next, modseq });
    out.push({ uid, flags: next, modseq });
  }
  if (out.length) {
    tx.emit(changedTopic(tx.db), 'changed', { user, kind: 'flags' } satisfies Changed, {
      key: user,
    });
  }
  return out;
}

/**
 * Delete a user's messages. A draft's parts are deleted once no copy of
 * the message refers to them (a message sent to several users shares
 * them). `mail/changed` announces it. Returns the uids deleted.
 */
export async function expunge(tx: DbTransaction, user: string, uids: number[]): Promise<number[]> {
  const table = tx.table<Message>('messages');
  const gone: Message[] = [];
  for (const uid of uids) {
    const m = await table.get([user, uid]);
    if (m) gone.push(m);
  }
  if (!gone.length) return [];
  const deleting = new Set(gone.map((m) => m.uid));
  for (const draft of new Set(gone.map((m) => m.draft))) {
    // Read before the deletes: the copies other than these keep the parts.
    const copies = await tx.table<Message>('messages').query().where('draft', '=', draft).all();
    if (copies.some((c) => c.user !== user || !deleting.has(c.uid))) continue;
    const parts = await tx.table<Part>('parts').query().where('draft', '=', draft).all();
    for (const p of parts) await tx.table<Part>('parts').delete([draft, p.seq]);
  }
  for (const m of gone) await table.delete([user, m.uid]);
  await nextModseq(tx, user);
  tx.emit(changedTopic(tx.db), 'changed', { user, kind: 'expunge' } satisfies Changed, {
    key: user,
  });
  return gone.map((m) => m.uid);
}

// ------------------------------------------------------------ reading a message

/**
 * The header section's length in the stored message, through its blank
 * line. 13a's rows don't record it: their first part is scanned.
 */
export async function headerLength(tx: DbTransaction, m: Message): Promise<number> {
  if (m.hdr !== undefined) return m.hdr;
  const head = new TextEncoder().encode(m.head).length;
  const body = m.size - head;
  const p = body > 0 ? await tx.table<Part>('parts').get([m.draft, 0]) : undefined;
  const h = scanHeaderEnd(2, p?.data ?? new Uint8Array(0));
  return head + (h.end >= 0 ? h.end : Math.min(body, PART_BYTES));
}

/**
 * `len` bytes of a stored message from `from` (its `Received:` header,
 * then the body), reading only the parts that cover them. Shorter at the
 * message's end.
 */
export async function bodyRange(
  tx: DbTransaction,
  m: Message,
  from: number,
  len: number,
): Promise<Uint8Array> {
  const head = new TextEncoder().encode(m.head);
  const end = Math.min(m.size, from + len);
  if (end <= from) return new Uint8Array(0);
  const out = new Uint8Array(end - from);
  let at = 0;
  if (from < head.length) {
    const piece = head.subarray(from, Math.min(head.length, end));
    out.set(piece);
    at = piece.length;
  }
  let off = Math.max(from, head.length) - head.length;
  const stop = end - head.length;
  while (off < stop) {
    const seq = Math.floor(off / PART_BYTES);
    const p = await tx.table<Part>('parts').get([m.draft, seq]);
    if (!p) throw new Error(`message ${m.uid} of ${m.user}: part ${seq} is missing`);
    const inPart = off - seq * PART_BYTES;
    const piece = p.data.subarray(inPart, Math.min(p.data.length, inPart + (stop - off)));
    if (!piece.length) throw new Error(`message ${m.uid} of ${m.user}: part ${seq} is short`);
    out.set(piece, at);
    at += piece.length;
    off += piece.length;
  }
  return out;
}

// ------------------------------------------------------------ app passwords

const B32 = 'abcdefghijklmnopqrstuvwxyz234567';

function base32(b: Uint8Array): string {
  let out = '';
  let acc = 0;
  let bits = 0;
  for (const x of b) {
    acc = (acc << 8) | x;
    bits += 8;
    while (bits >= 5) {
      out += B32[(acc >> (bits - 5)) & 31];
      bits -= 5;
    }
  }
  if (bits > 0) out += B32[(acc << (5 - bits)) & 31];
  return out;
}

const hex = (b: Uint8Array) => Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');

/** A password as typed: without its dashes and spaces, lowercase. */
export function normalizePassword(p: string): string {
  return p.replace(/[\s-]/g, '').toLowerCase();
}

/** SHA-256 of a normalized password, hex. */
export async function passwordHash(p: string): Promise<string> {
  const d = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(normalizePassword(p)));
  return hex(new Uint8Array(d));
}

/**
 * Make an app password for `user`: 20 random bytes as four groups of eight
 * base32 letters. Only its SHA-256 is stored; a slow hash would add
 * nothing to a random secret. The password is returned once.
 */
export async function createAppPassword(
  db: Db,
  user: string,
  label: string,
): Promise<{ id: string; password: string }> {
  const secret = base32(crypto.getRandomValues(new Uint8Array(20)));
  const password = (secret.match(/.{8}/g) ?? []).join('-');
  const id = hex(crypto.getRandomValues(new Uint8Array(8)));
  await db.table<AppPassword>('app_passwords').insert({
    user,
    id,
    label,
    hash: await passwordHash(password),
    created: Date.now(),
  });
  return { id, password };
}

/** A user's app passwords (their hashes, labels and ids). */
export function appPasswords(db: Db, user: string): Promise<AppPassword[]> {
  return db.table<AppPassword>('app_passwords').query().where('user', '=', user).all();
}

/** Revoke an app password: sessions logged in with it end at their next step. */
export function revokeAppPassword(db: Db, user: string, id: string): Promise<boolean> {
  return db.table<AppPassword>('app_passwords').delete([user, id]);
}

/** The user a login name stands for: a zen user name, or one of its addresses. */
export async function loginUser(tx: DbTransaction, login: string): Promise<string> {
  if (!login.includes('@')) return login;
  const box = await tx.table<Mailbox>('mailboxes').get(login.toLowerCase());
  return box?.user ?? '';
}

/** The id of `user`'s app password `password`, if it is one. */
export async function checkAppPassword(
  tx: DbTransaction,
  user: string,
  password: string,
): Promise<string | undefined> {
  if (!user) return undefined;
  const hash = await passwordHash(password);
  const all = await tx.table<AppPassword>('app_passwords').query().where('user', '=', user).all();
  return all.find((p) => p.hash === hash)?.id;
}

// ------------------------------------------------------------ waking IMAP

/**
 * Wake the handlers of a user's sessions (`c.wakeOn(user)`) when mail
 * arrives for them or their mailbox changes elsewhere. Returns a stop.
 */
export async function wakeOnMail(db: Db, h: ServeHandle): Promise<() => Promise<void>> {
  const wake = (m: { body: unknown }) => {
    const user = (m.body as { user?: unknown }).user;
    if (typeof user === 'string') h.wake(user);
  };
  const subs: OnSubscription[] = [
    await db.on(receivedTopic(db), wake),
    await db.on(changedTopic(db), wake),
  ];
  return async () => {
    for (const s of subs) await s.close();
  };
}
