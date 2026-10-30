// The Rust client (crates/zen-client, its `zen-client` binary) against the
// TS client on one server: values, checkpoints both ways, out-of-band pin
// comparison, and a fork. Build the binary first:
// `cargo build -p zen-client` (CI's client job does).
import { execFile } from 'node:child_process';
import { existsSync } from 'node:fs';
import { writeFile } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { afterEach, describe, expect, it } from 'vitest';
import { fromHex, hex, utf8 } from '../src/bytes.js';
import {
  connect,
  fullGrants,
  type Integrity,
  member,
  type Session,
  type UnlockedFs,
  zw,
} from '../src/index.js';
import type { TestServer } from '../src/testing/index.js';
import { type World, world } from './helpers.js';

const here = dirname(fileURLToPath(import.meta.url));
const BIN = resolve(here, '../../../target/debug/zen-client');

interface Run {
  code: number;
  stdout: string;
  stderr: string;
}

function run(args: string[]): Promise<Run> {
  if (!existsSync(BIN)) {
    throw new Error(`${BIN} is missing: build it with \`cargo build -p zen-client\``);
  }
  return new Promise((ok) => {
    execFile(BIN, args, { timeout: 60_000 }, (err, stdout, stderr) => {
      const code = err ? Number((err as { code?: unknown }).code ?? 1) : 0;
      ok({ code, stdout: stdout.trim(), stderr: stderr.trim() });
    });
  });
}

let servers: TestServer[] = [];

afterEach(async () => {
  for (const s of servers) await s.stop();
  servers = [];
});

interface Env {
  w: World;
  origin: string;
  keyFile: string;
  rustFp: Uint8Array;
  integ: Integrity;
  u: UnlockedFs;
  rust: (url: string, ...args: string[]) => Promise<Run>;
}

/** A server, the TS admin's device, and a Rust device (key file) on signed fs 1. */
async function setup(): Promise<Env> {
  const w = await world();
  servers.push(w.server);
  const origin = `http://localhost:${w.server.port}`;
  const seed = new Uint8Array(32).fill(5);
  const deviceSeed = new Uint8Array(32).fill(5 ^ 0x80);
  const identity = zw.SigningIdentity.fromSeed(seed);
  const device = zw.DeviceSecret.fromSeed(deviceSeed);
  const cert = identity.issueDeviceCert(device.devicePublic, 1_700_000_000n);
  const keyFile = join(w.server.dataDir, 'rust.key');
  await writeFile(
    keyFile,
    JSON.stringify({ identity_seed: hex(seed), device_seed: hex(deviceSeed), cert: hex(cert) }),
    { mode: 0o600 },
  );
  await w.session.acl.update(w.admin.identity, (doc) => {
    doc.members.push(member(identity, [cert]));
    doc.grants.push(...fullGrants(1, identity.fingerprint));
  });
  w.session.useIdentity(w.admin.identity);
  const fs = w.session.fs(1);
  const integ = await fs.integrity({ device: { device: w.admin.device, cert: w.admin.cert } });
  const u = await fs.init(
    (k) => [
      zw.createDeviceSlot(k, w.admin.device.devicePublic),
      zw.createDeviceSlot(k, device.devicePublic),
    ],
    { signed: true },
  );
  const rust = (url: string, ...args: string[]) =>
    run(['--url', url, '--origin', origin, '--device-key', keyFile, '--fs', '1', ...args]);
  return { w, origin, keyFile, rustFp: zw.fingerprint(device.devicePublic), integ, u, rust };
}

const vol = (seq: bigint) => ({
  type: 'vol' as const,
  vol: new Uint8Array(16).fill(7),
  seq,
  hash: new Uint8Array(32).fill(Number(seq)),
});

describe('the Rust client', () => {
  it('reads and writes values the TS client reads and writes', async () => {
    const e = await setup();
    const url = e.w.server.url;
    expect(await e.rust(url, 'kv', 'set', 'a/b', 'from rust')).toMatchObject({ code: 0 });
    const got = await e.u.kv.get(['a', 'b']);
    expect(got && new TextDecoder().decode(got)).toBe('from rust');
    await e.u.kv.set(['c'], utf8('from ts'));
    expect(await e.rust(url, 'kv', 'get', 'c')).toMatchObject({ code: 0, stdout: 'from ts' });
    const h = await e.rust(url, 'header');
    expect(JSON.parse(h.stdout)).toMatchObject({ signed: 1, epoch: 0, slots: 2 });
    expect(e.integ.alarms).toEqual([]);
  });

  it('checkpoints verify both ways, and pins compare', async () => {
    const e = await setup();
    const url = e.w.server.url;
    const ck = await e.rust(url, 'checkpoint');
    expect(ck).toMatchObject({ code: 0, stdout: '1' });
    await e.integ.sync();
    expect(e.integ.deviceHead(e.rustFp)?.seq).toBe(1n);

    e.integ.note(vol(1n));
    expect(await e.integ.checkpoint()).toBe(1n);
    expect(await e.rust(url, 'sync')).toMatchObject({ code: 0 });
    // The Rust device's next body links the TS device's head.
    expect(await e.rust(url, 'checkpoint')).toMatchObject({ code: 0, stdout: '2' });
    await e.integ.sync();
    const rustHead = e.integ.deviceHead(e.rustFp)!;
    expect(rustHead.seq).toBe(2n);
    const own = e.integ.deviceHead(e.w.admin.device.fingerprint)!;
    expect(rustHead.body.seen.map((s) => [hex(s.device), s.seq, hex(s.hash)])).toEqual([
      [hex(e.w.admin.device.fingerprint), own.seq, hex(own.hash)],
    ]);
    expect(rustHead.body.header.version).toBe(1n);

    // TS pins, compared by the Rust client; Rust pins, by the TS client.
    const tsPins = join(e.w.server.dataDir, 'ts.pins');
    await writeFile(tsPins, e.integ.exportPins());
    expect(await e.rust(url, 'pins', 'compare', tsPins)).toMatchObject({ code: 0 });
    const out = await e.rust(url, 'pins', 'export');
    expect(out.code).toBe(0);
    expect(await e.integ.comparePins(fromHex(out.stdout))).toEqual([]);
    expect(e.integ.alarms).toEqual([]);
  });

  it('a fork shows when the Rust client compares pins from the other side', async () => {
    const e = await setup();
    // Both devices checkpoint on the original; the Rust device pins the TS device's head.
    e.integ.note(vol(1n));
    await e.integ.checkpoint();
    expect(await e.rust(e.w.server.url, 'checkpoint')).toMatchObject({ code: 0 });
    const { self, copy } = await e.w.server.fork();
    servers = [self, copy];

    // The TS device goes on writing on the copy.
    const c = await connect(copy.url, { origin: e.origin });
    const s: Session = await c.signInDevice(e.w.admin);
    const fs = s.fs(1);
    const integ = await fs.integrity({
      device: { device: e.w.admin.device, cert: e.w.admin.cert },
    });
    await fs.unlock({ device: e.w.admin.device });
    await integ.sync();
    integ.note(vol(2n));
    expect(await integ.checkpoint()).toBe(2n);
    const pins = join(self.dataDir, 'copy.pins');
    await writeFile(pins, integ.exportPins());

    // On the original, the Rust client sees a head of the TS device its server can't produce.
    const r = await e.rust(self.url, 'pins', 'compare', pins);
    expect(r.code).toBe(3);
    const alarm = JSON.parse(r.stderr.split('\n')[0]!);
    expect(alarm).toMatchObject({ kind: 'fork', object: 'checkpoint' });
    expect(alarm.id).toBe(hex(e.w.admin.device.fingerprint));
  });
});
