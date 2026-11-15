// `zen-git worker` (spec/git.md §9, §13): the repack job of one fs, its gc,
// and the public mirrors it keeps up to date.
import { Consumer } from '@zen/client';
import type { ConsumerHandle, Delivered, Value } from '@zen/db';
import { publish } from './publish.js';
import { errText, signIn } from './signin.js';
import {
  collect,
  GC_EVENT,
  GRACE_MS,
  pushesTopic,
  REPACK_EVENT,
  repack,
  repackTopic,
} from './store.js';
import { parseRepoUrl } from './url.js';

export interface WorkerOptions {
  /** The fs: `zen://host/fs` parsed. */
  server: string;
  host: string;
  fs: number;
  /** The gc's grace (default 24 h). */
  graceMs?: number;
  /** Repo → directory: publish it there after each push (§13). */
  publish?: Map<string, string>;
  /** One line per repack, gc and publish (default stderr). */
  log?: (line: string) => void;
  /** Stops the worker. */
  signal: AbortSignal;
  /** Test only: called after a repack's upload, before its transaction. */
  afterUpload?: () => void | Promise<void>;
}

/** How often a handler renews its group's lease (a third of the default 10 s lease). */
const RENEW_MS = 3_000;

/**
 * Run `work` while renewing the group's lease: the client renews it only
 * between deliveries, and a repack of many packs outlasts the lease.
 */
async function keepingLease<T>(h: ConsumerHandle | undefined, work: () => Promise<T>): Promise<T> {
  const timer = setInterval(() => void h?.group?.lease().catch(() => {}), RENEW_MS);
  try {
    return await work();
  } finally {
    clearInterval(timer);
  }
}

/** Run until `signal` aborts; then stop the consumers, releasing their leases. */
export async function runWorker(o: WorkerOptions): Promise<void> {
  const log = o.log ?? ((l: string) => process.stderr.write(`${l}\n`));
  const trace = !!process.env.ZEN_GIT_TRACE;
  if (trace) {
    // Diagnostics: every lease and next call of the worker's consumers, timed.
    const proto = Consumer.prototype as unknown as Record<
      string,
      (...a: unknown[]) => Promise<unknown>
    >;
    for (const name of ['lease', 'next', 'ack'] as const) {
      const f = proto[name]!;
      proto[name] = async function (this: unknown, ...args: unknown[]) {
        const t0 = Date.now();
        try {
          const r = await f.apply(this, args);
          const n = Array.isArray(r) ? ` ${r.length} events` : '';
          const g = Buffer.from((this as { group: Uint8Array }).group)
            .toString('hex')
            .slice(0, 6);
          log(`trace: ${t0} ${name} ${g}${n} ${Date.now() - t0} ms`);
          return r;
        } catch (e) {
          log(`trace: ${t0} ${name} failed after ${Date.now() - t0} ms: ${errText(e)}`);
          throw e;
        }
      };
    }
  }
  const z = await signIn(o.server, o.fs, undefined, { scheduler: true });
  const handles: ConsumerHandle[] = [];
  const onError = (e: unknown) => log(`error: ${errText(e)}`);
  try {
    let repackH: ConsumerHandle | undefined;
    repackH = z.db.consumeManual(
      { group: 'repack', topic: repackTopic(z.db), mode: 'sequential', onError },
      (msg: Delivered<Value>) =>
        keepingLease(repackH, async () => {
          const body = msg.body as Map<Value, Value>;
          const repo = body.get(1) as string;
          const h0 = performance.now();
          if (trace) log(`trace: ${Date.now()} start ${msg.type} ${repo}`);
          let saw = '';
          if (msg.type === REPACK_EVENT) {
            const t0 = performance.now();
            const r = await repack(z.db, z.fs, repo, {
              graceMs: o.graceMs ?? GRACE_MS,
              ...(o.afterUpload ? { afterUpload: o.afterUpload } : {}),
            });
            const s = ((performance.now() - t0) / 1000).toFixed(2);
            saw = r.ok ? `rolled ${r.rolled}, left ${r.left}` : r.why;
            if (!r.ok) log(`${repo}: repack refused (${r.why})`);
            else if (r.rolled) {
              const mb = ((r.size ?? 0) / 1e6).toFixed(2);
              log(`${repo}: ${r.rolled} packs → 1, ${mb} MB, ${s} s; ${r.left} live`);
            }
          } else if (msg.type === GC_EVENT) {
            const n = await collect(z.db, z.fs, body);
            if (n) log(`${repo}: gc removed ${n} packs`);
          }
          const a0 = performance.now();
          await msg.ack();
          if (trace) {
            log(
              `trace: ${Date.now()} ${msg.type} ${repo} handled ${(a0 - h0).toFixed(0)} ms, ack ${(performance.now() - a0).toFixed(0)} ms, attempts ${msg.attempts}; ${saw}`,
            );
          }
        }),
    );
    handles.push(repackH);
    for (const [repo, dir] of o.publish ?? []) {
      const u = parseRepoUrl(
        `${o.server.startsWith('https:') ? 'zen' : 'zen+http'}://${o.host}/${o.fs}/${repo}`,
      );
      let h: ConsumerHandle | undefined;
      h = z.db.consumeManual(
        { group: 'publish', topic: pushesTopic(z.db, repo), mode: 'sequential', onError },
        (msg: Delivered<Value>) =>
          keepingLease(h, async () => {
            const at = await publish(z, u, dir);
            log(`${repo}: published to ${at}`);
            await msg.ack();
          }),
      );
      handles.push(h);
    }
    log(`worker: fs ${o.fs}, ${handles.length} group(s)`);
    await new Promise<void>((r) => {
      if (o.signal.aborted) r();
      else o.signal.addEventListener('abort', () => r(), { once: true });
    });
  } finally {
    for (const h of handles) await h.stop().catch(() => {});
    await z.close();
  }
}
