// The ledger's invariants in Node (EXAMPLES.md §5, milestone 5 demo): many
// concurrent random transfers from several sessions, then balances, entries,
// statements and the audit view must agree; a statement projection stopped
// mid-stream and taken over by another instance counts each entry once.
import type { ConsumerHandle, Db } from '@zen/db';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { startWorld, until, type World } from '../../shared/harness.js';
import { signInWorker, type WorkerContext } from '../../shared/node.js';
import {
  type Account,
  type AuditRow,
  DB_OPTIONS,
  type Entry,
  HistoryPager,
  LedgerError,
  listAudit,
  monthOf,
  NS,
  openAccount,
  runAuditView,
  runStatements,
  type Statement,
  transfer,
} from '../src/ledger.js';
import { GRANTS } from './grants.js';

let w: World;
const workers: WorkerContext[] = [];
/** Instances that run transfers: separate sessions of two members. */
let traders: Db[] = [];
/** The instance whose durable `on` keeps the audit view. */
let auditor: Db;
let projection: { worker: WorkerContext; handle: ConsumerHandle } | undefined;
const errors: unknown[] = [];

const ACCOUNTS = ['acct-0', 'acct-1', 'acct-2', 'acct-3'];
const DEPOSIT = 100;
const quiet = { ...DB_OPTIONS, warn: () => {} };

async function session(user: string): Promise<WorkerContext> {
  const c = await signInWorker({
    url: w.url,
    device: w.user(user).deviceFile,
    passphrase: w.passphrase,
  });
  workers.push(c);
  return c;
}

/** Start the statement projection on a new session. */
async function startProjection(user: string) {
  const worker = await session(user);
  const db = await worker.open(NS, quiet);
  return { worker, handle: runStatements(db, (e) => errors.push(e)) };
}

/** Stop the projection mid-stream: close its database (and its session's fs). */
async function stopProjection() {
  const p = projection;
  projection = undefined;
  await p?.handle.stop();
  await p?.worker.close();
}

beforeAll(async () => {
  w = await startWorld({ users: ['alice', 'bob'], grants: GRANTS });
  for (const user of ['alice', 'bob', 'alice', 'bob']) {
    traders.push(await (await session(user)).open(NS, quiet));
  }
  auditor = await (await session('bob')).open(NS, quiet);
  await runAuditView(auditor, { onError: (e) => errors.push(e) });
  projection = await startProjection('alice');
  for (const id of ACCOUNTS) await openAccount(traders[0]!, { id, owner: id, deposit: DEPOSIT });
}, 60_000);

afterAll(async () => {
  await stopProjection();
  for (const c of workers) await c.close().catch(() => {});
  traders = [];
  await w?.stop();
});

interface Done {
  id: string;
  from: string;
  to: string;
  amount: number;
  /** What the transfer wrote, for a failed check (`disagreement`): the trader that ran it, and each side's balance and time after it. */
  trader?: number;
  after?: { from: number; fromAt: number; to: number; toAt: number };
}
interface Refused {
  from: string;
  amount: number;
  seen: { balance: number; at: number };
}
const done: Done[] = [];
const refused: Refused[] = [];

const pick = <T>(xs: T[]): T => xs[Math.floor(Math.random() * xs.length)]!;

/** `n` random transfers, one after another, on one instance. */
async function trade(db: Db, n: number, maxAmount: number): Promise<void> {
  const trader = traders.indexOf(db);
  for (let i = 0; i < n; i++) {
    const from = pick(ACCOUNTS);
    const to = pick(ACCOUNTS.filter((a) => a !== from));
    const amount = 1 + Math.floor(Math.random() * maxAmount);
    try {
      const r = await transfer(db, { from, to, amount, memo: `${from}→${to}` });
      done.push({
        id: r.transfer,
        from,
        to,
        amount,
        trader,
        after: { from: r.from.balance, fromAt: r.from.at, to: r.to.balance, toAt: r.to.at },
      });
    } catch (e) {
      if (!(e instanceof LedgerError) || e.code !== 'overdraft' || !e.seen) throw e;
      refused.push({ from, amount, seen: e.seen });
    }
  }
}

/** Many concurrent random transfers: `loops` concurrent loops on each trader. */
const tradeAll = (perLoop: number, loops = 3, maxAmount = 60) =>
  Promise.all(
    traders.flatMap((db) => Array.from({ length: loops }, () => trade(db, perLoop, maxAmount))),
  );

const reader = () => traders[1]!;

async function accounts(): Promise<Account[]> {
  const t = reader().table<Account>('accounts');
  return Promise.all(ACCOUNTS.map(async (id) => (await t.get(id))!));
}

/** An account's whole history through the paged index (pages of 7), oldest first. */
async function history(id: string): Promise<Entry[]> {
  const p = new HistoryPager(reader(), id, 7);
  const all = [...(await p.reload())];
  while (p.hasOlder) all.push(...(await p.older()));
  return all.reverse();
}

/** Every entry, by account, oldest first. */
async function allEntries(): Promise<Map<string, Entry[]>> {
  const m = new Map<string, Entry[]>();
  for (const id of ACCOUNTS) m.set(id, await history(id));
  return m;
}

/** The statements the entries imply, by `account/month`. */
function expectedStatements(entries: Map<string, Entry[]>): Map<string, Statement> {
  const out = new Map<string, Statement>();
  for (const [account, es] of entries) {
    for (const e of es) {
      const month = monthOf(e.at);
      const k = `${account}/${month}`;
      const s = out.get(k) ?? { account, month, total_in: 0, total_out: 0, count: 0 };
      if (e.amount > 0) s.total_in += e.amount;
      else s.total_out -= e.amount;
      s.count++;
      out.set(k, s);
    }
  }
  return out;
}

const statementCount = async (keys: Iterable<Statement>) => {
  let n = 0;
  const t = reader().table<Statement>('statements');
  for (const s of keys) n += (await t.get([s.account, s.month]))?.count ?? 0;
  return n;
};

/** Wait until the projection counted every entry, then compare every statement. */
async function checkStatements(entries: Map<string, Entry[]>): Promise<void> {
  const want = expectedStatements(entries);
  const total = [...entries.values()].reduce((n, es) => n + es.length, 0);
  await until(
    async () => (await statementCount(want.values())) >= total,
    60_000,
    'the projection to catch up',
  );
  const t = reader().table<Statement>('statements');
  for (const s of want.values()) {
    expect(await t.get([s.account, s.month])).toEqual(s);
  }
}

/** Wait until the audit view has a row per transfer, then compare. */
async function checkAudit(): Promise<void> {
  await until(
    async () => (await listAudit(auditor)).length >= done.length,
    60_000,
    'the audit view to catch up',
  );
  const rows: AuditRow[] = await listAudit(auditor);
  expect(rows.length).toBe(done.length);
  const byId = new Map(rows.map((r) => [r.id, r]));
  expect(byId.size).toBe(rows.length);
  for (const d of done) {
    expect(byId.get(d.id)).toMatchObject({ from: d.from, to: d.to, amount: d.amount });
  }
}

/**
 * What disagrees, for a failed sum: each account's balance against the sum
 * of its entries, the transfers whose entries don't balance, and the
 * committed transfers with no entries. Balances off with entries summing
 * right would be a lost update on an account row (entries are only
 * inserted); entries off too would be a transfer's entries split.
 */
function disagreement(accts: Account[], entries: Map<string, Entry[]>): string {
  const lines = accts.map((a) => {
    const es = entries.get(a.id) ?? [];
    const sum = es.reduce((n, e) => n + e.amount, 0);
    return `${a.id}: balance ${a.balance} at ${a.at}; entries ${es.length}, sum ${sum}, newest ${es.at(-1)?.at}`;
  });
  const byTransfer = new Map<string, Entry[]>();
  for (const es of entries.values()) {
    for (const e of es) {
      if (e.transfer) byTransfer.set(e.transfer, [...(byTransfer.get(e.transfer) ?? []), e]);
    }
  }
  for (const [t, es] of byTransfer) {
    if (es.length !== 2 || es[0]!.amount + es[1]!.amount !== 0) {
      lines.push(`transfer ${t}: ${JSON.stringify(es.map((e) => [e.account, e.amount, e.at]))}`);
    }
  }
  for (const d of done) {
    if (ACCOUNTS.includes(d.from) && !byTransfer.has(d.id)) {
      lines.push(`no entries for done transfer ${JSON.stringify(d)}`);
    }
  }
  // Each account's committed transfers in its time order: each must have
  // written the balance before it plus its amount. One that didn't computed
  // from a stale balance (a lost update): it and the transfer before it.
  for (const a of accts) {
    const moves = done
      .filter((d) => d.after && (d.from === a.id || d.to === a.id))
      .map((d) =>
        d.from === a.id
          ? { d, at: d.after!.fromAt, wrote: d.after!.from, delta: -d.amount }
          : { d, at: d.after!.toAt, wrote: d.after!.to, delta: d.amount },
      )
      .sort((x, y) => x.at - y.at);
    let bal = DEPOSIT;
    let prev: (typeof moves)[number] | undefined;
    for (const m of moves) {
      if (m.wrote !== bal + m.delta) {
        const show = (x: typeof m) =>
          `transfer ${x.d.id} (trader ${x.d.trader}, ${x.d.from}→${x.d.to} ${x.d.amount}) at ${x.at} wrote ${x.wrote}`;
        lines.push(
          `${a.id}: ${show(m)}, but after ${bal} it should be ${bal + m.delta}; before it: ${prev ? show(prev) : 'the opening deposit'}`,
        );
      }
      bal = m.wrote;
      prev = m;
    }
  }
  return lines.join('\n');
}

/** The invariants of the balances and the entries, after transfers settled. */
async function checkLedger(): Promise<Map<string, Entry[]>> {
  const accts = await accounts();
  const entries = await allEntries();
  // The sum of balances is constant; no balance is negative.
  expect(
    accts.reduce((s, a) => s + a.balance, 0),
    `the balances' sum\n${disagreement(accts, entries)}`,
  ).toBe(DEPOSIT * ACCOUNTS.length);
  for (const a of accts) expect(a.balance).toBeGreaterThanOrEqual(0);
  // Each account's entries, in commit order (`at` strictly increasing),
  // never take it below zero and add up to its balance.
  for (const a of accts) {
    const es = entries.get(a.id)!;
    let bal = 0;
    for (const [i, e] of es.entries()) {
      if (i > 0) expect(e.at).toBeGreaterThan(es[i - 1]!.at);
      bal += e.amount;
      expect(bal).toBeGreaterThanOrEqual(0);
    }
    expect(bal).toBe(a.balance);
    expect(es.at(-1)!.at).toBe(a.at);
  }
  // Two entries per successful transfer and none for anything else.
  const byTransfer = new Map<string, Entry[]>();
  for (const es of entries.values()) {
    for (const e of es) {
      if (!e.transfer) continue;
      byTransfer.set(e.transfer, [...(byTransfer.get(e.transfer) ?? []), e]);
    }
  }
  const ours = done.filter((d) => ACCOUNTS.includes(d.from));
  expect(byTransfer.size).toBe(ours.length);
  for (const d of ours) {
    const es = byTransfer.get(d.id)!;
    expect(es.map((e) => [e.account, e.amount]).sort()).toEqual(
      [
        [d.from, -d.amount],
        [d.to, d.amount],
      ].sort(),
    );
  }
  // Every refusal was a real overdraft: the source's balance it saw was the
  // balance after the source's entries up to that time, and too small.
  for (const r of refused) {
    expect(r.seen.balance).toBeLessThan(r.amount);
    const upTo = entries.get(r.from)!.filter((e) => e.at <= r.seen.at);
    expect(upTo.reduce((s, e) => s + e.amount, 0)).toBe(r.seen.balance);
    expect(upTo.at(-1)!.at).toBe(r.seen.at);
  }
  return entries;
}

describe('the credits ledger', () => {
  it('many concurrent random transfers keep balances, entries, statements and the audit log in agreement', async () => {
    await tradeAll(10);
    expect(done.length + refused.length).toBe(traders.length * 3 * 10);
    // Both outcomes happened, so the checks below test something.
    expect(done.length).toBeGreaterThan(10);
    expect(refused.length).toBeGreaterThan(0);
    const entries = await checkLedger();
    await checkStatements(entries);
    await checkAudit();
    expect(errors).toEqual([]);
  }, 180_000);

  it('two transfers from one account at once: one commits, the other is refused on the new balance', async () => {
    const [a, b] = traders as [Db, Db];
    await openAccount(a, { id: 'pair-src', owner: 'pair', deposit: 50 });
    await openAccount(a, { id: 'pair-dst', owner: 'pair', deposit: 0 });
    const rs = await Promise.allSettled([
      transfer(a, { from: 'pair-src', to: 'pair-dst', amount: 40 }),
      transfer(b, { from: 'pair-src', to: 'pair-dst', amount: 40 }),
    ]);
    const ok = rs.filter((r) => r.status === 'fulfilled');
    const no = rs.filter((r): r is PromiseRejectedResult => r.status === 'rejected');
    expect(ok).toHaveLength(1);
    expect(no).toHaveLength(1);
    const e = no[0]!.reason;
    expect(e).toBeInstanceOf(LedgerError);
    expect(e.code).toBe('overdraft');
    expect(e.seen.balance).toBe(10);
    expect((await reader().table<Account>('accounts').get('pair-src'))!.balance).toBe(10);
    expect((await reader().table<Account>('accounts').get('pair-dst'))!.balance).toBe(40);
    done.push({
      id: (ok[0] as PromiseFulfilledResult<{ transfer: string }>).value.transfer,
      from: 'pair-src',
      to: 'pair-dst',
      amount: 40,
    });
    await checkAudit();
  }, 60_000);

  it('the projection stopped mid-stream and restarted elsewhere counts each entry exactly once', async () => {
    // A backlog of change events while no projection runs.
    await stopProjection();
    const before = await statementCount(expectedStatements(await allEntries()).values());
    await tradeAll(10, 2);
    const entries = await checkLedger();
    const want = expectedStatements(entries);
    const total = [...entries.values()].reduce((n, es) => n + es.length, 0);
    expect(total - before).toBeGreaterThan(20);
    // A first instance takes the backlog, and is closed partway.
    projection = await startProjection('bob');
    await until(
      async () => (await statementCount(want.values())) > before,
      60_000,
      'the first instance to start',
    );
    await stopProjection();
    const mid = await statementCount(want.values());
    expect(mid).toBeGreaterThan(before);
    expect(mid).toBeLessThan(total);
    // Another instance, on another session, finishes it; more transfers meanwhile.
    projection = await startProjection('alice');
    await tradeAll(3, 1);
    const after = await checkLedger();
    await checkStatements(after);
    await checkAudit();
    expect(errors).toEqual([]);
  }, 180_000);
});
