// The `ssh()` layer, driven as the runtime drives it but without a zen
// server: a Node TCP server prepares each step from the input, opens it,
// runs the handler on the step's view and sends what the layer finished,
// with OpenSSH's `ssh` as the client. Checks the room and `again` rules:
// a step's output fits `out_window`, and a step whose output was cut
// short, or that made room for held input, runs again without new input.
import { createServer, type Server, type Socket } from 'node:net';
import type { SvcInfo } from '@zen/wasm';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { runSsh, type TestSshKeys, testSshKeys } from '../../client/src/testing/index.js';
import type { Finished } from '../src/layer.js';
import { type SshStep, ssh } from '../src/ssh.js';
import { loadSvcWasm } from '../src/wasm.js';

let keys: TestSshKeys;
/** The user key's fingerprint. */
let user: string;
beforeAll(async () => {
  keys = testSshKeys();
  user = (await loadSvcWasm()).sshPublicKey(keys.userLine).fingerprint;
});
afterAll(() => keys?.cleanup());

const MIN_WINDOW = 65_536;
const text = new TextDecoder();

interface Stats {
  steps: number;
  again: number;
  maxOut: number;
  handlerCalls: number;
  questions: number;
}

type Handler = (c: SshStep) => void | Promise<void>;

/** Serve each connection through the layer: one step at a time, as `serve()` does. */
async function layerServer(
  handler: Handler,
  outWindow = MIN_WINDOW,
): Promise<{ port: number; stats: Stats; close(): void }> {
  const info = { resume: true, window: 1 << 20, out_window: outWindow } as unknown as SvcInfo;
  const engine = await ssh({ hostKey: keys.hostKey }).open(info);
  const stats: Stats = { steps: 0, again: 0, maxOut: 0, handlerCalls: 0, questions: 0 };
  const srv: Server = createServer((sock: Socket) => {
    const lc = engine.conn();
    lc.load(null, true);
    let pending: Uint8Array = new Uint8Array(0);
    let eof = false;
    let app: Uint8Array | null = null;
    let busy = false;
    let more = false;
    let closed = false;
    const run = async () => {
      if (busy) {
        more = true;
        return;
      }
      busy = true;
      try {
        for (let again = true; again && !closed; ) {
          more = false;
          stats.steps++;
          const p = lc.prepare(pending, eof, false);
          const live = lc.open(p, {
            conn: new Uint8Array(16),
            peer: { addr: '127.0.0.1', port: 0 } as never,
            state: app,
            finSent: false,
            outWindow,
          });
          let f: Finished;
          try {
            if (p.call) {
              stats.handlerCalls++;
              if (live.view.question) stats.questions++;
              await handler(live.view);
            }
            f = live.finish();
            app = live.app();
          } finally {
            live.dispose();
          }
          lc.committed(p, f, live);
          stats.maxOut = Math.max(stats.maxOut, f.bytes.length);
          if (f.again) stats.again++;
          pending = pending.subarray(f.inAck);
          if (f.bytes.length) sock.write(f.bytes);
          if (f.endFlag) {
            closed = true;
            sock.end();
          }
          again = f.again || more;
        }
      } finally {
        busy = false;
      }
    };
    sock.on('data', (d: Buffer) => {
      const n = new Uint8Array(pending.length + d.length);
      n.set(pending);
      n.set(d, pending.length);
      pending = n;
      void run();
    });
    sock.on('end', () => {
      eof = true;
      void run();
    });
    sock.on('error', () => {});
    void run();
  });
  await new Promise<void>((r) => srv.listen(0, '127.0.0.1', r));
  const addr = srv.address();
  if (!addr || typeof addr === 'string') throw new Error('no address');
  return {
    port: addr.port,
    stats,
    close: () => {
      srv.close();
      engine.close();
    },
  };
}

/** Per channel: its command and, for `big`, the bytes sent. */
type Chans = Record<number, { cmd: string; sent: number }>;

/**
 * `cat` (stdin to stdout), `big <n>` (n bytes of a pattern) and `who`
 * (the user). Commands are kept in the state: the engine gives them once.
 */
const app =
  (authorized: string): Handler =>
  (c) => {
    if (c.question) c.answer(c.question.key.fingerprint === authorized);
    const chans: Chans = c.state ? JSON.parse(text.decode(c.state)) : {};
    for (const e of c.events) if (e.type === 'exec') chans[e.ch] = { cmd: e.command, sent: 0 };
    for (const ch of c.channels) {
      const st = chans[ch.id];
      if (!st || ch.closed) continue;
      const done = (code: number) => {
        ch.exit(code);
        ch.close();
        delete chans[ch.id];
      };
      if (st.cmd === 'cat') {
        const n = ch.write(ch.data);
        ch.ack(n);
        if (ch.eof && ch.data.length === 0) done(0);
      } else if (st.cmd.startsWith('big ')) {
        const total = Number(st.cmd.slice(4));
        while (st.sent < total) {
          const want = Math.min(total - st.sent, 1 << 16);
          const b = new Uint8Array(want);
          for (let i = 0; i < want; i++) b[i] = (st.sent + i) % 251;
          const n = ch.write(b);
          st.sent += n;
          if (n < want) break;
        }
        if (st.sent === total) done(0);
      } else if (st.cmd === 'who') {
        ch.write(`${c.user}\n`);
        done(0);
      } else {
        ch.stderr(`unknown: ${st.cmd}\n`);
        done(127);
      }
    }
    c.state = new TextEncoder().encode(JSON.stringify(chans));
  };

describe('the ssh() layer', () => {
  it('logs in on the handler answer, and runs a command', async () => {
    const s = await layerServer(app(user));
    try {
      const r = await runSsh([...keys.sshArgs(s.port), 'who']);
      expect(r.stderr).toBe('');
      expect(r.code).toBe(0);
      expect(r.stdout.toString()).toBe('test\n');
      expect(s.stats.questions).toBe(2);
      const bad = await runSsh([...keys.sshArgs(s.port), 'nope']);
      expect(bad.code).toBe(127);
      expect(bad.stderr).toBe('unknown: nope\n');
    } finally {
      s.close();
    }
  });

  it('refuses a question the handler leaves unanswered', async () => {
    const s = await layerServer(() => {});
    try {
      const r = await runSsh([...keys.sshArgs(s.port), 'who']);
      expect(r.code).toBe(255);
      expect(r.stderr).toContain('Permission denied (publickey)');
      expect(s.stats.questions).toBe(1);
    } finally {
      s.close();
    }
  });

  it('a big download: each step fits out_window, and cut steps run again', async () => {
    const s = await layerServer(app(user));
    try {
      const n = 3_000_000;
      const r = await runSsh([...keys.sshArgs(s.port), `big ${n}`]);
      expect(r.stderr).toBe('');
      expect(r.code).toBe(0);
      expect(r.stdout.length).toBe(n);
      expect(r.stdout.every((b, i) => b === i % 251)).toBe(true);
      expect(s.stats.maxOut).toBeLessThanOrEqual(MIN_WINDOW);
      // 3 MB in steps of at most 64 KiB: most ran again at once.
      expect(s.stats.again).toBeGreaterThan(n / MIN_WINDOW / 2);
    } finally {
      s.close();
    }
  }, 60_000);

  it('cats 2 MB: held input is read once the handler acks', async () => {
    const s = await layerServer(app(user));
    try {
      const data = new Uint8Array(2 << 20);
      for (let i = 0; i < data.length; i++) data[i] = (i * 13 + (i >> 11)) & 0xff;
      const r = await runSsh(
        [...keys.sshArgs(s.port, { options: ['RekeyLimit=512K'] }), 'cat'],
        data,
      );
      expect(r.stderr).toBe('');
      expect(r.code).toBe(0);
      expect(Buffer.compare(r.stdout, Buffer.from(data))).toBe(0);
      expect(s.stats.maxOut).toBeLessThanOrEqual(MIN_WINDOW);
      expect(s.stats.again).toBeGreaterThan(0);
    } finally {
      s.close();
    }
  }, 60_000);
});
