// The checkout app's logic (EXAMPLES.md §4), over a `Db` and no DOM: the
// schema of database `shop`, placing an order, the `checkout` saga, and the
// handlers of each service. The workers (workers/*.ts), the page (ui/) and
// the tests all call these.
//
// Who runs what:
// * The orchestrator of the saga runs in a dedicated Node worker
//   (workers/orchestrator.ts): it is restartable (every transition is one
//   commit, and a killed worker's claims expire), and it keeps the orders'
//   `status` in step with the sagas (`trackStatus`). The page and the tests
//   only start sagas: `checkoutSaga(db, {orchestrate: false})`.
// * Each service is a consumer group in its own worker, signed in as its
//   own member (its own device): stock, payment, effects, shipping.
//
// Timeouts and late replies. A participant's forward step first checks, in
// its own transaction, that its command is still the one the saga waits
// for, its timeout still pending (`claimStep`). The reply and the timeout
// then exclude each other: a step either commits before its
// timeout fires, or does nothing (a late charge is refunded). Without this
// a reservation or a charge made after its timeout fired would never be
// compensated, and stock or money would leak.
import { bytes, type Topic } from '@zen/client';
import {
  appendMsg,
  type ConsumerHandle,
  type Db,
  type DbOptions,
  type DbTransaction,
  type Delivered,
  ReplyError,
  type Saga,
  type SagaState,
  type Value,
} from '@zen/db';

/** The database namespace. */
export const NS = 'shop';

/** One line of an order. */
export interface Item {
  [k: string]: Value;
  sku: string;
  n: number;
}

/** An order row. Amounts are in cents. */
export interface Order {
  [k: string]: Value;
  id: string;
  customer: string;
  items: Item[];
  total: number;
  /** `placed`, then the saga's progress (`statusOf`): `done` or `failed` at the end. */
  status: string;
  /** Unix ms. */
  placed: number;
  /** The checkout saga's id. */
  saga: Uint8Array;
}

/** A stock row. */
export interface Stock {
  [k: string]: Value;
  sku: string;
  available: number;
}

/** A reservation of stock for an order (deleted when released). */
export interface Reservation {
  [k: string]: Value;
  id: string;
  order: string;
  sku: string;
  n: number;
}

/** A payment: one per order. Its id is the charge command's id (the idempotency key). */
export interface Payment {
  [k: string]: Value;
  id: string;
  order: string;
  amount: number;
  /** `paid`, `declined`, `refunded`, or `voided` (charged after the saga gave up, then refunded). */
  status: string;
  provider_ref: string;
}

/** A shipment (its id is the order's). */
export interface Shipment {
  [k: string]: Value;
  id: string;
  order: string;
  status: string;
}

/** The saga's data: its start data, with the steps' replies merged in. */
export interface CheckoutData {
  [k: string]: Value;
  order: string;
  customer: string;
  items: Item[];
  total: number;
}

/** What the shop sells, with prices in cents. */
export const CATALOG: Record<string, { name: string; price: number }> = {
  apple: { name: 'Apple', price: 120 },
  bread: { name: 'Bread', price: 350 },
  cheese: { name: 'Cheese', price: 790 },
  dates: { name: 'Dates', price: 460 },
};

/** Schema and migrations, for `Db.open` (or `boot`). */
export const DB_OPTIONS: DbOptions = {
  schema: 1,
  migrations: {
    1: async (m) => {
      await m.createTable('orders', { pk: ['id'], changes: { image: 'keys' } });
      await m.createIndex('orders', 'by_placed', { fields: [['placed', 'int', 'desc']] });
      await m.createTable('payments', { pk: ['id'] });
      await m.createIndex('payments', 'by_order', {
        fields: [['order', 'text']],
        unique: true,
        kind: 'none',
      });
      await m.createTable('stock', { pk: ['sku'] });
      await m.createTable('reservations', { pk: ['id'] });
      await m.createIndex('reservations', 'by_order_sku', {
        fields: [
          ['order', 'text'],
          ['sku', 'text'],
        ],
        unique: true,
      });
      await m.createTable('shipments', { pk: ['id'] });
    },
  },
};

/** The services' topics. */
export function topics(db: Db) {
  return {
    reserve: db.topic('shop', 'stock', 'reserve'),
    release: db.topic('shop', 'stock', 'release'),
    pay: db.topic('shop', 'pay'),
    refund: db.topic('shop', 'refund'),
    ship: db.topic('shop', 'ship'),
    /** The external effects (zendb.md §12.6): commands the effects worker carries out. */
    charge: db.topic('shop', 'effects', 'charge'),
    refundEffect: db.topic('shop', 'effects', 'refund'),
  };
}

/** The steps' reply timeouts, in ms (a compensation has its step's). */
export interface Timeouts {
  reserve: number;
  pay: number;
  ship: number;
}

/** EXAMPLES.md §4: 10 s, 60 s, 1 day. */
export const TIMEOUTS: Timeouts = { reserve: 10_000, pay: 60_000, ship: 86_400_000 };

/** Options of `checkoutSaga`. */
export interface SagaOpts {
  timeouts?: Partial<Timeouts>;
  /** Run the orchestrator here (default true); false: only start sagas and read them. */
  orchestrate?: boolean;
  onError?: (e: unknown) => void;
}

/**
 * The `checkout` saga: reserve (release), pay (refund), ship. Every process
 * that starts or advances it must use the same timeouts: the starter
 * schedules step 0's, the orchestrator the others'.
 */
export function checkoutSaga(db: Db, o: SagaOpts = {}): Saga<CheckoutData> {
  const t = { ...TIMEOUTS, ...o.timeouts };
  const tp = topics(db);
  const saga = db.saga<CheckoutData>(
    'checkout',
    [
      { name: 'reserve', command: tp.reserve, compensate: tp.release, timeoutMs: t.reserve },
      { name: 'pay', command: tp.pay, compensate: tp.refund, timeoutMs: t.pay },
      { name: 'ship', command: tp.ship, timeoutMs: t.ship },
    ],
    o.onError ? { onError: o.onError } : {},
  );
  // `db.saga` always starts an orchestrator in the owner: stop it at once.
  if (o.orchestrate === false) void saga.stop();
  return saga;
}

/** Merge the lines of one sku, check skus and counts. */
export function normalize(items: Item[]): Item[] {
  const by = new Map<string, number>();
  for (const it of items) {
    if (!CATALOG[it.sku]) throw new Error(`unknown sku ${it.sku}`);
    if (!Number.isSafeInteger(it.n) || it.n < 0) throw new Error(`bad count for ${it.sku}`);
    if (it.n) by.set(it.sku, (by.get(it.sku) ?? 0) + it.n);
  }
  if (!by.size) throw new Error('an order needs an item');
  return [...by].sort(([a], [b]) => (a < b ? -1 : 1)).map(([sku, n]) => ({ sku, n }));
}

/** An order's total in cents. */
export const totalOf = (items: Item[]) =>
  items.reduce((s, it) => s + CATALOG[it.sku]!.price * it.n, 0);

/**
 * Place an order: one transaction inserts the order (`placed`) and starts
 * its saga (the `$sagas` row, the `reserve` command and its timeout).
 */
export async function placeOrder(
  db: Db,
  saga: Saga<CheckoutData>,
  customer: string,
  lines: Item[],
): Promise<Order> {
  const items = normalize(lines);
  const id = crypto.randomUUID();
  const total = totalOf(items);
  // Every saga transition writes the `$sched` index (its timeouts), so
  // under load a start conflicts often: more attempts than the default 8.
  return db.transaction(
    async (tx) => {
      const sagaId = await saga.start({ order: id, customer, items, total }, tx);
      const o: Order = {
        id,
        customer,
        items,
        total,
        status: 'placed',
        placed: Date.now(),
        saga: sagaId,
      };
      await tx.table<Order>('orders').insert(o);
      return o;
    },
    { attempts: 24 },
  );
}

/** The newest orders first. */
export async function listOrders(db: Db, limit = 200): Promise<Order[]> {
  const { rows } = await db
    .table<Order>('orders')
    .query()
    .orderBy('placed', 'desc')
    .limit(limit)
    .page();
  return rows;
}

/** The stock rows, by sku. */
export function listStock(db: Db): Promise<Stock[]> {
  // The catalog's skus by pk, in one read-only transaction (no table scan).
  return db.transaction(async (tx) => {
    const rows: Stock[] = [];
    for (const sku of Object.keys(CATALOG)) {
      const s = await tx.table<Stock>('stock').get(sku);
      if (s) rows.push(s);
    }
    return rows;
  });
}

/** Set the available count of skus (a delivery to the shop). */
export async function restock(db: Db, counts: Record<string, number>): Promise<void> {
  await db.transaction(async (tx) => {
    for (const [sku, available] of Object.entries(counts)) {
      await tx.table<Stock>('stock').put({ sku, available });
    }
  });
}

// ------------------------------------------------------------ participants

/**
 * Whether `m` is the command its saga waits for: its `corr` is the
 * `$sagas` row's, and the step's timeout is still pending. Both rows are
 * in `tx`'s read set, so if the scheduler delivers the timeout before
 * `tx` commits, `tx` conflicts and re-runs; if `tx` commits first, its
 * reply precedes the timeout on the saga's topic (same key), and the
 * timeout is stale. False: the saga moved on, and the step must do
 * nothing. A message from outside a saga is always current.
 *
 * Only reads: the orchestrator cancels the timeout when it handles the
 * reply. (Cancelling here, a write to the `$sched` index, made every
 * participant conflict with every other.) It reads the `$sagas` and
 * `$sched` system tables directly: the saga API has no call for this.
 */
export async function claimStep(tx: DbTransaction, m: Delivered<unknown>): Promise<boolean> {
  if (!m.saga || !m.corr) return true;
  const row = await tx.table<SagaState>('$sagas').get(m.saga);
  if (!row?.corr || !bytes.equal(row.corr, m.corr)) return false;
  // A timeout already delivered (its `$sched` row gone) is on its way to the orchestrator.
  if (row.timeout && !(await tx.table('$sched').get(row.timeout))) return false;
  return true;
}

/**
 * Reply to a saga command in `tx`, as `serve` does (zendb.md §12.5): an
 * event to its `reply_to` keyed by the saga id. For participants that
 * write rows on a failure too, which `serve` (a `ReplyError` rolls the
 * handler's writes back) doesn't allow.
 */
export function replyIn(
  tx: DbTransaction,
  req: Delivered<unknown>,
  ok: boolean,
  body: Value,
): void {
  if (!req.replyTo) return;
  appendMsg(
    tx,
    tx.db.topic(...req.replyTo),
    {
      type: `${req.type}.${ok ? 'reply' : 'error'}`,
      id: bytes.randomBytes(16),
      corr: req.corr ?? req.id,
      ...(req.saga ? { saga: req.saga } : {}),
      ...(req.step !== undefined ? { step: req.step } : {}),
      body,
    },
    req.saga,
  );
}

/**
 * Forward a saga command to another topic with its reply route (corr,
 * `reply_to`, saga, step): whoever handles the copy replies to the saga.
 * The copy has a fresh id, the idempotency key of what it asks for.
 */
export function forward(
  tx: DbTransaction,
  m: Delivered<unknown>,
  to: Topic,
  body: Value,
): Uint8Array {
  const id = bytes.randomBytes(16);
  appendMsg(
    tx,
    to,
    {
      type: m.type,
      id,
      ...(m.corr ? { corr: m.corr } : {}),
      ...(m.replyTo ? { replyTo: m.replyTo } : {}),
      ...(m.saga ? { saga: m.saga } : {}),
      ...(m.step !== undefined ? { step: m.step } : {}),
      body,
    },
    m.saga,
  );
  return id;
}

/** Stock, `reserve`: all lines or none; `ReplyError` when short. */
export async function reserveStock(tx: DbTransaction, m: Delivered<CheckoutData>): Promise<Value> {
  if (!(await claimStep(tx, m))) return null;
  const { order, items } = m.body;
  const stock = tx.table<Stock>('stock');
  const rows: Stock[] = [];
  for (const it of items) {
    const s = await stock.get(it.sku);
    if (!s || s.available < it.n) {
      throw new ReplyError('out of stock', {
        sku: it.sku,
        wanted: it.n,
        available: s?.available ?? 0,
      });
    }
    rows.push(s);
  }
  for (const [i, it] of items.entries()) {
    await stock.put({ ...rows[i]!, available: rows[i]!.available - it.n });
    await tx
      .table<Reservation>('reservations')
      .insert({ id: `${order}/${it.sku}`, order, sku: it.sku, n: it.n });
  }
  return { reserved: items.length };
}

/** Stock, `release` (the compensation): return the order's reservations. Idempotent. */
export async function releaseStock(tx: DbTransaction, m: Delivered<CheckoutData>): Promise<Value> {
  // No `claimStep`: a compensation runs even if its timeout fired (the
  // saga is then `stuck`), since the stock must come back.
  const res = tx.table<Reservation>('reservations');
  const mine = await res.query().where('order', '=', m.body.order).all();
  for (const r of mine) {
    await res.delete(r.id);
    const s = await tx.table<Stock>('stock').get(r.sku);
    await tx.table<Stock>('stock').put({ sku: r.sku, available: (s?.available ?? 0) + r.n });
  }
  return { released: mine.length };
}

/** The body of a charge command. */
export interface ChargeBody {
  [k: string]: Value;
  order: string;
  customer: string;
  amount: number;
}

/** Payment, `pay`: emit the charge to the effects topic; the effects worker replies. */
export function pay(tx: DbTransaction, m: Delivered<CheckoutData>): void {
  const b: ChargeBody = { order: m.body.order, customer: m.body.customer, amount: m.body.total };
  forward(tx, m, topics(tx.db).charge, b);
}

/** Payment, `refund` (the compensation): emit the refund to the effects topic. */
export function refund(tx: DbTransaction, m: Delivered<CheckoutData>): void {
  forward(tx, m, topics(tx.db).refundEffect, { order: m.body.order });
}

/** Shipping, `ship`. */
export async function ship(tx: DbTransaction, m: Delivered<CheckoutData>): Promise<Value> {
  if (!(await claimStep(tx, m))) return null;
  const { order } = m.body;
  await tx.table<Shipment>('shipments').put({ id: order, order, status: 'shipped' });
  return { shipment: order };
}

// ------------------------------------------------------------ effects

/** A charge's result at the provider. */
export interface ChargeResult {
  status: 'charged' | 'declined' | 'voided';
  ref: string;
}

/** The payment provider (workers/provider.ts over HTTP; a fake in tests). */
export interface Provider {
  /** Charge once per `key`: a repeated key returns the first result. */
  charge(r: {
    key: string;
    order: string;
    customer: string;
    amount: number;
  }): Promise<ChargeResult>;
  /** Refund the charge `charge` once per `key`; a charge not made yet can never be made after. */
  refund(r: { key: string; charge: string }): Promise<{ status: string }>;
}

/** The provider at `url` (workers/provider.ts). */
export function httpProvider(url: string): Provider {
  const post = async <R>(path: string, body: unknown): Promise<R> => {
    const r = await fetch(`${url}${path}`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
    });
    if (!r.ok) throw new Error(`provider ${path}: ${r.status}`);
    return (await r.json()) as R;
  };
  return { charge: (r) => post('/charge', r), refund: (r) => post('/refund', r) };
}

/**
 * Effects, `charge`: call the provider with the command's id as the
 * idempotency key, then record the payment and reply to the saga, in one
 * transaction with the consume step. A crash between the call and the
 * commit repeats the call with the same key: one charge.
 */
export async function charge(
  tx: DbTransaction,
  m: Delivered<ChargeBody>,
  provider: Provider,
  afterCall?: (key: string) => Promise<void>,
): Promise<void> {
  const key = bytes.hex(m.id);
  const { order, customer, amount } = m.body;
  const pays = tx.table<Payment>('payments');
  if (!(await claimStep(tx, m))) {
    // The saga gave up on this step: undo what an earlier attempt may have
    // charged, and never charge this key later. No reply (it would be stale).
    await provider.refund({ key: `void-${key}`, charge: key });
    await pays.put({ id: key, order, amount, status: 'voided', provider_ref: '' });
    return;
  }
  const r = await provider.charge({ key, order, customer, amount });
  await afterCall?.(key);
  const ok = r.status === 'charged';
  await pays.put({ id: key, order, amount, status: ok ? 'paid' : 'declined', provider_ref: r.ref });
  if (ok) replyIn(tx, m, true, { payment: key });
  else replyIn(tx, m, false, { message: 'payment declined', data: { order, ref: r.ref } });
}

/** Effects, `refund` (a compensation): refund the order's payment, once. */
export async function refundCharge(
  tx: DbTransaction,
  m: Delivered<{ order: string }>,
  provider: Provider,
): Promise<void> {
  // A compensation: no `claimStep` (see `releaseStock`).
  const pays = tx.table<Payment>('payments');
  const [p] = await pays.query().where('order', '=', m.body.order).all();
  if (p?.status === 'paid') {
    await provider.refund({ key: bytes.hex(m.id), charge: p.id });
    await pays.put({ ...p, status: 'refunded' });
  }
  replyIn(tx, m, true, { refunded: p?.status === 'paid' });
}

// ------------------------------------------------------------ services

/** Options of a service's consumer groups. */
export interface ServiceOpts {
  onError?: (e: unknown) => void;
  /** Deliveries handled at once per group (default 8). */
  maxInflight?: number;
}

/**
 * A participant's consumer options: `per_key` (by saga id: one instance's
 * messages in order), never dead-lettered (a conflict retries).
 */
function groupOpts(group: string, topic: Topic, o: ServiceOpts) {
  return {
    group,
    topic,
    mode: 'per_key' as const,
    maxInflight: o.maxInflight ?? 8,
    maxAttempts: 0,
    // A killed worker's claimed messages come back when its claims expire,
    // but a long poll that is waiting isn't woken by that: poll often.
    waitMs: 2000,
    ...(o.onError ? { onError: (e: unknown) => o.onError!(e) } : {}),
  };
}

/** The stock service: `serve` on reserve and release. */
export function serveStock(db: Db, o: ServiceOpts = {}): ConsumerHandle[] {
  const tp = topics(db);
  // Every order writes the few stock rows: handled in parallel, the
  // transactions mostly conflict with each other. One at a time is faster.
  o = { maxInflight: 1, ...o };
  return [
    db.serve<CheckoutData>(groupOpts('stock', tp.reserve, o), (m, tx) => reserveStock(tx, m)),
    db.serve<CheckoutData>(groupOpts('stock', tp.release, o), (m, tx) => releaseStock(tx, m)),
  ];
}

/** The payment service: pay and refund, as commands to the effects topics. */
export function servePayment(db: Db, o: ServiceOpts = {}): ConsumerHandle[] {
  const tp = topics(db);
  return [
    db.consume<CheckoutData>(groupOpts('payment', tp.pay, o), (m, tx) => pay(tx, m)),
    db.consume<CheckoutData>(groupOpts('payment', tp.refund, o), (m, tx) => refund(tx, m)),
  ];
}

/** The effects worker's groups: charges and refunds at the provider. */
export function serveEffects(
  db: Db,
  provider: Provider,
  o: ServiceOpts & { afterCall?: (key: string) => Promise<void> } = {},
): ConsumerHandle[] {
  const tp = topics(db);
  return [
    db.consume<ChargeBody>(groupOpts('effects', tp.charge, o), (m, tx) =>
      charge(tx, m, provider, o.afterCall),
    ),
    db.consume<{ order: string }>(groupOpts('effects', tp.refundEffect, o), (m, tx) =>
      refundCharge(tx, m, provider),
    ),
  ];
}

/** The shipping service. */
export function serveShipping(db: Db, o: ServiceOpts = {}): ConsumerHandle[] {
  return [
    db.serve<CheckoutData>(groupOpts('shipping', topics(db).ship, o), (m, tx) => ship(tx, m)),
  ];
}

// ------------------------------------------------------------ order status

/** The order status a saga state shows. */
export function statusOf(s: SagaState): string {
  switch (s.status) {
    case 'running':
      return ['reserving', 'paying', 'shipping'][s.step] ?? 'running';
    case 'compensating':
      return ['releasing', 'refunding'][s.step] ?? 'compensating';
    default:
      return s.status;
  }
}

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

/**
 * Keep `orders.status` in step with the sagas. The orchestrator's
 * transactions can't write app rows (the saga API has no hook), so a
 * second group on the saga's topic follows every reply and timeout: it
 * waits until the orchestrator has handled the message (the row's `corr`
 * moved on), then copies the saga's state to its order, exactly once per
 * message. The order's status lags its saga by one commit.
 */
export function trackStatus(db: Db, saga: Saga<CheckoutData>, o: ServiceOpts = {}): ConsumerHandle {
  const handled = (row: SagaState | undefined, m: Delivered<unknown>) =>
    !row?.corr || !m.corr || !bytes.equal(row.corr, m.corr);
  return db.consumeManual(groupOpts('order-status', saga.topic, o), async (m) => {
    if (!m.saga) return m.ack();
    // Wait (a while; then redelivery) for the orchestrator to handle it.
    const end = Date.now() + 5_000;
    while (!handled(await db.table<SagaState>('$sagas').get(m.saga), m)) {
      if (Date.now() > end) throw new Error('the orchestrator has not handled it yet');
      await sleep(50);
    }
    const tx = db.begin({ consumes: m });
    try {
      const row = await tx.table<SagaState>('$sagas').get(m.saga);
      if (row?.saga === saga.name && typeof row.data.order === 'string') {
        const orders = tx.table<Order>('orders');
        const ord = await orders.get(row.data.order);
        const status = statusOf(row);
        if (ord && ord.status !== status) await orders.put({ ...ord, status });
      }
      await tx.commit();
    } catch (e) {
      try {
        tx.abort();
      } catch {
        // Already closed by the failed commit.
      }
      throw e;
    }
  });
}
