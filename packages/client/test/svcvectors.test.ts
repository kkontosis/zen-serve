// Relayed-service seals through wasm, against spec/test-vectors/svc.json
// (formats.md §13): states (kind 9), record key escrows (kind 10), plain
// records, the sealed inbox.
import { readFileSync } from 'node:fs';
import { beforeAll, describe, expect, it } from 'vitest';
import { fromHex, hex } from '../src/bytes.js';
import { initWasm, zw } from '../src/wasm.js';

// biome-ignore lint/suspicious/noExplicitAny: vector JSON
type J = any;
const root = new URL('../../../spec/test-vectors/', import.meta.url);
const V: J = JSON.parse(readFileSync(new URL('svc.json', root), 'utf8'));
const K: J = JSON.parse(readFileSync(new URL('keys.json', root), 'utf8'));
const enc = new TextEncoder();

beforeAll(async () => {
  await initWasm();
});

/** The escrow inside a sealed record: after version, flags and key hash. */
function escrowOf(record: Uint8Array, rawLen: number): Uint8Array {
  return record.subarray(34, record.length - rawLen - 16);
}

describe('svc.json', () => {
  const svcKeys = () => {
    const fs = zw.FsKeys.fromBundle(fromHex(K.fs_epoch0.bundle));
    const topic = fs.topic(V.svc.segments.map((s: string) => enc.encode(s)));
    return fs.svcKeys(topic);
  };
  const conn = fromHex(V.conn);

  it('the service keys: id, and the same keys from parts', () => {
    const k = svcKeys();
    expect(hex(k.id)).toBe(V.svc.id);
    expect(k.epoch).toBe(0);
    const parts = zw.SvcKeys.fromParts(k.fsId, 0, k.id, fromHex(V.svc.k_svc));
    const st = V.state;
    for (const key of [k, parts]) {
      const pt = key.openState(conn, BigInt(st.in_acked), BigInt(st.out_head), fromHex(st.sealed));
      expect(hex(pt)).toBe(st.plaintext);
    }
  });

  it('a state opens only at its cursors', () => {
    const k = svcKeys();
    const sealed = k.sealState(conn, 10n, 20n, enc.encode('s'));
    expect(zw.peekSealed(sealed)).toEqual({ kind: 9, epoch: 0 });
    expect(new TextDecoder().decode(k.openState(conn, 10n, 20n, sealed))).toBe('s');
    expect(() => k.openState(conn, 11n, 20n, sealed)).toThrow('decrypt');
    expect(() => k.openState(conn, 10n, 21n, sealed)).toThrow('decrypt');
  });

  it("the vector record's escrow opens to its key", () => {
    const k = svcKeys();
    for (const name of ['record', 'fin_record']) {
      const r = V[name];
      const raw = fromHex(r.raw);
      const key = k.openKeyEscrow(conn, BigInt(r.off), escrowOf(fromHex(r.record), raw.length));
      expect(hex(key)).toBe(r.k_r);
    }
  });

  it('a record sealed here escrows the key it returns, at its offset only', () => {
    const k = svcKeys();
    const raw = enc.encode('hello\n');
    const { record, key } = k.sealRecord(conn, 7n, 0, raw);
    expect(record.length).toBe(2 + 32 + 80 + raw.length + 16);
    const escrow = escrowOf(record, raw.length);
    expect(zw.peekSealed(escrow)).toEqual({ kind: 10, epoch: 0 });
    expect(hex(k.openKeyEscrow(conn, 7n, escrow))).toBe(hex(key));
    expect(() => k.openKeyEscrow(conn, 8n, escrow)).toThrow('decrypt');
    // An empty record must end the stream.
    expect(() => k.sealRecord(conn, 7n, 0, new Uint8Array())).toThrow('param');
    expect(k.sealRecord(conn, 7n, 1, new Uint8Array()).record[1]).toBe(1);
  });

  it('plain records', () => {
    const p = V.plain_record;
    expect(hex(zw.plainRecord(p.flags, fromHex(p.raw)))).toBe(p.record);
    expect(() => zw.plainRecord(0, new Uint8Array())).toThrow('param');
  });

  it('the inbox: the public key, decapsulation and the chunks', () => {
    const i = V.inbox;
    const k = svcKeys();
    const seed = fromHex(i.seed);
    expect(hex(zw.inboxPublic(seed))).toBe(i.public);
    const key = zw.InboxKey.decaps(seed, fromHex(i.ct), k.fsId, k.id, conn);
    let next = 0n;
    for (const c of i.chunks) {
      const off = BigInt(c.off);
      expect(off).toBe(next);
      const data = key.openChunk(off, fromHex(c.sealed));
      expect(hex(data)).toBe(c.data);
      next = off + BigInt(data.length);
    }
    // Another offset, a flipped bit, another connection: no.
    const c0 = fromHex(i.chunks[0].sealed);
    expect(() => key.openChunk(1n, c0)).toThrow('decrypt');
    c0[0] = c0[0]! ^ 1;
    expect(() => key.openChunk(0n, c0)).toThrow('decrypt');
    const other = zw.InboxKey.decaps(seed, fromHex(i.ct), k.fsId, k.id, new Uint8Array(16));
    expect(() => other.openChunk(0n, fromHex(i.chunks[0].sealed))).toThrow('decrypt');
    expect(() => zw.InboxKey.decaps(seed, fromHex(i.ct).subarray(1), k.fsId, k.id, conn)).toThrow(
      'format',
    );
    // The node's side: what it seals, the handler opens.
    const node = zw.InboxKey.encaps(fromHex(i.public), k.fsId, k.id, conn);
    const sealed = node.sealChunk(5n, enc.encode('hi'));
    expect(sealed.length).toBe(2 + 16);
    const mine = zw.InboxKey.decaps(seed, node.ct!, k.fsId, k.id, conn);
    expect(new TextDecoder().decode(mine.openChunk(5n, sealed))).toBe('hi');
    for (const x of [key, other, node, mine]) x.free();
  });

  it('the svc request codecs round-trip', () => {
    const step = {
      fs: 1,
      svc: fromHex(V.svc.id),
      conn,
      token: 3n,
      in_from: 0n,
      in_to: 5n,
      out_from: 0n,
      out: [zw.plainRecord(0, enc.encode('x'))],
      resumable: true,
    };
    expect(zw.decodeSvcStep(zw.encodeSvcStep(step))).toEqual(step);
    const claims = {
      conns: [
        {
          conn,
          token: 1n,
          peer: { addr: '127.0.0.1:5' },
          accepted_ms: 9n,
          in_acked: 0n,
          in_head: 4n,
          out_head: 0n,
          written_to: 0n,
          flags: 0,
          pending: [{ off: 0n, escrow: new Uint8Array([1]) }],
        },
      ],
    };
    expect(zw.decodeSvcClaims(zw.encodeSvcClaims(claims))).toEqual(claims);
  });
});
