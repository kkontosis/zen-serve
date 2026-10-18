// The echo app's page: serve the echo service from this tab, and the lines
// it stored, live. A peer connects to the service's port (`nc host port`).
import { serve } from '@zen/svc';
import { boot, h, ready, render } from '../../shared/boot.js';
import { DB_OPTIONS, echo, lines, NS, SERVICE } from '../src/echo.js';

const ctx = await boot({ app: 'echo', ns: NS, db: DB_OPTIONS });
const { db, root } = ctx;

const list = h('ul', { id: 'lines' });
const status = h('span', { id: 'serve-status' }, 'stopped');
const show = async () => {
  const rows = await lines(db);
  render(
    list,
    rows.map((r) => h('li', { class: 'line' }, `${r.conn.slice(0, 8)} ${r.n}: ${r.line}`)),
  );
};

let handle: ReturnType<typeof serve> | undefined;
const button = h(
  'button',
  {
    id: 'serve-toggle',
    onclick: async () => {
      if (handle) {
        const h0 = handle;
        handle = undefined;
        status.textContent = 'stopping';
        await h0.stop();
        status.textContent = 'stopped';
        button.textContent = 'Serve';
      } else {
        handle = serve(db, {
          service: SERVICE,
          handler: async (c, tx) => {
            await echo(c, tx);
            tx.after(() => void show());
          },
          onError: (e) => console.log(`svc: ${e instanceof Error ? e.message : String(e)}`),
        });
        status.textContent = 'serving';
        button.textContent = 'Stop';
      }
    },
  },
  'Serve',
);

render(root, h('p', {}, button, ' ', status), h('h2', {}, 'Lines'), list);
await show();
// Lines another handler stored.
setInterval(() => void show(), 2000);
ready(ctx, { serving: () => handle?.running === true, conns: () => handle?.conns ?? 0 });
