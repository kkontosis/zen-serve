// The credits ledger's logic (docs/EXAMPLES.md §5), over a `Db` and no DOM:
// accounts whose balances never go negative, an entry per movement, an
// audit event per transfer in the same commit (the outbox), a statement
// projection fed by the entries' change events (exactly once per entry),
// and a per-device audit view kept by a durable `on`.
import { isCode, type Topic } from '@zen/client';
import type {
  ChangeBody,
  ConsumerHandle,
  Cursor,
  Db,
  DbOptions,
  Message,
  OnSubscription,
} from '@zen/db';

/** The database namespace. */
export const NS = 'ledger';

/** An account. `at` is the time of its newest entry (they are strictly increasing). */
export type Account = { id: string; owner: string; balance: number; at: number };

/** One movement on one account: negative out, positive in. */
export type Entry = {
  id: string;
  account: string;
  amount: number;
  /** Unix ms; strictly increasing per account, in commit order. */
  at: number;
  memo: string;
  /** The transfer it belongs to ('' for an opening deposit). */
  transfer: string;
};

/** A month's totals of an account, kept by the `statements` projection. */
export type Statement = {
  account: string;
  /** `YYYY-MM` (UTC). */
  month: string;
  total_in: number;
  total_out: number;
  /** Entries counted: each exactly once. */
  count: number;
};

/** The body of an audit event. */
export type AuditBody = { transfer: string; from: string; to: string; amount: number; at: number };

/** A row of a device's audit view (its own read model, per device). */
export type AuditRow = AuditBody & { device: Uint8Array; id: string };

/** Schema and migrations, for `Db.open` (or `boot`). */
export const DB_OPTIONS: DbOptions = {
  schema: 1,
  migrations: {
    1: async (m) => {
      await m.createTable('accounts', { pk: ['id'] });
      await m.createIndex('accounts', 'by_owner', { fields: [['owner', 'text']] });
      await m.createTable('entries', {
        pk: ['id'],
        changes: { topic: ['ledger', 'entries'], image: 'full' },
      });
      await m.createIndex('entries', 'by_account_at', {
        kind: 'private',
        fields: [
          ['account', 'text'],
          ['at', 'int', 'desc'],
        ],
      });
      await m.createTable('statements', {
        pk: ['account', 'month'],
        changes: { topic: ['ledger', 'statements'], image: 'full' },
      });
      await m.createTable('audit_view', { pk: ['device', 'id'] });
      await m.createIndex('audit_view', 'by_device_at', {
        fields: [
          ['device', 'bytes'],
          ['at', 'int', 'desc'],
        ],
      });
    },
  },
};

/** The change topic of `entries`. */
export const entriesTopic = (db: Db): Topic => db.topic('ledger', 'entries');
/** The change topic of `statements` (for UIs that show them). */
export const statementsTopic = (db: Db): Topic => db.topic('ledger', 'statements');
/** `("ledger", "audit")`. */
export const auditTopic = (db: Db): Topic => db.topic('ledger', 'audit');

/** The month (`YYYY-MM`, UTC) of a time. */
export const monthOf = (at: number): string => new Date(at).toISOString().slice(0, 7);

export type LedgerErrorCode = 'overdraft' | 'no_account' | 'bad_amount' | 'same_account';

/** A refused operation. An overdraft carries the source as the refusal saw it. */
export class LedgerError extends Error {
  constructor(
    readonly code: LedgerErrorCode,
    message: string,
    /** For `overdraft`: the source's balance and newest entry time at the refusal. */
    readonly seen?: { balance: number; at: number },
  ) {
    super(message);
    this.name = 'LedgerError';
  }
}

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
const isAmount = (n: number) => Number.isSafeInteger(n) && n > 0;

/**
 * Run a short transaction, retrying after a random pause when its attempts
 * ran out on conflicts (many writers on one account). A conflicting commit
 * wrote nothing, so running it again is safe.
 */
async function withRetry<R>(db: Db, fn: Parameters<Db['transaction']>[0]): Promise<R> {
  for (let round = 1; ; round++) {
    try {
      return (await db.transaction(fn)) as R;
    } catch (e) {
      if (!isCode(e, 'conflict') || round >= 20) throw e;
      await sleep(Math.random() * 20 * Math.min(round, 10));
    }
  }
}

/** Open an account; its initial deposit is an entry, so statements and entries agree. */
export async function openAccount(
  db: Db,
  o: { id?: string; owner: string; deposit: number },
): Promise<Account> {
  if (!(isAmount(o.deposit) || o.deposit === 0)) {
    throw new LedgerError('bad_amount', 'the deposit must be a whole number ≥ 0');
  }
  const id = o.id ?? crypto.randomUUID();
  const at = Date.now();
  const acct: Account = { id, owner: o.owner, balance: o.deposit, at };
  await db.transaction(async (tx) => {
    await tx.table<Account>('accounts').insert(acct);
    if (o.deposit > 0) {
      await tx.table<Entry>('entries').insert({
        id: crypto.randomUUID(),
        account: id,
        amount: o.deposit,
        at,
        memo: 'opening deposit',
        transfer: '',
      });
    }
  });
  return acct;
}

/** All accounts, by owner. */
export async function listAccounts(db: Db): Promise<Account[]> {
  return db.table<Account>('accounts').query().orderBy('owner').all();
}

export interface TransferResult {
  transfer: string;
  from: Account;
  to: Account;
}

type Outcome =
  | { ok: true; from: Account; to: Account }
  | { ok: false; code: LedgerErrorCode; message: string; seen?: { balance: number; at: number } };

/**
 * Move `amount` from one account to another, in one short transaction:
 * read both accounts, refuse an overdraft, update both balances, insert two
 * entries and emit the audit event. Balances, entries and the audit log
 * never disagree. Two transfers from one account at once: the second
 * commit conflicts, re-runs on the new balance, and is refused if it
 * would overdraw.
 *
 * A refusal is the transaction's result rather than a throw, so it is a
 * read-only transaction whose reads are checked before it returns: the
 * balance it refused on was the account's balance at that point.
 */
export async function transfer(
  db: Db,
  o: { from: string; to: string; amount: number; memo?: string },
): Promise<TransferResult> {
  if (!isAmount(o.amount))
    throw new LedgerError('bad_amount', 'the amount must be a whole number > 0');
  if (o.from === o.to) throw new LedgerError('same_account', 'transfer to the same account');
  const id = crypto.randomUUID();
  const memo = o.memo ?? '';
  const r = await withRetry<Outcome>(db, async (tx) => {
    const accounts = tx.table<Account>('accounts');
    const a = await accounts.get(o.from);
    const b = await accounts.get(o.to);
    if (!a || !b) {
      return { ok: false, code: 'no_account', message: `no account ${a ? o.to : o.from}` };
    }
    if (a.balance < o.amount) {
      return {
        ok: false,
        code: 'overdraft',
        message: `overdraft: ${a.id} has ${a.balance}, the transfer is ${o.amount}`,
        seen: { balance: a.balance, at: a.at },
      };
    }
    const now = Date.now();
    const from: Account = { ...a, balance: a.balance - o.amount, at: Math.max(now, a.at + 1) };
    const to: Account = { ...b, balance: b.balance + o.amount, at: Math.max(now, b.at + 1) };
    await accounts.put(from);
    await accounts.put(to);
    const entries = tx.table<Entry>('entries');
    await entries.insert({
      id: `${id}:out`,
      account: from.id,
      amount: -o.amount,
      at: from.at,
      memo,
      transfer: id,
    });
    await entries.insert({
      id: `${id}:in`,
      account: to.id,
      amount: o.amount,
      at: to.at,
      memo,
      transfer: id,
    });
    const audit: AuditBody = { transfer: id, from: from.id, to: to.id, amount: o.amount, at: now };
    tx.emit(auditTopic(db), 'transfer', audit, { key: id });
    return { ok: true, from, to };
  });
  if (!r.ok) throw new LedgerError(r.code, r.message, r.seen);
  return { transfer: id, from: r.from, to: r.to };
}

/** One page of an account's entries, newest first, and the cursor of the next (older) page. */
export async function historyPage(
  db: Db,
  account: string,
  limit: number,
  after?: Cursor,
): Promise<{ rows: Entry[]; cursor?: Cursor }> {
  const q = db
    .table<Entry>('entries')
    .query()
    .where('account', '=', account)
    .orderBy('at', 'desc')
    .limit(limit);
  return (after ? q.after(after) : q).page();
}

/**
 * Pages through an account's history, newest first: `older()` follows the
 * page's cursor, `newer()` goes back to the cursor the previous page
 * started from.
 */
export class HistoryPager {
  /** The cursor each shown page started after (undefined: the newest page). */
  private starts: (Cursor | undefined)[] = [undefined];
  private next: Cursor | undefined;
  rows: Entry[] = [];

  constructor(
    private readonly db: Db,
    readonly account: string,
    readonly limit = 10,
  ) {}

  /** The page number, from 0 (the newest). */
  get index(): number {
    return this.starts.length - 1;
  }
  get hasOlder(): boolean {
    return this.next !== undefined;
  }
  get hasNewer(): boolean {
    return this.index > 0;
  }

  /** Load the current page again (after a change). */
  async reload(): Promise<Entry[]> {
    const p = await historyPage(this.db, this.account, this.limit, this.starts[this.index]);
    this.rows = p.rows;
    this.next = p.cursor;
    return p.rows;
  }

  async older(): Promise<Entry[]> {
    if (!this.next) return this.rows;
    this.starts.push(this.next);
    return this.reload();
  }

  async newer(): Promise<Entry[]> {
    if (this.index === 0) return this.rows;
    this.starts.pop();
    return this.reload();
  }
}

/** An account's statement for a month (zeros before any entry). */
export async function statement(db: Db, account: string, month: string): Promise<Statement> {
  return (
    (await db.table<Statement>('statements').get([account, month])) ?? {
      account,
      month,
      total_in: 0,
      total_out: 0,
      count: 0,
    }
  );
}

/**
 * The statement projection: a `sequential` consumer group `statements` on
 * the entries' change topic. Each change event updates its month's row in
 * the transaction that consumes it, so each entry counts exactly once,
 * whichever instance runs the group and however often it restarts.
 */
export function runStatements(db: Db, onError?: (e: unknown) => void): ConsumerHandle {
  return db.consume<ChangeBody<Entry>>(
    {
      group: 'statements',
      topic: entriesTopic(db),
      mode: 'sequential',
      ...(onError ? { onError } : {}),
    },
    async (msg, tx) => {
      if (msg.type !== 'zen.change' || msg.body.op !== 'put') return;
      // Entries are never updated or deleted: a put is an insert.
      const e = msg.body.row ?? (await tx.table<Entry>('entries').get(msg.body.pk));
      if (!e) return;
      const st = tx.table<Statement>('statements');
      const month = monthOf(e.at);
      const cur = await st.get([e.account, month]);
      await st.put({
        account: e.account,
        month,
        total_in: (cur?.total_in ?? 0) + Math.max(e.amount, 0),
        total_out: (cur?.total_out ?? 0) + Math.max(-e.amount, 0),
        count: (cur?.count ?? 0) + 1,
      });
    },
  );
}

/**
 * The audit view of this device: a durable `on(("ledger", "audit"))` with
 * the cursor `audit-view`, from the start of the log the first time, that
 * inserts a row per transfer in the transaction that moves the cursor
 * (exactly once per device). `onRow` runs after each commit.
 */
export function runAuditView(
  db: Db,
  o: { onRow?: (r: AuditRow) => void; onError?: (e: unknown) => void } = {},
): Promise<OnSubscription> {
  return db.on<AuditBody>(
    auditTopic(db),
    async (msg: Message<AuditBody>, tx) => {
      if (msg.type !== 'transfer') return;
      const row: AuditRow = { ...msg.body, device: db.device, id: msg.body.transfer };
      await tx.table<AuditRow>('audit_view').insert(row);
      if (o.onRow) tx.after(() => o.onRow?.(row));
    },
    {
      cursor: 'audit-view',
      start: 'earliest',
      ...(o.onError ? { onError: o.onError } : {}),
    },
  );
}

/** This device's audit view, newest first. */
export async function listAudit(db: Db, limit?: number): Promise<AuditRow[]> {
  const q = db
    .table<AuditRow>('audit_view')
    .query()
    .where('device', '=', db.device)
    .orderBy('at', 'desc');
  if (limit === undefined) return q.all();
  return (await q.limit(limit).page()).rows;
}
