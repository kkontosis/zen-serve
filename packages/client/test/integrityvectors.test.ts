// Milestone 6 integrity formats through wasm, against
// spec/test-vectors/integrity.json: device checkpoints (formats.md §14),
// exported pins (§14.5), signed fs headers (§12.1), event chains (§5.1),
// tree states (fs.md §9.1).
import { readFileSync } from 'node:fs';
import { beforeAll, describe, expect, it } from 'vitest';
import { fromHex, hex } from '../src/bytes.js';
import { type DevicePin, decodePins, encodePins } from '../src/integrity.js';
import { initWasm, zw } from '../src/wasm.js';

// biome-ignore lint/suspicious/noExplicitAny: vector JSON
type J = any;
const root = new URL('../../../spec/test-vectors/', import.meta.url);
const V: J = JSON.parse(readFileSync(new URL('integrity.json', root), 'utf8'));
const K: J = JSON.parse(readFileSync(new URL('keys.json', root), 'utf8'));

beforeAll(async () => {
  await initWasm();
});

describe('integrity.json', () => {
  it('a checkpoint opens, verifies, re-encodes and hashes', () => {
    const c = V.checkpoint;
    const keys = zw.FsKeys.fromBundle(fromHex(K.fs_epoch0.bundle));
    const device = fromHex(c.device_fingerprint);
    const devicePublic = fromHex(c.device_public);
    expect(hex(zw.fingerprint(devicePublic))).toBe(c.device_fingerprint);
    const body = zw.decodeCheckpoint(fromHex(c.body));
    expect(body.seq).toBe(2n);
    expect(body.objects.map((o) => o.type)).toEqual(['topic', 'tree', 'db', 'vol', 'svc']);
    // Re-encoding, even from shuffled lists, gives the same bytes.
    const shuffled = {
      ...body,
      objects: [...body.objects].reverse(),
      seen: [...body.seen].reverse(),
    };
    expect(hex(zw.encodeCheckpoint(shuffled))).toBe(c.body);
    expect(hex(zw.checkpointHash(fromHex(c.body)))).toBe(c.hash);

    const signed = keys.openCheckpoint(device, body.seq, fromHex(c.sealed));
    expect(hex(signed)).toBe(c.signed);
    expect(() => keys.openCheckpoint(device, body.seq + 1n, fromHex(c.sealed))).toThrow();
    expect(zw.verifyCheckpoint(devicePublic, signed)).toEqual(body);
    expect(hex(zw.checkpointBodyBytes(signed))).toBe(c.body);
    const flipped = signed.slice();
    flipped[8]! ^= 1;
    expect(() => zw.verifyCheckpoint(devicePublic, flipped)).toThrow();

    // A device signs and seals its own checkpoint; it round-trips.
    const dev = zw.DeviceSecret.fromSeed(new Uint8Array(32).fill(5));
    const own = { ...body, device: dev.fingerprint, seq: 1n, prev: new Uint8Array(32) };
    const s = dev.signCheckpoint(own);
    const sealed = keys.sealCheckpoint(dev.fingerprint, 1n, s);
    const back = zw.verifyCheckpoint(
      dev.devicePublic,
      keys.openCheckpoint(dev.fingerprint, 1n, sealed),
    );
    expect(back).toEqual(zw.decodeCheckpoint(zw.encodeCheckpoint(own)));
    // The body must name the signing device.
    expect(() => dev.signCheckpoint(body)).toThrow();
  });

  it('signed headers chain and verify', () => {
    const sh = V.signed_header;
    const admin = fromHex(sh.user_public);
    const plain = fromHex(sh.header);
    const v1 = fromHex(sh.v1.signed);
    const v2 = fromHex(sh.v2.signed);
    expect(zw.isSignedHeader(v1)).toBe(true);
    expect(zw.isSignedHeader(plain)).toBe(false);
    const d1 = zw.decodeSignedHeader(v1);
    expect([d1.fs, d1.version, hex(d1.hash), hex(d1.header)]).toEqual([
      7,
      1n,
      sh.v1.hash,
      sh.header,
    ]);
    zw.verifySignedHeader(v1, admin);
    zw.validateHeaderSuccessor(7, plain, v1, [admin]);
    zw.validateHeaderSuccessor(7, v1, v2, [admin]);
    expect(() => zw.validateHeaderSuccessor(7, v1, plain, [admin])).toThrow();
    expect(() => zw.validateHeaderSuccessor(7, v2, v2, [admin])).toThrow();
    expect(() => zw.validateHeaderSuccessor(7, undefined, v1, [])).toThrow();
    // Signing again gives the same bytes (deterministic hybrid signatures).
    const user = zw.SigningIdentity.fromSeed(fromHex(signatureUserSeed()));
    expect(hex(user.signHeader(7, 2n, d1.hash, plain))).toBe(sh.v2.signed);
  });

  it('event chain bodies link by HE', () => {
    let prev: Uint8Array = new Uint8Array(32);
    for (const e of V.event_chain.events) {
      const body = zw.decodeEventBody(fromHex(e.body));
      expect(hex(body.chainId!)).toBe(V.event_chain.chain_id);
      expect(body.dseq).toBe(BigInt(e.dseq));
      expect(hex(body.dprev!)).toBe(hex(prev));
      expect(hex(zw.encodeEventBody(body))).toBe(e.body);
      prev = zw.eventChainHash(body);
      expect(hex(prev)).toBe(e.hash);
    }
    // A body with part of a chain is refused.
    const partial = {
      ...zw.decodeEventBody(fromHex(V.event_chain.events[0].body)),
      dseq: undefined,
    };
    expect(() => zw.encodeEventBody(partial)).toThrow();
  });

  it('tree states, incrementally', () => {
    const ts = V.tree_state;
    const nodes = ts.nodes.map((n: J) => ({
      node: fromHex(n.node),
      parent: n.parent === null ? undefined : fromHex(n.parent),
      moveHlc: BigInt(n.move_hlc),
      moveDevice: fromHex(n.move_device),
      meta: n.meta === null ? undefined : fromHex(n.meta),
      metaHlc: n.meta_hlc === null ? undefined : BigInt(n.meta_hlc),
      metaDevice: n.meta_device === null ? undefined : fromHex(n.meta_device),
      versions: n.versions,
      changed: fromHex(n.changed),
    }));
    ts.nodes.forEach((n: J, i: number) => {
      expect(hex(zw.stateLeaf(nodes[i]))).toBe(n.leaf);
    });
    const t = new zw.TreeState();
    expect(hex(t.root())).toBe(ts.empty);
    t.set(nodes[0]);
    expect(hex(t.root())).toBe(ts.first);
    t.set(nodes[1]);
    t.set(nodes[2]);
    expect([t.size, hex(t.root())]).toEqual([3, ts.all]);
    t.remove(nodes[1].node);
    expect(hex(t.root())).toBe(ts.without_second);
    t.free();
  });
  it('exported pins encode and decode (formats.md §14.5)', () => {
    const p = V.pins;
    const head = (h: J) => ({ version: BigInt(h.version), hash: fromHex(h.hash) });
    const devices: [Uint8Array, DevicePin][] = p.devices.map((d: J) => [
      fromHex(d.device),
      { seq: BigInt(d.seq), hash: fromHex(d.hash) } as DevicePin,
    ]);
    expect(hex(encodePins(p.fs, head(p.acl), head(p.header), devices))).toBe(p.bytes);
    const d = decodePins(fromHex(p.bytes));
    expect(d.fs).toBe(p.fs);
    expect([d.acl.version, hex(d.acl.hash)]).toEqual([BigInt(p.acl.version), p.acl.hash]);
    expect([d.header.version, hex(d.header.hash)]).toEqual([
      BigInt(p.header.version),
      p.header.hash,
    ]);
    expect(d.devices.map((x) => [hex(x.device), x.seq, hex(x.hash)])).toEqual(
      p.devices.map((x: J) => [x.device, BigInt(x.seq), x.hash]),
    );
    expect(() => decodePins(fromHex(p.bytes).subarray(1))).toThrow();
  });
});

function signatureUserSeed(): string {
  const S: J = JSON.parse(readFileSync(new URL('signatures.json', root), 'utf8'));
  return S.user.seed;
}
