// IMAP as a Node worker: signs in from the environment (harness.workerEnv),
// opens `mail` and serves `svc/imaps` with TLS (the `smtp` certificate of
// `certs`), waking idling sessions when mail arrives or a mailbox changes,
// until killed. MAIL_LEASE_TTL_MS sets the handler lease's lifetime (how
// long a takeover waits after a kill); MAIL_STALL=fetch makes the handler
// print `stall` and hang inside the transaction of the step after a
// FETCH's first one, for kill tests. Built to apps/dist/mail/workers/imap.mjs.
import { workerFromEnv } from '../../shared/node.js';
import { DB_OPTIONS, NS, serveImap, wakeOnMail } from '../src/mail.js';

const w = await workerFromEnv();
const db = await w.open(NS, { ...DB_OPTIONS, warn: (m) => console.log(`warn: ${m}`) });
const ttl = Number(process.env.MAIL_LEASE_TTL_MS ?? 5000);
const stall = process.env.MAIL_STALL === 'fetch';
const h = await serveImap(db, {
  leaseTtlMs: ttl,
  renewMs: Math.max(100, Math.floor(ttl / 5)),
  onError: (e: unknown) => console.log(`error: ${e instanceof Error ? e.message : String(e)}`),
  imap: stall
    ? {
        onFetchMore: async () => {
          console.log('stall');
          await new Promise(() => {});
        },
      }
    : {},
});
const unwake = await wakeOnMail(db, h);
console.log('handler ready');
process.on('SIGTERM', async () => {
  await unwake();
  await h.stop();
  await w.close();
  process.exit(0);
});
