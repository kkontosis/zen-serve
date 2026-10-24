// The echo service over SSH (`ssh()`, svc.md §7.2) with OpenSSH's `ssh` as
// the client: handlers in-process and as Node workers killed with
// SIGKILL, mid-transfer (with rekeys) and mid-key-exchange; the session
// goes on with the next handler. And `git ls-remote` against the stub.
import { execFile, spawn } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { createServer, type Server, type Socket, connect as tcp } from 'node:net';
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
  DB_OPTIONS,
  lines,
  NS,
  putHostKey,
  putSshKey,
  SSH_SERVICE,
  STUB_COMMIT,
  STUB_REFS,
  sshEcho,
  sshLayer,
} from '../src/echo.js';
import { GRANTS } from './grants.js';

let w: World;
let alice: WorkerContext;
let db: Db;
let keys: TestSshKeys;
let port: number;
const run = promisify(execFile);

beforeAll(async () => {
  w = await startWorld({
    grants: GRANTS,
    // 10 channels' windows and 64 KiB fit the relay's input window (svc.md §7.2).
    services: [{ name: 'echo-ssh', path: SSH_SERVICE, more: { window: 1_048_576 } }],
  });
  port = w.servicePort('echo-ssh');
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

/** Serve in this process while `fn` runs. */
async function serving(fn: () => Promise<void>): Promise<void> {
  const h = serve(db, {
    service: SSH_SERVICE,
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

/** Workers of the service, killed at the end. */
function workers() {
  const env = workerEnv(w, 'alice', { ECHO_LEASE_TTL_MS: '1000', ECHO_LAYER: 'ssh' });
  const started: WorkerProcess[] = [];
  return {
    started,
    start: async () => {
      const k = spawnWorker('echo', 'handler', env);
      started.push(k);
      await k.waitFor('handler ready');
      return k;
    },
    logs: () => started.map((k, i) => `worker ${i}:\n${k.output.join('\n')}`).join('\n'),
    killAll: async () => {
      for (const k of started) await k.kill();
    },
  };
}

describe('echo over SSH', () => {
  it('lines over stdin: the replies and the rows; exit statuses', async () => {
    await serving(async () => {
      const r = await runSsh([...keys.sshArgs(port), 'lines'], 'one\ntwo\n');
      expect(r.stderr).toBe('');
      expect(r.code).toBe(0);
      expect(r.stdout.toString()).toBe('1: one\n2: two\nbye\n');
      const rows = (await lines(db)).filter((l) => l.line === 'one' || l.line === 'two');
      expect(rows.map((l) => [l.n, l.line])).toEqual([
        [1, 'one'],
        [2, 'two'],
      ]);
      expect(rows[0]!.conn).toMatch(/^[0-9a-f]+\/0$/);
      const e = await runSsh([...keys.sshArgs(port), 'exit 3']);
      expect(e.code).toBe(3);
      expect(e.stderr).toBe('bye\n');
      const u = await runSsh([...keys.sshArgs(port), 'rm -rf /']);
      expect(u.code).toBe(127);
    });
  });

  it('refuses a key that is not in ssh_keys, and a user it is not for', async () => {
    await serving(async () => {
      const other = await runSsh([...keys.sshArgs(port, { keyFile: keys.otherKeyFile }), 'cat']);
      expect(other.code).toBe(255);
      expect(other.stderr).toContain('Permission denied (publickey)');
      const bob = await runSsh([...keys.sshArgs(port, { user: 'bob' }), 'cat']);
      expect(bob.code).toBe(255);
      expect(bob.stderr).toContain('Permission denied (publickey)');
    });
  });

  it('git ls-remote lists the stub repository', async () => {
    await serving(async () => {
      // git adds `-p <port> <host> <command>` to GIT_SSH_COMMAND.
      const opts = keys.sshArgs(port).slice(0, -1);
      opts.splice(opts.indexOf('-p'), 2);
      // Not execFileSync: the handler runs in this process.
      const { stdout: out } = await run(
        'git',
        ['ls-remote', `ssh://test@127.0.0.1:${port}/repo.git`],
        {
          env: { ...process.env, GIT_SSH_COMMAND: `ssh ${opts.join(' ')}`, GIT_SSH_VARIANT: 'ssh' },
          encoding: 'utf8',
          timeout: 60_000,
        },
      );
      expect(out).toBe(STUB_REFS.map((r) => `${STUB_COMMIT}\t${r}\n`).join(''));
    });
  });

  it('50 MB through cat across two kills, with rekeys: the SHA-256 matches', async () => {
    const ws = workers();
    try {
      let worker = await ws.start();
      const N = 50 << 20;
      const data = randomBytes(N);
      const p = spawn('ssh', [...keys.sshArgs(port, { options: ['RekeyLimit=4M'] }), 'cat']);
      const out: Buffer[] = [];
      let back = 0;
      let stderr = '';
      p.stdout.on('data', (d: Buffer) => {
        out.push(d);
        back += d.length;
      });
      p.stderr.on('data', (d: Buffer) => {
        stderr += d.toString();
      });
      p.stdin.on('error', () => {});
      const exited = new Promise<number | null>((r) => p.on('close', r));
      const sending = (async () => {
        for (let at = 0; at < N; at += 1 << 16) {
          if (!p.stdin.write(data.subarray(at, Math.min(at + (1 << 16), N)))) {
            await new Promise((r) => p.stdin.once('drain', r));
          }
        }
        p.stdin.end();
      })();
      try {
        for (const frac of [0.3, 0.65]) {
          await until(() => back >= N * frac, 240_000, `${frac * 100}% back`);
          await worker.kill();
          worker = await ws.start();
        }
        await sending;
        const code = await Promise.race([
          exited,
          sleep(240_000).then(() => {
            throw new Error('ssh did not finish');
          }),
        ]);
        expect(code).toBe(0);
      } catch (e) {
        p.kill('SIGKILL');
        throw new Error(`${String(e)}; ${back} bytes back; ssh: ${stderr}\n${ws.logs()}`);
      }
      expect(stderr).toBe('');
      expect(back).toBe(N);
      const sha = (b: Buffer) => createHash('sha256').update(b).digest('hex');
      expect(sha(Buffer.concat(out))).toBe(sha(data));
    } finally {
      await ws.killAll();
    }
  }, 600_000);

  it('a worker SIGKILLed mid-key-exchange: the next one answers it', async () => {
    const ws = workers();
    const proxy = await kexProxy(port);
    try {
      const first = await ws.start();
      const run = runSsh([...keys.sshArgs(proxy.port), 'lines'], 'after\n', 120_000);
      // The client's KEXINIT has gone through; its key-exchange init is
      // held. The handler commits the KEXINIT in the state, then dies.
      await until(() => proxy.held() > 0, 30_000, "the client's key-exchange init");
      await sleep(1500);
      await first.kill();
      await ws.start();
      proxy.release();
      const r = await run;
      if (r.code !== 0) throw new Error(`ssh: ${r.code} ${r.stderr}\n${ws.logs()}`);
      expect(r.stdout.toString()).toBe('1: after\nbye\n');
      expect(r.stderr).toBe('');
    } finally {
      proxy.close();
      await ws.killAll();
    }
  }, 120_000);
});

/**
 * A TCP proxy to `port` that forwards the client's version line and its
 * first packet (KEXINIT), then holds what the client sends after it,
 * until `release()`.
 */
async function kexProxy(port: number): Promise<{
  port: number;
  held(): number;
  release(): void;
  close(): void;
}> {
  const held: Buffer[] = [];
  let up: Socket | undefined;
  let down: Socket | undefined;
  let released = false;
  let seen = Buffer.alloc(0);
  /** The client bytes to forward before holding (unknown until its KEXINIT's length is in). */
  let pass = -1;
  let sent = 0;
  const server: Server = createServer((c) => {
    down = c;
    up = tcp(port, '127.0.0.1');
    up.on('data', (d) => c.write(d));
    up.on('close', () => c.destroy());
    up.on('error', () => {});
    c.on('data', (d: Buffer) => {
      if (released) {
        up!.write(d);
        return;
      }
      seen = Buffer.concat([seen, d]);
      if (pass < 0) {
        const nl = seen.indexOf('\n');
        if (nl >= 0 && seen.length >= nl + 5) pass = nl + 5 + seen.readUInt32BE(nl + 1);
      }
      const to = pass < 0 ? 0 : Math.min(pass, seen.length);
      if (to > sent) {
        up!.write(seen.subarray(sent, to));
        sent = to;
      }
      if (pass >= 0 && seen.length > pass) {
        held.push(seen.subarray(Math.max(sent, pass)));
        seen = seen.subarray(0, pass);
      }
    });
    c.on('error', () => {});
    c.on('close', () => up?.destroy());
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const addr = server.address();
  if (!addr || typeof addr === 'string') throw new Error('no address');
  return {
    port: addr.port,
    held: () => held.length,
    release: () => {
      released = true;
      for (const d of held) if (up && !up.destroyed) up.write(d);
    },
    close: () => {
      down?.destroy();
      up?.destroy();
      server.close();
    },
  };
}
