// The echo services as `tcp-auth` (api.md §14.9), reached through
// `zen-connect`: OpenSSH with zen-connect as its ProxyCommand, git over
// it, and `--listen` with Node `net` peers, including a handler worker
// SIGKILLed mid-stream.
import { type ChildProcess, execFile, execFileSync, spawn } from 'node:child_process';
import { type Socket, connect as tcp } from 'node:net';
import { join } from 'node:path';
import { promisify } from 'node:util';
import type { Db } from '@zen/db';
import { serve } from '@zen/svc';
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import {
  runSsh,
  type TestSshKeys,
  testSshKeys,
} from '../../../packages/client/src/testing/index.js';
import {
  ROOT_DIR,
  sleep,
  spawnWorker,
  startWorld,
  until,
  type WorkerProcess,
  type World,
  workerEnv,
} from '../../shared/harness.js';
import { signInWorker, type WorkerContext } from '../../shared/node.js';
import {
  AUTH_SERVICE,
  DB_OPTIONS,
  lines,
  NS,
  putHostKey,
  putSshKey,
  SSH_AUTH_SERVICE,
  STUB_COMMIT,
  STUB_REFS,
  serveEcho,
  sshEcho,
  sshLayer,
} from '../src/echo.js';
import { GRANTS, HANDLER_ONLY } from './grants.js';

const CONNECT = join(ROOT_DIR, 'packages/svc/dist/zen-connect.mjs');

let w: World;
let alice: WorkerContext;
let db: Db;
let keys: TestSshKeys;
const run = promisify(execFile);

beforeAll(async () => {
  for (const script of [['scripts/build-apps.mjs', 'echo'], ['scripts/build-zen-connect.mjs']]) {
    execFileSync(process.execPath, script, { cwd: ROOT_DIR, stdio: 'inherit' });
  }
  w = await startWorld({
    grants: GRANTS,
    // Bob serves, but may not connect.
    grantsOf: { bob: HANDLER_ONLY },
    services: [
      { name: 'echo-auth', path: AUTH_SERVICE, mode: 'tcp-auth' },
      // 10 channels' windows and 64 KiB fit the input window (svc.md §7.2).
      {
        name: 'echo-ssh-auth',
        path: SSH_AUTH_SERVICE,
        mode: 'tcp-auth',
        more: { window: 1_048_576 },
      },
    ],
  });
  alice = await signInWorker({
    url: w.url,
    device: w.user('alice').deviceFile,
    passphrase: w.passphrase,
  });
  db = await alice.open(NS, { ...DB_OPTIONS, warn: () => {} });
  keys = testSshKeys();
  await putHostKey(db, keys.hostKey, keys.hostLine);
  await putSshKey(db, 'test', keys.userLine);
});
afterAll(async () => {
  await alice?.close();
  await w?.stop();
  keys?.cleanup();
});

const errors: unknown[] = [];
const onError = (e: unknown) => errors.push(e);
afterEach(() => {
  expect(errors).toEqual([]);
  errors.length = 0;
});

/** zen-connect's arguments for `service`, signed in as `user` by device key. */
function connectArgs(service: string, user = 'alice'): string[] {
  return [
    CONNECT,
    '--url',
    w.url,
    '--origin',
    w.origin,
    '--svc',
    w.serviceId(service),
    '--device-key',
    w.user(user).deviceFile,
  ];
}

/** `ssh` options that reach `echo-ssh-auth` through zen-connect. */
function sshArgs(user = 'alice'): string[] {
  const proxy = [process.execPath, ...connectArgs('echo-ssh-auth', user)].join(' ');
  // The port is never used: the ProxyCommand is the connection.
  return keys.sshArgs(22, { options: [`ProxyCommand=${proxy}`] });
}

/** Serve `echo-ssh-auth` in this process while `fn` runs. */
async function servingSsh(fn: () => Promise<void>): Promise<void> {
  const h = serve(db, {
    service: SSH_AUTH_SERVICE,
    layer: await sshLayer(db),
    handler: sshEcho,
    onError,
  });
  try {
    await fn();
    await until(() => h.conns === 0, 10_000, 'the connections to be forgotten');
  } finally {
    await h.stop();
  }
}

/** zen-connect `--listen` on a free port, until `stop()`. */
async function listener(
  service: string,
): Promise<{ port: number; p: ChildProcess; stderr(): string; stop(): Promise<void> }> {
  const p = spawn(process.execPath, [...connectArgs(service), '--listen', '127.0.0.1:0'], {
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  let err = '';
  p.stderr!.on('data', (d: Buffer) => {
    err += d.toString();
  });
  await until(() => /listening on /.test(err) || p.exitCode !== null, 30_000, 'zen-connect');
  const m = /listening on 127\.0\.0\.1:(\d+)/.exec(err);
  if (!m) throw new Error(`zen-connect did not start: ${err}`);
  return {
    port: Number(m[1]),
    p,
    stderr: () => err,
    stop: async () => {
      if (p.exitCode === null) {
        p.kill('SIGTERM');
        await new Promise((r) => p.once('exit', r));
      }
    },
  };
}

/** A local peer of a zen-connect listener. */
interface Peer {
  s: Socket;
  got(): string;
  ended: Promise<void>;
}

async function peer(port: number): Promise<Peer> {
  const s = tcp(port, '127.0.0.1');
  await new Promise<void>((ok, fail) => s.once('connect', ok).once('error', fail));
  let buf = '';
  s.on('data', (d) => {
    buf += d.toString();
  });
  s.on('error', () => {});
  return { s, got: () => buf, ended: new Promise<void>((ok) => s.once('end', ok)) };
}

describe('echo over tcp-auth', () => {
  it('ssh through zen-connect as its ProxyCommand: lines, rows and exit statuses', async () => {
    await servingSsh(async () => {
      const before = new Set((await lines(db)).map((r) => r.conn));
      const r = await runSsh([...sshArgs(), 'lines'], 'one\ntwo\n');
      expect(r.stderr).toBe('');
      expect(r.code).toBe(0);
      expect(r.stdout.toString()).toBe('1: one\n2: two\nbye\n');
      const rows = (await lines(db)).filter((l) => !before.has(l.conn));
      expect(rows.map((l) => [l.n, l.line])).toEqual([
        [1, 'one'],
        [2, 'two'],
      ]);
      const e = await runSsh([...sshArgs(), 'exit 3']);
      expect(e.code).toBe(3);
      expect(e.stderr).toBe('bye\n');
    });
  });

  it('a member without append is refused: ssh fails, zen-connect says forbidden', async () => {
    await servingSsh(async () => {
      const r = await runSsh([...sshArgs('bob'), 'lines']);
      expect(r.code).toBe(255);
      expect(r.stderr).toContain('zen-connect: forbidden');
    });
  });

  it('git ls-remote through the ProxyCommand lists the stub repository', async () => {
    await servingSsh(async () => {
      // git adds `<host> <command>` to GIT_SSH_COMMAND; each option is quoted.
      const opts = sshArgs().slice(0, -1);
      opts.splice(opts.indexOf('-p'), 2);
      const quoted = opts.map((o) => `'${o.replaceAll("'", `'\\''`)}'`).join(' ');
      // Not execFileSync: the handler runs in this process.
      const { stdout } = await run('git', ['ls-remote', 'ssh://test@zen-test/repo.git'], {
        env: { ...process.env, GIT_SSH_COMMAND: `ssh ${quoted}`, GIT_SSH_VARIANT: 'ssh' },
        encoding: 'utf8',
        timeout: 60_000,
      });
      expect(stdout).toBe(STUB_REFS.map((r) => `${STUB_COMMIT}\t${r}\n`).join(''));
    });
  });

  it('--listen: local peers get the echo, then bye and EOF after their own end', async () => {
    const h = serveEcho(db, { service: AUTH_SERVICE, onError });
    const l = await listener('echo-auth');
    try {
      const [p, q] = await Promise.all([peer(l.port), peer(l.port)]);
      p.s.write('a\nb');
      q.s.write('x\n');
      await until(() => p.got() === '1: a\n' && q.got() === '1: x\n', 15_000, 'the replies');
      p.s.write('c\n');
      await until(() => p.got() === '1: a\n2: bc\n', 15_000, 'the second reply');
      p.s.end();
      q.s.end();
      await Promise.all([p.ended, q.ended]);
      expect(p.got()).toBe('1: a\n2: bc\nbye\n');
      expect(q.got()).toBe('1: x\nbye\n');
      await until(() => h.conns === 0, 10_000, 'the connections to be forgotten');
      // Nothing but the listening line.
      expect(l.stderr().trim().split('\n')).toHaveLength(1);
    } finally {
      await l.stop();
      await h.stop();
    }
  });

  it('--listen: a worker SIGKILLed mid-stream; the next continues, every line once and in order', async () => {
    const N = 200;
    const env = workerEnv(w, 'alice', { ECHO_LEASE_TTL_MS: '1000', ECHO_LAYER: 'auth' });
    const started: WorkerProcess[] = [];
    const start = async () => {
      const k = spawnWorker('echo', 'handler', env);
      started.push(k);
      await k.waitFor('handler ready');
      return k;
    };
    const before = new Set((await lines(db)).map((r) => r.conn));
    const l = await listener('echo-auth');
    try {
      let worker = await start();
      const p = await peer(l.port);
      const replies = () => p.got().split('\n').filter(Boolean).length;
      const sending = (async () => {
        for (let i = 1; i <= N; i++) {
          p.s.write(`l${i}\n`);
          await sleep(5);
        }
      })();
      await until(() => replies() >= 80, 60_000, '80 replies');
      await worker.kill();
      worker = await start();
      await sending;
      await until(() => replies() >= N, 60_000, `all ${N} replies`);
      const want = Array.from({ length: N }, (_, i) => `${i + 1}: l${i + 1}\n`).join('');
      expect(p.got()).toBe(want);
      p.s.end();
      await p.ended;
      expect(p.got()).toBe(`${want}bye\n`);
      const mine = (await lines(db)).filter((r) => !before.has(r.conn)).sort((a, b) => a.n - b.n);
      expect(new Set(mine.map((r) => r.conn)).size).toBe(1);
      expect(mine.map((r) => [r.n, r.line])).toEqual(
        Array.from({ length: N }, (_, i) => [i + 1, `l${i + 1}`]),
      );
    } catch (e) {
      const logs = started.map((k, i) => `worker ${i}:\n${k.output.join('\n')}`).join('\n');
      throw new Error(`${String(e)}\nzen-connect: ${l.stderr()}\n${logs}`);
    } finally {
      await l.stop();
      for (const k of started) await k.kill();
    }
  }, 120_000);
});
