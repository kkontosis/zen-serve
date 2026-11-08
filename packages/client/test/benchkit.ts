// Shared parts of the large-file benches (docs/STATS.md §7): a seekable
// random source, hashing, disk checks, and peak memory, the process's own
// and another's (`/proc/PID/status` `VmHWM`, reset through `clear_refs`).
import { execFile, execFileSync } from 'node:child_process';
import { createCipheriv, createHash } from 'node:crypto';
import { readFileSync, statfsSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import type { Session } from '../src/index.js';

export const MiB = 1 << 20;
export const GiB = 1 << 30;

/** The bench's file size: `ZEN_BENCH_GIB` GiB (default 4). */
export function benchBytes(dflt = 4): number {
  return Math.round(Number(process.env.ZEN_BENCH_GIB ?? dflt) * GiB);
}

/** Refuse to start with less than `need` bytes free under the temp dir. */
export function checkDisk(need: number, what: string): void {
  const s = statfsSync(tmpdir());
  const free = s.bavail * s.bsize;
  if (free < need) {
    throw new Error(
      `${what} needs ${(need / GiB).toFixed(1)} GiB free under ${tmpdir()}, ` +
        `and ${(free / GiB).toFixed(1)} GiB is; set TMPDIR or a smaller ZEN_BENCH_GIB`,
    );
  }
}

/**
 * Bytes `from..to` of the AES-128-CTR keystream under the key `seed`
 * (every byte of the key): random-looking, so content-defined cuts
 * resync, and seekable, so a resume restarts it at any offset.
 */
export function* keystream(seed: number, from: number, to: number): Generator<Uint8Array> {
  const block = Math.floor(from / 16);
  const iv = Buffer.alloc(16);
  iv.writeBigUInt64BE(BigInt(block), 8);
  const c = createCipheriv('aes-128-ctr', Buffer.alloc(16, seed), iv);
  if (from > block * 16) c.update(Buffer.alloc(from - block * 16));
  const zeros = Buffer.alloc(MiB);
  for (let pos = from; pos < to; pos += MiB) {
    const n = Math.min(MiB, to - pos);
    const out = c.update(n === MiB ? zeros : zeros.subarray(0, n));
    yield new Uint8Array(out.buffer, out.byteOffset, out.length);
  }
}

/** A part of a file's expected content: a keystream range, or bytes. */
export type Part = { seed: number; from: number; to: number } | Uint8Array;

/** The bytes of `parts`, in order. */
export function* partsOf(parts: Part[]): Generator<Uint8Array> {
  for (const p of parts) {
    if (p instanceof Uint8Array) yield p;
    else yield* keystream(p.seed, p.from, p.to);
  }
}

/** `parts` with `remove` bytes at `offset` replaced by `insert`. */
export function edited(parts: Part[], offset: number, remove: number, insert: Uint8Array): Part[] {
  const out: Part[] = [];
  let pos = 0;
  const end = offset + remove;
  let put = false;
  for (const p of parts) {
    const len = p instanceof Uint8Array ? p.length : p.to - p.from;
    const [a, b] = [pos, pos + len];
    pos = b;
    const slice = (x: number, y: number): Part | undefined => {
      if (y <= x) return undefined;
      if (p instanceof Uint8Array) return p.subarray(x - a, y - a);
      return { seed: p.seed, from: p.from + x - a, to: p.from + y - a };
    };
    const head = slice(a, Math.min(b, offset));
    if (head) out.push(head);
    if (!put && b >= offset) {
      out.push(insert);
      put = true;
    }
    const tail = slice(Math.max(a, end), b);
    if (tail) out.push(tail);
  }
  if (!put) out.push(insert);
  return out;
}

/** SHA-256 (hex) and length of a stream of bytes. */
export async function digest(
  src: Iterable<Uint8Array> | AsyncIterable<Uint8Array>,
): Promise<{ hash: string; bytes: number }> {
  const h = createHash('sha256');
  let bytes = 0;
  for await (const b of src) {
    h.update(b);
    bytes += b.length;
  }
  return { hash: h.digest('hex'), bytes };
}

/** A `ReadableStream`'s chunks as an async iterable. */
export async function* chunksOf(s: ReadableStream<Uint8Array>): AsyncGenerator<Uint8Array> {
  const r = s.getReader();
  try {
    for (let x = await r.read(); !x.done; x = await r.read()) yield x.value;
  } finally {
    r.releaseLock();
  }
}

/** A process's peak resident set (`VmHWM`), in bytes. */
export function peakRss(pid: number | 'self' = 'self'): number {
  const m = /VmHWM:\s+(\d+) kB/.exec(readFileSync(`/proc/${pid}/status`, 'utf8'));
  return m ? Number(m[1]) * 1024 : Number.NaN;
}

/** Reset a process's `VmHWM` to its current RSS (Linux `clear_refs` 5). */
export function resetPeak(pid: number | 'self' = 'self'): void {
  try {
    writeFileSync(`/proc/${pid}/clear_refs`, '5');
  } catch {
    // not permitted: the peak is then since the process started
  }
}

/** Bytes allocated on disk under `dir` (`du`; not the apparent size: redb's file is sparse). */
export function du(dir: string): number {
  try {
    return Number(execFileSync('du', ['-sB1', dir], { encoding: 'utf8' }).split('\t')[0]);
  } catch {
    return Number.NaN;
  }
}

/**
 * The most bytes on disk under `dir` until `stop()`, by `du` every `ms`
 * (run as a child, so the event loop isn't held).
 */
export function diskPeak(dir: string, ms = 2000): { stop(): Promise<number> } {
  let most = 0;
  let on = true;
  const once = () =>
    new Promise<void>((ok) =>
      execFile('du', ['-sB1', dir], (_, out) => {
        const n = Number(String(out).split('\t')[0]);
        if (Number.isFinite(n)) most = Math.max(most, n);
        ok();
      }),
    );
  const loop = (async () => {
    while (on) {
      await once();
      await sleep(ms);
    }
  })();
  return {
    async stop() {
      on = false;
      await loop;
      await once();
      return most;
    },
  };
}

/**
 * The peaks of `process.memoryUsage()` and of extra gauges, sampled every
 * `ms` until `stop()`.
 */
export class Peaks {
  readonly most: Record<string, number> = {};
  private t: ReturnType<typeof setInterval>;
  constructor(
    private gauges: Record<string, () => number> = {},
    ms = 200,
  ) {
    this.sample();
    this.t = setInterval(() => this.sample(), ms);
  }
  private sample() {
    const m = process.memoryUsage();
    const all: Record<string, number> = {
      rss: m.rss,
      heapUsed: m.heapUsed,
      external: m.external,
      arrayBuffers: m.arrayBuffers,
    };
    for (const [k, f] of Object.entries(this.gauges)) all[k] = f();
    for (const [k, v] of Object.entries(all)) {
      if (Number.isFinite(v)) this.most[k] = Math.max(this.most[k] ?? 0, v);
    }
  }
  stop(): Record<string, number> {
    this.sample();
    clearInterval(this.t);
    return this.most;
  }
}

export const mib = (b: number) => (b / MiB).toFixed(b >= 100 * MiB ? 0 : 1);
export const rate = (b: number, ms: number) => (b / MiB / (ms / 1000)).toFixed(1);
export const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** The value at quantile `q` of `xs`. */
export function quantile(xs: number[], q: number): number {
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.floor(q * s.length))]!;
}

/** A deterministic PRNG (xorshift32) giving floats in [0, 1). */
export function prng(seed: number): () => number {
  let x = seed >>> 0 || 1;
  return () => {
    x ^= x << 13;
    x >>>= 0;
    x ^= x >>> 17;
    x ^= x << 5;
    x >>>= 0;
    return x / 2 ** 32;
  };
}

/** What the commits and fetches while `fn` runs carried. */
export interface Traffic {
  commits: number;
  sent: number;
  chunksPut: number;
  pagesPut: number;
  chunksGot: number;
  pagesGot: number;
}

export async function traffic<T>(s: Session, fn: () => Promise<T>): Promise<[T, Traffic]> {
  const orig = s.call;
  const t: Traffic = { commits: 0, sent: 0, chunksPut: 0, pagesPut: 0, chunksGot: 0, pagesGot: 0 };
  type Put = { data: Uint8Array };
  s.call = ((path: string, enc: unknown, dec: unknown, req: unknown) => {
    const r = req as { chunks?: Put[]; pages?: Put[]; ids?: unknown[] };
    if (path === '/v1/commit') {
      t.commits++;
      t.chunksPut += r.chunks?.length ?? 0;
      t.pagesPut += r.pages?.length ?? 0;
      for (const p of [...(r.chunks ?? []), ...(r.pages ?? [])]) t.sent += p.data.length;
    } else if (path === '/v1/fs/chunks/get') t.chunksGot += r.ids!.length;
    else if (path === '/v1/fs/pages/get') t.pagesGot += r.ids!.length;
    return (orig as (...a: unknown[]) => unknown).call(s, path, enc, dec, req);
  }) as Session['call'];
  try {
    return [await fn(), t];
  } finally {
    s.call = orig;
  }
}

export const counts = (t: Traffic) =>
  `fetched ${t.pagesGot} pages and ${t.chunksGot} chunks; sent ${t.pagesPut} pages and ` +
  `${t.chunksPut} chunks (${mib(t.sent)} MiB) in ${t.commits} commits`;
