// The echo handler as a Node worker: signs in from the environment
// (harness.workerEnv), opens `echo` and serves the service until killed.
// ECHO_LEASE_TTL_MS sets the handler lease's lifetime (how long a takeover
// waits after a kill), ECHO_SERVICE the service (`svc/<name>`, default
// echo), ECHO_LAYER the layer: `raw` (default), `tls` (echo behind TLS,
// with the `echo` certificate of `certs`), `mirror` (the mirror behind
// TLS, default service mirror-tls) or `ssh` (echo over SSH, with the host
// key of `certs`, default service echo-ssh). Built to
// apps/dist/echo/workers/handler.mjs.
import { type Layer, type ServeHandle, type ServeOptions, serve } from '@zen/svc';
import { workerFromEnv } from '../../shared/node.js';
import { DB_OPTIONS, echo, mirror, NS, sshEcho, sshLayer, tlsLayer } from '../src/echo.js';

const w = await workerFromEnv();
const db = await w.open(NS, { ...DB_OPTIONS, warn: (m) => console.log(`warn: ${m}`) });
const ttl = Number(process.env.ECHO_LEASE_TTL_MS ?? 5000);
const kind = process.env.ECHO_LAYER ?? 'raw';
const defaults: Record<string, string> = { mirror: 'mirror-tls', ssh: 'echo-ssh' };
const name = process.env.ECHO_SERVICE ?? defaults[kind] ?? 'echo';
const common = {
  service: ['svc', name],
  leaseTtlMs: ttl,
  renewMs: Math.max(100, Math.floor(ttl / 5)),
  onError: (e: unknown) => console.log(`error: ${e instanceof Error ? e.message : String(e)}`),
} satisfies Partial<ServeOptions>;
let h: Pick<ServeHandle, 'stop'>;
if (kind === 'ssh') {
  h = serve(db, { ...common, layer: await sshLayer(db), handler: sshEcho });
} else {
  const layer: Layer | undefined =
    kind === 'tls' || kind === 'mirror' ? await tlsLayer(db) : undefined;
  h = serve(db, {
    ...common,
    handler: kind === 'mirror' ? mirror : echo,
    ...(layer ? { layer } : {}),
  });
}
console.log('handler ready');
process.on('SIGTERM', async () => {
  await h.stop();
  await w.close();
  process.exit(0);
});
