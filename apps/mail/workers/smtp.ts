// Inbound SMTP as a Node worker: signs in from the environment
// (harness.workerEnv), opens `mail` and serves `svc/smtp` with STARTTLS
// (the `smtp` certificate of `certs`) and the inbox seed of `certs`, if
// there is one, until killed. Drafts left by connections that died
// mid-DATA are swept every 10 minutes. MAIL_LEASE_TTL_MS sets the handler
// lease's lifetime (how long a takeover waits after a kill); MAIL_HOST the
// server's name (default mx.test); MAIL_STALL=data-end makes the handler
// print `stall` and hang inside the transaction at the end of DATA, for
// kill tests. Built to apps/dist/mail/workers/smtp.mjs.
import { workerFromEnv } from '../../shared/node.js';
import { DB_OPTIONS, inboxSeed, NS, serveSmtp, sweepDrafts } from '../src/mail.js';

const w = await workerFromEnv();
const db = await w.open(NS, { ...DB_OPTIONS, warn: (m) => console.log(`warn: ${m}`) });
const ttl = Number(process.env.MAIL_LEASE_TTL_MS ?? 5000);
const inbox = await inboxSeed(db);
const stall = process.env.MAIL_STALL === 'data-end';
const h = await serveSmtp(db, {
  leaseTtlMs: ttl,
  renewMs: Math.max(100, Math.floor(ttl / 5)),
  onError: (e: unknown) => console.log(`error: ${e instanceof Error ? e.message : String(e)}`),
  ...(inbox ? { inbox } : {}),
  smtp: {
    host: process.env.MAIL_HOST ?? 'mx.test',
    ...(stall
      ? {
          onDataEnd: async () => {
            console.log('stall');
            await new Promise(() => {});
          },
        }
      : {}),
  },
});
const sweep = setInterval(
  () => void sweepDrafts(db, 10 * 60_000).catch((e) => console.log(`error: sweep: ${e}`)),
  10 * 60_000,
);
console.log('handler ready');
process.on('SIGTERM', async () => {
  clearInterval(sweep);
  await h.stop();
  await w.close();
  process.exit(0);
});
