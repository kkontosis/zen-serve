// The mail app's database: its namespace, tables and topic. Kept apart from
// smtp.ts so that code without @zen/svc can use it.
import type { Topic } from '@zen/client';
import type { Db, DbOptions } from '@zen/db';

/** The database namespace. */
export const NS = 'mail';

/** A local address and the user whose mail it is. */
export type Mailbox = {
  /** Lowercase, `local@domain`. */
  address: string;
  user: string;
};

/** A message being received: DATA staged as `parts` until its end. */
export type Draft = {
  /** Its id, hex; the parts and the stored messages refer to it. */
  draft: string;
  /** The connection receiving it, hex. */
  conn: string;
  /** Unix milliseconds. */
  started: number;
};

/** A piece of a message's body (at most `PART_BYTES`), in order of `seq`. */
export type Part = {
  draft: string;
  seq: number;
  data: Uint8Array;
};

/** A stored message: one per recipient, sharing the parts of its draft. */
export type Message = {
  user: string;
  /** Per user, from 1, in the order received. */
  uid: number;
  draft: string;
  /** Bytes: `head` (UTF-8) and the body. */
  size: number;
  /** The reverse-path (`MAIL FROM`), empty for `<>`. */
  from: string;
  /** The recipient address this copy was for. */
  to: string;
  /** Unix milliseconds. */
  received: number;
  /** The trace header put before the body (`Received: …`). */
  head: string;
  /** IMAP system flags (`\Seen`, `\Flagged`, …). */
  flags: string[];
  /** The user's change counter when the row last changed (schema 2; missing: 0). */
  modseq?: number;
  /** The header section's length in the stored message, `head` included, through the blank line (schema 2). */
  hdr?: number;
  /** Header fields, unfolded and as sent (schema 2). */
  fields?: HeaderFields;
};

/** A message's header fields, unfolded, as sent (no RFC 2047 decoding). */
export type HeaderFields = {
  from: string;
  to: string;
  cc: string;
  subject: string;
  date: string;
  messageId: string;
};

/**
 * A user's counters: the next uid, the change counter (`modseq`, schema 2)
 * and the mailbox's UIDVALIDITY (schema 2; missing: 1). Uids are never
 * reused.
 */
export type Counter = { user: string; next: number; modseq?: number; validity?: number };

/** An app password: what IMAP clients log in with. Only its hash is kept. */
export type AppPassword = {
  user: string;
  /** Hex. */
  id: string;
  label: string;
  /** SHA-256 of the password, normalized (`normalizePassword`), hex. */
  hash: string;
  /** Unix milliseconds. */
  created: number;
};

/** A certificate, as stored (PEM), or the inbox seed (`key` its hex). */
export type Cert = {
  name: string;
  /** The chain, leaf first. */
  chain: string;
  key: string;
};

/** What `mail/received` carries for each message stored. */
export type Received = { user: string; uid: number; size: number };

/** What `mail/changed` carries: a user's flags changed, or messages were expunged. */
export type Changed = { user: string; kind: 'flags' | 'expunge' };

/** The most body bytes a part holds. */
export const PART_BYTES = 65_536;

/** The `certs` row of the TLS certificate. */
export const TLS_CERT = 'smtp';

/** The `certs` row that holds the sealed inbox's seed: `key` is its hex. */
export const INBOX_SEED = 'inbox';

export const DB_OPTIONS: DbOptions = {
  schema: 2,
  migrations: {
    1: async (m) => {
      await m.createTable('mailboxes', { pk: ['address'] });
      await m.createTable('drafts', { pk: ['draft'] });
      await m.createTable('parts', { pk: ['draft', 'seq'] });
      await m.createIndex('parts', 'by_draft_seq', {
        fields: [
          ['draft', 'text'],
          ['seq', 'int'],
        ],
      });
      await m.createTable('messages', { pk: ['user', 'uid'] });
      await m.createIndex('messages', 'by_user_uid', {
        fields: [
          ['user', 'text'],
          ['uid', 'int'],
        ],
      });
      await m.createTable('counters', { pk: ['user'] });
      await m.createTable('certs', { pk: ['name'] });
    },
    2: async (m) => {
      // Whether an expunged message's parts are still shared.
      await m.createIndex('messages', 'by_draft', { fields: [['draft', 'text']] });
      // A user's addresses, for the page.
      await m.createIndex('mailboxes', 'by_user', { fields: [['user', 'text']] });
      await m.createTable('app_passwords', { pk: ['user', 'id'] });
      await m.createIndex('app_passwords', 'by_user', { fields: [['user', 'text']] });
    },
  },
};

/** Where each stored message is announced (`message` events, keyed by user). */
export const receivedTopic = (db: Db): Topic => db.topic('mail', 'received');

/** Where flag changes and expunges are announced (`changed` events, keyed by user). */
export const changedTopic = (db: Db): Topic => db.topic('mail', 'changed');

/** The UIDVALIDITY of a counters row (13a's rows have none: 1). */
export const validityOf = (c: Counter | undefined): number => c?.validity ?? 1;

/** A new counters row's UIDVALIDITY: the time in seconds, as RFC 3501 §2.3.1.1 suggests. */
export const newValidity = (): number => Math.max(1, Math.floor(Date.now() / 1000) % 2 ** 32);

/** Deliver mail for `address` to `user`. */
export async function putMailbox(db: Db, address: string, user: string): Promise<void> {
  await db.table<Mailbox>('mailboxes').put({ address: address.toLowerCase(), user });
}

/** Store the TLS certificate (a row, sealed like every row). */
export async function putCert(db: Db, chain: string, key: string): Promise<void> {
  await db.table<Cert>('certs').put({ name: TLS_CERT, chain, key });
}

/** Store the sealed inbox's 32-byte decapsulation seed (svc.md §9). */
export async function putInboxSeed(db: Db, seed: Uint8Array): Promise<void> {
  const hex = Array.from(seed, (b) => b.toString(16).padStart(2, '0')).join('');
  await db.table<Cert>('certs').put({ name: INBOX_SEED, chain: '', key: hex });
}

/** The sealed inbox's seed, if stored. */
export async function inboxSeed(db: Db): Promise<Uint8Array | undefined> {
  const c = await db.table<Cert>('certs').get(INBOX_SEED);
  if (!c) return undefined;
  return Uint8Array.from(c.key.match(/../g) ?? [], (h) => Number.parseInt(h, 16));
}
