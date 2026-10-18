// The echo app's page: serve the echo service from this tab, plain, over
// TLS (with the `echo` certificate of `certs`) or over SSH (with the host
// key of `certs` and the keys of `ssh_keys`), and the lines it stored,
// live. A peer connects to the service's port (`nc host port`, `openssl
// s_client -connect host:port` for TLS, `ssh -p port user@host lines`).
import type { DbTransaction } from '@zen/db';
import { type ServeHandle, serve } from '@zen/svc';
import { boot, h, ready, render } from '../../shared/boot.js';
import {
  DB_OPTIONS,
  echo,
  lines,
  NS,
  SERVICE,
  SSH_SERVICE,
  sshEcho,
  sshLayer,
  TLS_SERVICE,
  tlsLayer,
} from '../src/echo.js';

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

type Handle = Pick<ServeHandle, 'stop' | 'running' | 'conns'>;
type Start = (onError: (e: unknown) => void) => Promise<Handle>;

/** Show new lines after a step commits. */
const showing = (tx: DbTransaction) => tx.after(() => void show());

/** A button that starts and stops one service, with its status. */
function toggle(id: string, label: string, start: Start) {
  const status = h('span', { id: `${id}-status` }, 'stopped');
  let handle: Handle | undefined;
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
        const onError = (e: unknown) => {
          const msg = e instanceof Error ? e.message : String(e);
          console.log(`svc: ${msg}`);
          status.textContent = `error: ${msg}`;
        };
        try {
          handle = await start(onError);
        } catch (e) {
          onError(e);
          return;
        }
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

const plain = toggle('serve', 'Serve', async (onError) =>
  serve(db, {
    service: SERVICE,
    handler: async (c, tx) => {
      await echo(c, tx);
      showing(tx);
    },
    onError,
  }),
);
const overTls = toggle('serve-tls', 'Serve TLS', async (onError) =>
  serve(db, {
    service: TLS_SERVICE,
    layer: await tlsLayer(db),
    handler: async (c, tx) => {
      await echo(c, tx);
      showing(tx);
    },
    onError,
  }),
);
const overSsh = toggle('serve-ssh', 'Serve SSH', async (onError) =>
  serve(db, {
    service: SSH_SERVICE,
    layer: await sshLayer(db),
    handler: async (c, tx) => {
      await sshEcho(c, tx);
      showing(tx);
    },
    onError,
  }),
);

render(root, plain.el, overTls.el, overSsh.el, h('h2', {}, 'Lines'), list);
await show();
// Lines another handler stored.
setInterval(() => void show(), 2000);
ready(ctx, {
  serving: plain.serving,
  conns: plain.conns,
  servingTls: overTls.serving,
  connsTls: overTls.conns,
  servingSsh: overSsh.serving,
  connsSsh: overSsh.conns,
});
