// The checkout app's page: place an order from the stock, and the orders
// with their live status (the `orders` change events). The services and the
// saga's orchestrator run as Node workers (workers/); this page only starts
// sagas.
import { boot, h, message, ready, render } from '../../shared/boot.js';
import {
  CATALOG,
  checkoutSaga,
  DB_OPTIONS,
  type Item,
  listOrders,
  listStock,
  NS,
  type Order,
  placeOrder,
  restock,
  type Stock,
} from '../src/checkout.js';

const ctx = await boot({ app: 'checkout', ns: NS, db: DB_OPTIONS });
const { db, root } = ctx;
const saga = checkoutSaga(db, { orchestrate: false });

const money = (cents: number) => `$${(cents / 100).toFixed(2)}`;
const customer = h('input', { id: 'customer', placeholder: 'customer', value: ctx.user });
const qty = new Map(
  Object.keys(CATALOG).map((sku) => [
    sku,
    h('input', { id: `qty-${sku}`, type: 'number', min: 0, value: 0, class: 'qty' }),
  ]),
);
const stockEl = h('table', { id: 'stock' });
const ordersEl = h('ul', { id: 'orders' });
const error = h('p', { id: 'error', class: 'zen-error' });

async function showStock(): Promise<Stock[]> {
  const rows = await listStock(db);
  const by = new Map(rows.map((s) => [s.sku, s.available]));
  render(
    stockEl,
    h(
      'tr',
      {},
      h('th', {}, 'item'),
      h('th', {}, 'price'),
      h('th', {}, 'available'),
      h('th', {}, 'order'),
    ),
    Object.entries(CATALOG).map(([sku, c]) =>
      h(
        'tr',
        { id: `stock-${sku}` },
        h('td', {}, c.name),
        h('td', {}, money(c.price)),
        h('td', { class: 'available' }, by.get(sku) ?? 0),
        h('td', {}, qty.get(sku)!),
      ),
    ),
  );
  return rows;
}

async function showOrders(): Promise<Order[]> {
  const orders = await listOrders(db);
  render(
    ordersEl,
    orders.map((o) =>
      h(
        'li',
        { class: `order ${o.status}`, 'data-id': o.id },
        h('span', { class: 'customer' }, o.customer),
        ' · ',
        o.items.map((i) => `${i.n} ${CATALOG[i.sku]?.name ?? i.sku}`).join(', '),
        ' · ',
        money(o.total),
        ' · ',
        h('span', { class: 'status' }, o.status),
      ),
    ),
  );
  return orders;
}

// One refresh at a time, so an older read never renders over a newer one.
let chain: Promise<unknown> = Promise.resolve();
const refresh = () => {
  chain = chain
    .then(() => Promise.all([showOrders(), showStock()]))
    .catch((e) => {
      error.textContent = message(e);
    });
  return chain;
};

async function place(): Promise<Order> {
  const items: Item[] = [...qty].map(([sku, el]) => ({ sku, n: Number(el.value) || 0 }));
  const o = await placeOrder(db, saga, customer.value || ctx.user, items);
  for (const el of qty.values()) el.value = '0';
  return o;
}

render(
  root,
  h('h2', {}, 'Stock'),
  stockEl,
  h(
    'button',
    {
      id: 'restock',
      onclick: async () => {
        await restock(db, Object.fromEntries(Object.keys(CATALOG).map((s) => [s, 10])));
        await refresh();
      },
    },
    'Restock (10 of each)',
  ),
  h(
    'form',
    {
      id: 'order-form',
      onsubmit: async (e: Event) => {
        e.preventDefault();
        error.textContent = '';
        try {
          await place();
        } catch (err) {
          error.textContent = message(err);
        }
      },
    },
    customer,
    h('button', { id: 'place', type: 'submit' }, 'Place order'),
    h('small', {}, ' (a customer named "decline…" has their card declined)'),
  ),
  error,
  h('h2', {}, 'Orders'),
  ordersEl,
);

// Every change of an order (placed, or its saga moved) re-renders.
await db.table<Order>('orders').on(() => void refresh());
await refresh();
ready(ctx, {
  saga,
  orders: () => listOrders(db),
  stock: () => listStock(db),
  restock: (counts: Record<string, number>) => restock(db, counts).then(refresh),
  place: (who: string, items: Item[]) => placeOrder(db, saga, who, items),
});
