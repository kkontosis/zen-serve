// The replica's queued uploads under an upload session (fs.md §11, api.md
// §12.1): a queued write whose content takes several commits keeps what
// the early ones put past the grace period, until the write lands.
import {
  bytes,
  memoryLocalStore,
  ROOT,
  type SealedStore,
  type Session,
  sealedStore,
  Tree,
  type UnlockedFs,
  ZenError,
  zw,
} from '@zen/client';
import type { CdcParams } from '@zen/wasm';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type World, world } from '../../client/test/helpers.js';
import { openReplica, type ReplicaOptions } from '../src/index.js';
import { installation } from './helpers.js';

let w: World;
let admin: UnlockedFs;
beforeAll(async () => {
  // Content commits of at most 60 KB, a grace of 1 s, swept every second.
  w = await world({
    limits: {
      max_value_bytes: 8192,
      max_commit_bytes: 60_000,
      chunk_grace_secs: 1,
      sweep_interval_secs: 1,
    },
  });
  admin = await w.session.fs(1).init((k) => [zw.createDeviceSlot(k, w.admin.device.devicePublic)]);
});
afterAll(() => w?.server.stop());

const CDC: CdcParams = { min: 256, avgBits: 10, max: 8128 };
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const random = (n: number) => {
  const b = new Uint8Array(n);
  for (let i = 0; i < n; i += 65_536) crypto.getRandomValues(b.subarray(i, i + 65_536));
  return b;
};
const solo = (local: SealedStore): ReplicaOptions => ({
  local,
  locks: null,
  channel: null,
  waitMs: 200,
  cdc: CDC,
});

/**
 * Delay a session's commits: once a commit that only puts content has
 * applied, the next commit waits `ms` (past the grace, with sweeps
 * running). Counts content commits and those naming a session.
 */
function slow(s: Session, ms: number) {
  const orig = s.call.bind(s);
  const st = { content: 0, underSession: 0, delayed: 0, armed: true };
  let delayNext = false;
  s.call = (async (path: string, enc: never, dec: never, req: never) => {
    if (path === '/v1/commit') {
      const c = req as { crdt_ops?: unknown[]; chunks?: unknown[]; upload?: unknown };
      if (delayNext && st.armed) {
        delayNext = false;
        st.delayed++;
        await sleep(ms);
      }
      const r = await orig(path, enc, dec, req);
      if (!c.crdt_ops?.length && c.chunks?.length) {
        st.content++;
        if (c.upload) st.underSession++;
        if (st.content === 1) delayNext = true;
      }
      return r;
    }
    return orig(path, enc, dec, req);
  }) as Session['call'];
  return st;
}

async function setup() {
  const id = Tree.newId();
  await admin.tree(id).mkdir(ROOT, '.keep');
  const inst = await installation(w);
  const local = sealedStore(memoryLocalStore());
  const r = await openReplica(inst.ufs, id, solo(local));
  await r.sync();
  return { id, inst, local, r };
}

describe('queued uploads under a session', () => {
  it('content put by early commits stays past the grace until the write lands', async () => {
    const { id, inst, r } = await setup();
    const st = slow(inst.ufs.session, 3000);
    const data = random(600_000);
    await r.writeFile('/big', data);
    await r.flush();
    expect(st.delayed).toBe(1);
    expect(st.content).toBeGreaterThan(2);
    // The write landed: another replica reads it.
    const first = await openReplica(admin, id, solo(sealedStore(memoryLocalStore())));
    await first.sync();
    expect(bytes.equal(await first.readFile('/big'), data)).toBe(true);
    await first.close();
    // An edit of the same file, queued and sent the same way.
    st.armed = false;
    const edited = data.slice();
    edited[300_000] = edited[300_000]! ^ 0xff;
    await r.writeFile('/big', edited);
    await r.flush();
    // After the grace, both read back elsewhere, and no session is left.
    await sleep(2500);
    const view = await openReplica(admin, id, solo(sealedStore(memoryLocalStore())));
    await view.sync();
    expect(bytes.equal(await view.readFile('/big'), edited)).toBe(true);
    expect((await inst.ufs.tree(id).uploads()).length).toBe(0);
    expect(st.underSession).toBe(st.content);
    await view.close();
    await r.close();
  });

  it('a restart renews the stored session and lands the write', async () => {
    const { id, inst, local, r } = await setup();
    const s = inst.ufs.session;
    const orig = s.call.bind(s);
    let dropped = false;
    // From the first write commit on, nothing goes out (as if the process
    // died just before it).
    s.call = (async (path: string, enc: never, dec: never, req: never) => {
      const c = req as { crdt_ops?: unknown[] };
      if (path === '/v1/commit' && c.crdt_ops?.length) dropped = true;
      if (dropped) throw new ZenError(0, 'network', 'the process died (test)');
      return orig(path, enc, dec, req);
    }) as Session['call'];
    const data = random(400_000);
    await r.writeFile('/f', data);
    for (let i = 0; i < 200 && !dropped; i++) await sleep(25);
    expect(dropped).toBe(true);
    await r.close();
    s.call = orig;
    const open = await inst.ufs.tree(id).uploads();
    expect(open.length).toBe(1);
    await sleep(2500);
    const again = await openReplica(inst.ufs, id, solo(local));
    await again.flush();
    expect(bytes.equal(await again.readFile('/f'), data)).toBe(true);
    expect((await inst.ufs.tree(id).uploads()).length).toBe(0);
    await sleep(2500);
    const view = await openReplica(admin, id, solo(sealedStore(memoryLocalStore())));
    await view.sync();
    expect(bytes.equal(await view.readFile('/f'), data)).toBe(true);
    await view.close();
    await again.close();
  });
});
