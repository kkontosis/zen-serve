// The checkout saga in Node (EXAMPLES.md §4, the milestone 5 demo): the
// services run as Node workers (one member, one device each), killed with
// SIGKILL at will; the test places orders as the customer.
import type { Db } from '@zen/db';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { sleep, startWorld, until } from '../../shared/harness.js';
import { signInWorker, type WorkerContext } from '../../shared/node.js';
import {
  CATALOG,
  checkoutSaga,
  DB_OPTIONS,
  type Item,
  NS,
  type Order,
  type Payment,
  placeOrder,
  type Reservation,
  restock,
  type Shipment,
  type Stock,
} from '../src/checkout.js';
import { GRANTS } from './grants.js';
import { providerCharges, SCHEDULER, Services, startProvider, USERS } from './services.js';

beforeAll(() => {});

/** A world, the customer's database and the services. */
async function shop(opts: { scheduler: boolean }) {
  const w = await startWorld({
    users: USERS,
    grants: GRANTS,
    server: { limits: { claim_ttl_ms: 1000 } },
  });
  const alice: WorkerContext = await signInWorker({
    url: w.url,
    device: w.user('alice').deviceFile,
    passphrase: w.passphrase,
  });
  const db = await alice.open(NS, {
    ...DB_OPTIONS,
    scheduler: opts.scheduler,
    schedulerOptions: SCHEDULER,
    warn: () => {},
  });
  const provider = await startProvider();
  const services = new Services(w, { PROVIDER_URL: provider.url });
  const saga = checkoutSaga(db, { orchestrate: false });
  return {
    w,
    db,
    saga,
    provider,
    services,
    async stop() {
      await services.stop();
      await provider.p.kill();
      await alice.close();
      await w.stop();
    },
  };
}

type Shop = Awaited<ReturnType<typeof shop>>;

const order = (db: Db, id: string) => db.table<Order>('orders').get(id);
const all = <T extends Record<string, unknown>>(db: Db, t: string) =>
  db.table(t).query().all() as Promise<T[]>;
const available = async (db: Db) =>
  Object.fromEntries((await all<Stock>(db, 'stock')).map((s) => [s.sku, s.available]));

async function waitStatus(s: Shop, id: string, want: string[], ms = 30_000): Promise<Order> {
  try {
    await until(
      async () => want.includes((await order(s.db, id))?.status ?? ''),
      ms,
      want.join('/'),
    );
  } catch (e) {
    const o = await order(s.db, id);
    const g = o && (await s.saga.get(o.saga));
    throw new Error(
      `${(e as Error).message}: order ${o?.status}, saga ${JSON.stringify(g?.status)} step ${g?.step}\n${s.services.dump()}`,
    );
  }
  return (await order(s.db, id))!;
}

describe('one order at a time', () => {
  let s: Shop;
  const initial = { apple: 10, bread: 10, cheese: 10, dates: 10 };

  beforeAll(async () => {
    s = await shop({ scheduler: true });
    await restock(s.db, initial);
    await s.services.startAll();
  });

  afterAll(async () => {
    await s?.stop();
  });

  it('an order goes through: reserved, charged once, shipped, done', async () => {
    const o = await placeOrder(s.db, s.saga, 'carol', [
      { sku: 'apple', n: 2 },
      { sku: 'bread', n: 1 },
    ]);
    expect(o.status).toBe('placed');
    expect(o.total).toBe(2 * CATALOG.apple!.price + CATALOG.bread!.price);
    await waitStatus(s, o.id, ['done']);
    const state = (await s.saga.get(o.saga))!;
    expect(state.status).toBe('done');
    expect(state.done).toEqual([0, 1, 2]);
    expect(await available(s.db)).toEqual({ ...initial, apple: 8, bread: 9 });
    const res = await all<Reservation>(s.db, 'reservations');
    expect(res.map((r) => [r.sku, r.n]).sort()).toEqual([
      ['apple', 2],
      ['bread', 1],
    ]);
    const pays = await all<Payment>(s.db, 'payments');
    expect(pays.map((p) => [p.order, p.amount, p.status])).toEqual([[o.id, o.total, 'paid']]);
    expect(await s.db.table<Shipment>('shipments').get(o.id)).toMatchObject({ status: 'shipped' });
    const c = (await providerCharges(s.provider.url)).charges.filter((x) => x.order === o.id);
    // One charge. (A transaction that re-runs on a conflict calls again, with the same key.)
    expect(c).toMatchObject([{ amount: o.total, status: 'charged', refunded: false }]);
    expect(c[0]!.key).toBe(pays[0]!.id);
  });

  it('an effects worker killed after the provider call: the retry reuses the key, one charge', async () => {
    await s.services.kill('effects');
    const fx = await s.services.start('effects', { CHECKOUT_HANG_AFTER_CHARGE: '1' });
    const o = await placeOrder(s.db, s.saga, 'erin', [{ sku: 'cheese', n: 1 }]);
    await fx.waitFor('charged ');
    const key = fx.output.find((l) => l.startsWith('charged '))!.slice('charged '.length);
    await s.services.kill('effects');
    expect(await fx.exited).toBe('SIGKILL');
    // Charged at the provider, nothing committed.
    expect((await s.saga.get(o.saga))!.status).toBe('running');
    expect(await s.db.table<Payment>('payments').get(key)).toBeUndefined();

    await s.services.start('effects');
    await waitStatus(s, o.id, ['done']);
    const c = (await providerCharges(s.provider.url)).charges.filter((x) => x.order === o.id);
    // One charge, called again (at least) by the retry, with the same key.
    expect(c).toMatchObject([{ key, amount: o.total, status: 'charged', refunded: false }]);
    expect(c[0]!.calls).toBeGreaterThanOrEqual(2);
    const pays = (await all<Payment>(s.db, 'payments')).filter((p) => p.order === o.id);
    expect(pays.map((p) => [p.id, p.status])).toEqual([[key, 'paid']]);
  });

  it('a payment that fails: the stock is released, nothing is charged', async () => {
    const before = await available(s.db);
    const o = await placeOrder(s.db, s.saga, 'decline-dave', [
      { sku: 'dates', n: 3 },
      { sku: 'apple', n: 1 },
    ]);
    const done = await waitStatus(s, o.id, ['failed']);
    expect(done.status).toBe('failed');
    const state = (await s.saga.get(o.saga))!;
    expect(state.data.error).toMatchObject({ message: 'payment declined' });
    // The reserve step ran and was compensated.
    expect(state.done).toEqual([0]);
    expect(await available(s.db)).toEqual(before);
    expect((await all<Reservation>(s.db, 'reservations')).filter((r) => r.order === o.id)).toEqual(
      [],
    );
    const pays = (await all<Payment>(s.db, 'payments')).filter((p) => p.order === o.id);
    expect(pays.map((p) => p.status)).toEqual(['declined']);
    const c = (await providerCharges(s.provider.url)).charges.filter((x) => x.order === o.id);
    expect(c.map((x) => x.status)).toEqual(['declined']);
    expect(await s.db.table<Shipment>('shipments').get(o.id)).toBeUndefined();
  });

  it('out of stock: the saga fails at once, nothing to compensate', async () => {
    const before = await available(s.db);
    const o = await placeOrder(s.db, s.saga, 'frank', [{ sku: 'bread', n: 50 }]);
    await waitStatus(s, o.id, ['failed']);
    const state = (await s.saga.get(o.saga))!;
    expect(state.data.error).toMatchObject({ message: 'out of stock' });
    expect(state.done).toEqual([]);
    expect(await available(s.db)).toEqual(before);
  });

  it('a pay timeout: the saga fails, the stock comes back, the late charge is voided', async () => {
    // A short pay timeout (the orchestrator schedules it), and no effects worker.
    await s.services.kill('orchestrator');
    await s.services.start('orchestrator', { CHECKOUT_TIMEOUTS: JSON.stringify({ pay: 1500 }) });
    await s.services.kill('effects');
    const before = await available(s.db);
    const o = await placeOrder(s.db, s.saga, 'gina', [{ sku: 'apple', n: 1 }]);
    await waitStatus(s, o.id, ['failed']);
    const state = (await s.saga.get(o.saga))!;
    expect(state.data.error).toEqual({ message: 'timeout', step: 'pay' });
    expect(await available(s.db)).toEqual(before);

    // The effects worker comes back and finds the charge command, stale.
    await s.services.start('effects');
    await until(
      async () =>
        (await all<Payment>(s.db, 'payments')).some(
          (p) => p.order === o.id && p.status === 'voided',
        ),
      30_000,
      'the voided payment',
    );
    const c = (await providerCharges(s.provider.url)).charges.filter(
      (x) => x.order === o.id || x.status === 'voided',
    );
    // Never charged: the key is voided at the provider, with no call.
    expect(c.map((x) => [x.status, x.calls])).toEqual([['voided', 0]]);
    expect((await order(s.db, o.id))!.status).toBe('failed');
  });
});

/** A small deterministic generator (the run can be repeated). */
function rng(seed: number) {
  let x = seed >>> 0;
  return () => {
    x = (x * 1664525 + 1013904223) >>> 0;
    return x / 2 ** 32;
  };
}

describe('100 orders while workers are killed at random', () => {
  let s: Shop;
  const initial = { apple: 60, bread: 60, cheese: 60, dates: 60 };

  beforeAll(async () => {
    // Timeouts come from the workers' schedulers only (killed too).
    s = await shop({ scheduler: false });
    await restock(s.db, initial);
    await s.services.startAll();
  });

  afterAll(async () => {
    await s?.stop();
  });

  it('every saga ends done or failed; stock and payments balance', async () => {
    const rand = rng(20261012);
    const skus = Object.keys(CATALOG);
    const wanted = Array.from({ length: 100 }, (_, i) => {
      const lines: Item[] = [];
      for (let k = 1 + Math.floor(rand() * 2); k > 0; k--) {
        lines.push({ sku: skus[Math.floor(rand() * skus.length)]!, n: 1 + Math.floor(rand() * 3) });
      }
      return { customer: i % 6 === 5 ? `decline-${i}` : `c${i}`, lines };
    });

    // Kill a random service every 0.2–0.7 s, and start it again.
    let chaos = true;
    let kills = 0;
    const services = ['stock', 'payment', 'effects', 'shipping', 'orchestrator'] as const;
    const killer = (async () => {
      while (chaos) {
        await sleep(200 + rand() * 500);
        if (!chaos) break;
        const v = services[Math.floor(rand() * services.length)]!;
        await s.services.kill(v);
        kills++;
        await s.services.start(v, {}, false);
      }
    })();

    const orders: Order[] = [];
    let next = 0;
    await Promise.all(
      Array.from({ length: 4 }, async () => {
        while (next < wanted.length) {
          const x = wanted[next++]!;
          orders.push(await placeOrder(s.db, s.saga, x.customer, x.lines));
        }
      }),
    );

    const final = new Set(['done', 'failed']);
    const sagaStatus = async (o: Order) => (await s.saga.get(o.saga))?.status;
    try {
      await until(
        async () => {
          for (const o of orders) if (!final.has((await sagaStatus(o)) ?? '')) return false;
          return true;
        },
        150_000,
        'every saga to end',
      );
    } catch (e) {
      const st = await Promise.all(orders.map(sagaStatus));
      throw new Error(`${(e as Error).message}: ${JSON.stringify(st)}\n${s.services.dump()}`);
    } finally {
      chaos = false;
      await killer;
    }
    // Every service up again, so the order statuses catch up.
    for (const v of services) if (!s.services.procs.has(v)) await s.services.start(v);
    await until(
      async () => {
        for (const o of orders) {
          if ((await order(s.db, o.id))?.status !== (await sagaStatus(o))) return false;
        }
        return true;
      },
      60_000,
      'every order status to follow its saga',
    );

    const byId = new Map(orders.map((o) => [o.id, o]));
    const status = new Map<string, string>();
    for (const o of orders) status.set(o.id, (await sagaStatus(o))!);
    const done = orders.filter((o) => status.get(o.id) === 'done');
    const failed = orders.filter((o) => status.get(o.id) === 'failed');
    expect(done.length + failed.length).toBe(100);
    // Both outcomes happened: declined cards, and stock running out.
    expect(done.length).toBeGreaterThan(10);
    expect(failed.length).toBeGreaterThan(10);

    // Stock: available + reserved-and-not-released = initial; only done orders hold reservations.
    const res = await all<Reservation>(s.db, 'reservations');
    for (const r of res) expect(status.get(r.order)).toBe('done');
    const stock = await available(s.db);
    for (const sku of skus) {
      const held = res.filter((r) => r.sku === sku).reduce((a, r) => a + r.n, 0);
      expect(stock[sku]! + held).toBe(initial[sku as keyof typeof initial]);
    }
    const reservedOf = (o: Order) =>
      res
        .filter((r) => r.order === o.id)
        .map((r) => [r.sku, r.n])
        .sort();
    for (const o of done) expect(reservedOf(o)).toEqual(o.items.map((i) => [i.sku, i.n]).sort());

    // Payments: charges minus refunds = the totals of the done orders, one charge each.
    const doneTotal = done.reduce((a, o) => a + o.total, 0);
    const { charges } = await providerCharges(s.provider.url);
    const kept = charges.filter((c) => c.status === 'charged' && !c.refunded);
    const charged = charges.filter((c) => c.status === 'charged').reduce((a, c) => a + c.amount, 0);
    const refunded = charges
      .filter((c) => c.status === 'charged' && c.refunded)
      .reduce((a, c) => a + c.amount, 0);
    expect(charged - refunded).toBe(doneTotal);
    expect(kept.map((c) => c.order).sort()).toEqual(done.map((o) => o.id).sort());
    for (const c of kept) expect(c.amount).toBe(byId.get(c.order)!.total);
    const pays = await all<Payment>(s.db, 'payments');
    const paid = pays.filter((p) => p.status === 'paid');
    expect(paid.map((p) => p.order).sort()).toEqual(done.map((o) => o.id).sort());
    expect(paid.reduce((a, p) => a + p.amount, 0)).toBe(doneTotal);
    // Each payment row is the provider's charge of the same key.
    for (const p of paid) expect(kept.some((c) => c.key === p.id)).toBe(true);
    // Every done order shipped, no failed one.
    const ships = await all<Shipment>(s.db, 'shipments');
    expect(ships.map((x) => x.order).sort()).toEqual(done.map((o) => o.id).sort());
    const why = new Map<string, number>();
    for (const o of failed) {
      const e = (await s.saga.get(o.saga))!.data.error as { message?: string; step?: string };
      const k = `${e.message}${e.step ? ` (${e.step})` : ''}`;
      why.set(k, (why.get(k) ?? 0) + 1);
    }
    console.log(
      `100 orders: ${done.length} done, ${failed.length} failed ${JSON.stringify(Object.fromEntries(why))}, ` +
        `${kills} workers killed, ` +
        `${charges.reduce((a, c) => a + c.calls, 0)} provider charge calls for ${charges.length} keys`,
    );
    expect(kills).toBeGreaterThanOrEqual(8);
  }, 300_000);
});
