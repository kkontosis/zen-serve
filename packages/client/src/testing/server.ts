// A throwaway zen-serve for tests: the debug binary on the embedded backend,
// in a temporary data directory, on a free localhost port.
import { type ChildProcess, spawn } from 'node:child_process';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

/** Options of a test server. */
export interface ServerOptions {
  /** Extra `[auth]` settings (TOML values, e.g. `{opaque: true}`). */
  auth?: Record<string, string | number | boolean | string[]>;
  /** Extra `[limits]` settings. */
  limits?: Record<string, number>;
  /** Extra top-level settings (before any table). */
  top?: Record<string, string | number | boolean | string[]>;
  /** Raw TOML appended at the end (e.g. a `[tls]` table). */
  extra?: string;
  /** fs ids to configure (default 1 and 2). */
  fs?: number[];
  /** Serve TLS: the URL becomes https (needs a `[tls]` table in `extra`). */
  https?: boolean;
  /** `fetch` init for the readiness probe (an undici dispatcher trusting the test CA). */
  probeInit?: Record<string, unknown>;
  /** A fixed port (default: a free one). */
  port?: number;
}

/** A running test server. */
export interface TestServer {
  url: string;
  /** The origin a client signs (`scheme://127.0.0.1:port`). */
  origin: string;
  port: number;
  dataDir: string;
  /** The claim token, until the server is claimed. */
  claimToken: string;
  /** Stop the process and delete its data directory. */
  stop(): Promise<void>;
  /** Stop the process, keeping the data (for a restart). */
  kill(): Promise<void>;
  /** The server's log so far. */
  log(): Promise<string>;
}

function toml(v: string | number | boolean | string[]): string {
  if (Array.isArray(v)) return `[${v.map((s) => JSON.stringify(s)).join(', ')}]`;
  return typeof v === 'string' ? JSON.stringify(v) : String(v);
}

function table(name: string | undefined, kv: Record<string, unknown> | undefined): string {
  if (!kv) return '';
  const lines = Object.entries(kv).map(
    ([k, v]) => `${k} = ${toml(v as string | number | boolean | string[])}`,
  );
  return `${name ? `[${name}]\n` : ''}${lines.join('\n')}\n`;
}

/** A free TCP port on 127.0.0.1. */
export async function freePort(): Promise<number> {
  return new Promise((ok, fail) => {
    const s = createServer();
    s.once('error', fail);
    s.listen(0, '127.0.0.1', () => {
      const port = (s.address() as { port: number }).port;
      s.close(() => ok(port));
    });
  });
}

/** The zen-serve binary: `$ZEN_SERVE_BIN`, or the workspace's debug build. */
export function serverBinary(): string {
  if (process.env.ZEN_SERVE_BIN) return process.env.ZEN_SERVE_BIN;
  const here = dirname(fileURLToPath(import.meta.url));
  return resolve(here, '../../../../target/debug/zen-serve');
}

/** Start zen-serve and wait until it answers `/v1/info`. */
export async function spawnServer(opts: ServerOptions = {}): Promise<TestServer> {
  const dataDir = await mkdtemp(join(tmpdir(), 'zen-test-'));
  return startIn(dataDir, opts);
}

/** Start zen-serve on an existing data directory. */
export async function startIn(dataDir: string, opts: ServerOptions = {}): Promise<TestServer> {
  const port = opts.port ?? (await freePort());
  const scheme = opts.https ? 'https' : 'http';
  const fsTables = (opts.fs ?? [1, 2]).map((id) => `[[fs]]\nid = ${id}\n`).join('\n');
  const config = [
    `data_dir = ${JSON.stringify(join(dataDir, 'data'))}`,
    `listen = "127.0.0.1:${port}"`,
    table(undefined, opts.top),
    fsTables,
    table('auth', {
      password_m_cost_kib: 65536,
      password_t_cost: 1,
      password_p_cost: 1,
      ...opts.auth,
    }),
    table('limits', opts.limits),
    opts.extra ?? '',
  ].join('\n');
  const cfgPath = join(dataDir, 'zen.toml');
  await writeFile(cfgPath, config);
  const logPath = join(dataDir, 'server.log');
  const { openSync, closeSync } = await import('node:fs');
  const fd = openSync(logPath, 'a');
  const child: ChildProcess = spawn(serverBinary(), ['serve', '-c', cfgPath], {
    stdio: ['ignore', fd, fd],
    env: { ...process.env, RUST_LOG: process.env.RUST_LOG ?? 'warn' },
  });
  closeSync(fd);
  const url = `${scheme}://127.0.0.1:${port}`;
  let exited = false;
  child.once('exit', () => {
    exited = true;
  });
  const log = () => readFile(logPath, 'utf8').catch(() => '');
  const kill = async () => {
    if (exited) return;
    const done = new Promise((r) => child.once('exit', r));
    child.kill('SIGTERM');
    const t = setTimeout(() => child.kill('SIGKILL'), 5000);
    await done;
    clearTimeout(t);
  };
  const deadline = Date.now() + 30_000;
  for (;;) {
    if (exited) throw new Error(`zen-serve exited during start-up:\n${await log()}`);
    try {
      const r = await fetch(`${url}/v1/info`, opts.probeInit as RequestInit | undefined);
      if (r.ok) break;
    } catch {
      // not up yet
    }
    if (Date.now() > deadline) {
      await kill();
      throw new Error(`zen-serve didn't start:\n${await log()}`);
    }
    await new Promise((r) => setTimeout(r, 50));
  }
  const claimToken = (
    await readFile(join(dataDir, 'data', 'claim-token'), 'utf8').catch(() => '')
  ).trim();
  return {
    url,
    origin: url,
    port,
    dataDir,
    claimToken,
    kill,
    log,
    async stop() {
      await kill();
      await rm(dataDir, { recursive: true, force: true });
    },
  };
}
