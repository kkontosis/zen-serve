// The echo handler as a Node worker: signs in from the environment
// (harness.workerEnv), opens `echo` and serves the service until killed.
// ECHO_LEASE_TTL_MS sets the handler lease's lifetime (how long a takeover
// waits after a kill), ECHO_SERVICE the service (`svc/<name>`, default
// echo), ECHO_LAYER the layer: `raw` (default), `tls` (echo behind TLS,
// with the `echo` certificate of `certs`) or `mirror` (the mirror behind
// TLS, default service mirror-tls). Built to apps/dist/echo/workers/handler.mjs.
import type { Layer } from '@zen/svc';
import { workerFromEnv } from '../../shared/node.js';
import { DB_OPTIONS, echo, mirror, NS, serveEcho, tlsLayer } from '../src/echo.js';

const w = await workerFromEnv();
const db = await w.open(NS, { ...DB_OPTIONS, warn: (m) => console.log(`warn: ${m}`) });
const ttl = Number(process.env.ECHO_LEASE_TTL_MS ?? 5000);
const kind = process.env.ECHO_LAYER ?? 'raw';
let layer: Layer | undefined;
if (kind === 'tls' || kind === 'mirror') layer = await tlsLayer(db);
const name = process.env.ECHO_SERVICE ?? (kind === 'mirror' ? 'mirror-tls' : 'echo');
const h = serveEcho(db, {
  service: ['svc', name],
  handler: kind === 'mirror' ? mirror : echo,
  ...(layer ? { layer } : {}),
  leaseTtlMs: ttl,
  renewMs: Math.max(100, Math.floor(ttl / 5)),
  onError: (e) => console.log(`error: ${e instanceof Error ? e.message : String(e)}`),
});
console.log('handler ready');
process.on('SIGTERM', async () => {
  await h.stop();
  await w.close();
  process.exit(0);
});
