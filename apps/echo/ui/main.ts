// The echo app's page: serve the echo service from this tab, plain or over
// TLS (with the `echo` certificate of `certs`), and the lines it stored,
// live. A peer connects to the service's port (`nc host port`, or
// `openssl s_client -connect host:port` for TLS).
import { type Layer, serve } from '@zen/svc';
import { boot, h, ready, render } from '../../shared/boot.js';
import { DB_OPTIONS, echo, lines, NS, SERVICE, TLS_SERVICE, tlsLayer } from '../src/echo.js';

const ctx = await boot({ app: 'echo', ns: NS, db: DB_OPTIONS });
const { db, root } = ctx;

const list = h('ul', { id: 'lines' });
const show = async () => {
  const rows = await lines(db);
  render(
    list,
    rows.map((r) => h('li', { class: 'line' }, `${r.conn.slice(0, 8)} ${r.n}: ${r.line}`)),
  );
};

/** A button that starts and stops one service, with its status. */
function toggle(id: string, label: string, service: string[], layer?: () => Promise<Layer>) {
  const status = h('span', { id: `${id}-status` }, 'stopped');
  let handle: ReturnType<typeof serve> | undefined;
  const button = h(
    'button',
    {
      id: `${id}-toggle`,
      onclick: async () => {
        if (handle) {
          const h0 = handle;
          handle = undefined;
          status.textContent = 'stopping';
          await h0.stop();
          status.textContent = 'stopped';
          button.textContent = label;
          return;
        }
        let l: Layer | undefined;
        try {
          l = await layer?.();
        } catch (e) {
          status.textContent = `error: ${e instanceof Error ? e.message : String(e)}`;
          return;
        }
        handle = serve(db, {
          service,
          handler: async (c, tx) => {
            await echo(c, tx);
            tx.after(() => void show());
          },
          ...(l ? { layer: l } : {}),
          onError: (e) => {
            const msg = e instanceof Error ? e.message : String(e);
            console.log(`svc: ${msg}`);
            status.textContent = `error: ${msg}`;
          },
        });
        status.textContent = 'serving';
        button.textContent = 'Stop';
      },
    },
    label,
  );
  return {
    el: h('p', {}, button, ' ', status),
    serving: () => handle?.running === true,
    conns: () => handle?.conns ?? 0,
  };
}

const plain = toggle('serve', 'Serve', SERVICE);
const overTls = toggle('serve-tls', 'Serve TLS', TLS_SERVICE, () => tlsLayer(db));

render(root, plain.el, overTls.el, h('h2', {}, 'Lines'), list);
await show();
// Lines another handler stored.
setInterval(() => void show(), 2000);
ready(ctx, {
  serving: plain.serving,
  conns: plain.conns,
  servingTls: overTls.serving,
  connsTls: overTls.conns,
});
