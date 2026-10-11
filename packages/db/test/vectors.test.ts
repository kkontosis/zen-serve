// The TypeScript encoders against spec/test-vectors/zendb.json (zendb.md §17).
import { readFileSync } from 'node:fs';
import { bytes, initWasm, zw } from '@zen/client';
import { beforeAll, describe, expect, it } from 'vitest';
import type { TableRecord } from '../src/catalog.js';
import { decode, encode, type Value } from '../src/cbor.js';
import { mergeState, viewOf } from '../src/crdt.js';
import type { Db } from '../src/db.js';
import { encodeRoot } from '../src/index/node.js';
import { type Change, rewrite, treeKeys } from '../src/index/prolly.js';
import { blobParts, encodeBlob, encodeHead } from '../src/index/sealed.js';
import { Keys, u64 } from '../src/keys.js';
import { decodeMsg, encodeBodyRef, encodeMsg, eventId } from '../src/msg.js';
import { cutParts, encodeRow, joinParts, partsDigest, rowBucket } from '../src/row.js';
import { decodeSortKey, type FieldType, sortKey } from '../src/sortkey.js';
import { MemStore } from './memstore.js';

// biome-ignore lint/suspicious/noExplicitAny: vector JSON
type J = any;
const root = new URL('../../../spec/test-vectors/', import.meta.url);
const V: J = JSON.parse(readFileSync(new URL('zendb.json', root), 'utf8'));
const K: J = JSON.parse(readFileSync(new URL('keys.json', root), 'utf8'));
const hex = bytes.hex;
const unhex = bytes.fromHex;

/** A tagged vector value (zen-core `db_vectors::to_json`). */
function val(j: J): Value {
  if (j === null || typeof j === 'boolean') return j;
  if (Array.isArray(j)) return j.map(val);
  if ('int' in j) {
    const i = BigInt(j.int);
    return i >= -(2n ** 53n) && i <= 2n ** 53n ? Number(i) : i;
  }
  if ('float' in j) {
    if (j.float === 'Infinity') return Number.POSITIVE_INFINITY;
    if (j.float === '-Infinity') return Number.NEGATIVE_INFINITY;
    if (j.float === null) return Number.NaN; // JSON has no NaN
    return j.float;
  }
  if ('text' in j) return j.text;
  if ('bytes' in j) return unhex(j.bytes);
  const pairs: [Value, Value][] = j.map.map(([k, v]: [J, J]) => [val(k), val(v)]);
  if (pairs.every(([k]) => typeof k === 'string')) {
    return Object.fromEntries(pairs) as Value;
  }
  return new Map(pairs);
}

beforeAll(async () => {
  await initWasm();
});

describe('deterministic CBOR (§1)', () => {
  it('encodes and decodes every `ok` value', () => {
    for (const c of V.cbor.ok) {
      expect(hex(encode(val(c.value))), JSON.stringify(c.value)).toBe(c.cbor);
      expect(decode(unhex(c.cbor))).toEqual(val(c.decoded));
    }
  });

  it('refuses the `refused_encode` values', () => {
    for (const c of V.cbor.refused_encode) {
      // JavaScript maps can't hold a duplicate key.
      if (c.why === 'duplicate key') continue;
      expect(() => encode(val(c.value)), c.why).toThrow();
    }
  });

  it('refuses the `refused_decode` bytes', () => {
    for (const c of V.cbor.refused_decode) {
      expect(() => decode(unhex(c.cbor)), c.why).toThrow();
    }
  });

  it('refuses what JavaScript adds: undefined, class instances', () => {
    expect(() => encode(undefined as unknown as Value)).toThrow();
    expect(() => encode([undefined] as unknown as Value)).toThrow();
    expect(() => encode(new Date() as unknown as Value)).toThrow();
    expect(hex(encode({ a: 1, b: undefined }))).toBe(hex(encode({ a: 1 })));
  });
});

describe('sort keys (§5.1)', () => {
  const fields = (fs: J[]) =>
    fs.map((f) => ({ value: val(f.value), type: f.type as FieldType, desc: f.desc }));

  it('matches every case', () => {
    for (const c of V.sort_keys.cases) {
      expect(hex(sortKey(fields(c.fields), c.pk.map(val))), JSON.stringify(c)).toBe(c.key);
    }
  });

  it('decode back to their values and pk', () => {
    for (const c of V.sort_keys.cases) {
      const fs = fields(c.fields);
      const got = decodeSortKey(fs, unhex(c.key));
      const want = (v: Value, ty?: FieldType) =>
        ty === 'float' && typeof v === 'bigint' ? Number(v) : v;
      expect(got.values).toEqual(fs.map((f) => want(f.value ?? null, f.type)));
      expect(got.pk).toEqual(c.pk.map(val));
    }
  });

  it('refuses the `refused` cases', () => {
    for (const c of V.sort_keys.refused) {
      expect(() => sortKey(fields(c.fields), c.pk.map(val)), c.why).toThrow();
    }
  });
});

describe('rows (§4)', () => {
  const r = () => V.rows;

  it('a plain row', () => {
    const p = r().plain;
    expect(hex(encode(val(p.pk)))).toBe(p.pk_element);
    expect(
      hex(encodeRow(val(p.pk), val(p.fields) as Record<string, Value>, undefined, false)),
    ).toBe(p.row);
  });

  it('padded rows', () => {
    for (const p of r().padded) {
      const row = encodeRow(
        'u1',
        { id: 'u1', blob: new Uint8Array(p.blob_len).fill(7) },
        undefined,
        true,
      );
      expect(row.length).toBe(p.row_len);
      if (p.row) expect(hex(row)).toBe(p.row);
      else expect(hex(partsDigest(row))).toBe(p.row_digest);
    }
  });

  it('a row in parts', () => {
    const p = r().parts;
    const f = encode(val(p.fields));
    expect(hex(f)).toBe(p.fields_cbor);
    const parts = cutParts(f, p.part_size, false);
    expect(parts.map(hex)).toEqual(p.parts);
    expect(hex(partsDigest(f))).toBe(p.digest);
    expect(hex(encodeRow('u1', {}, { n: parts.length, digest: partsDigest(f) }, false))).toBe(
      p.row,
    );
    expect(joinParts(parts, partsDigest(f))).toEqual(val(p.fields));
    // Padded parts decode alike; a non-zero tail or a wrong digest doesn't.
    const padded = cutParts(f, p.part_size, true);
    expect(joinParts(padded, partsDigest(f))).toEqual(val(p.fields));
    const bad = padded.map((x) => x.slice());
    bad.at(-1)![bad.at(-1)!.length - 1] = 1;
    expect(() => joinParts(bad, partsDigest(f))).toThrow(/non-zero/);
    expect(() => joinParts(parts, new Uint8Array(32))).toThrow(/digest/);
  });

  it('buckets', () => {
    for (const [n, b] of r().buckets) expect(rowBucket(n)).toBe(b);
  });
});

describe('database keys (§2)', () => {
  it('match the fixture database', () => {
    const fsKeys = zw.FsKeys.fromBundle(unhex(K.fs_epoch0.bundle));
    const k = new Keys(fsKeys.db(V.database.ns));
    expect(hex(k.db.prefix)).toBe(V.database.prefix);
    for (const c of V.database.keys) {
      const els = c.elements.map((e: string, i: number) => {
        const m = /^u64\((\d+)\)$/.exec(e);
        if (m) return u64(Number(m[1]));
        return c.elements[0] === 't' && i > 0 ? unhex(e) : e;
      });
      expect(hex(k.key(...els)), JSON.stringify(c.elements)).toBe(c.key);
    }
    k.db.free();
    fsKeys.free();
  });
});

describe('private index (§5.4)', () => {
  const P = () => V.prolly;
  const ik = () =>
    zw.FsKeys.fromBundle(unhex(K.fs_epoch0.bundle)).db(V.database.ns).index(unhex(P().index_id));

  it('boundaries and shards', () => {
    const k = ik();
    for (const b of P().boundaries) {
      expect(k.isBoundary(b.level, unhex(b.key), b.fanout)).toBe(b.boundary);
    }
    const flags = k.boundaries(
      0,
      P()
        .boundaries.filter((b: J) => b.level === 0)
        .map((b: J) => unhex(b.key)),
      4,
    );
    expect([...flags].map((x) => x === 1)).toEqual(
      P()
        .boundaries.filter((b: J) => b.level === 0)
        .map((b: J) => b.boundary),
    );
    for (const s of P().shards) expect(k.shard(unhex(s.pk_element), s.shards)).toBe(s.shard);
  });

  it('a build from empty, then a change, give the vector trees', async () => {
    const tk = treeKeys(ik(), 4);
    const store = new MemStore();
    const entries: Uint8Array[] = P().entries.map(unhex);
    const ids = (t: J) => new Set<string>(t.nodes.map((n: J) => n.id));
    const r1 = await rewrite(
      tk,
      store,
      undefined,
      entries.map((key) => ({ key, add: true })),
    );
    store.apply(r1);
    expect(hex(encodeRoot(r1.root!))).toBe(P().tree.root.record);
    expect(new Map([...store.m].map(([id, b]) => [id, hex(b)]))).toEqual(
      new Map(P().tree.nodes.map((n: J) => [n.id, n.node])),
    );
    const changes: Change[] = [
      ...P().deleted.map((i: number) => ({ key: entries[i]!, add: false })),
      ...P().inserted.map((h: string) => ({ key: unhex(h), add: true })),
    ];
    const r2 = await rewrite(tk, store, r1.root, changes);
    expect(hex(encodeRoot(r2.root!))).toBe(P().after.root.record);
    const before = ids(P().tree);
    const after = ids(P().after);
    expect(new Set(r2.put.map((w) => hex(w.id)))).toEqual(
      new Set([...after].filter((x) => !before.has(x))),
    );
    expect(new Set(r2.del.map(hex))).toEqual(new Set([...before].filter((x) => !after.has(x))));
    store.apply(r2);
    expect(new Map([...store.m].map(([id, b]) => [id, hex(b)]))).toEqual(
      new Map(P().after.nodes.map((n: J) => [n.id, n.node])),
    );
  });
});

describe('sealed index (§5.7)', () => {
  it('blob, parts, head and keys', () => {
    const S = V.sealed_index;
    const entries: Uint8Array[] = V.prolly.entries.slice(0, 12).map(unhex);
    const blob = encodeBlob(entries);
    expect(hex(blob)).toBe(S.blob);
    expect(blobParts(blob, S.part_size).map(hex)).toEqual(S.parts);
    const head = encodeHead({ parts: S.parts.length, digest: partsDigest(blob), count: 12 });
    expect(hex(head)).toBe(S.head);
    const fsKeys = zw.FsKeys.fromBundle(unhex(K.fs_epoch0.bundle));
    const k = new Keys(fsKeys.db(V.database.ns));
    const id = unhex(V.prolly.index_id);
    expect(hex(k.sealedHead(id))).toBe(S.head_key);
    expect(hex(k.sealedPart(id, 1))).toBe(S.part_key_1);
  });
});

describe('broker (§10, §11.3)', () => {
  const B = () => V.broker;

  it('Msg', () => {
    const v = val(B().msg.value) as Map<Value, Value>;
    // The vector value encodes as given, and the codec reproduces it.
    expect(hex(encode(v))).toBe(B().msg.cbor);
    const m = decodeMsg(unhex(B().msg.cbor));
    expect(m.type).toBe('order.paid');
    expect(m.replyTo!.slice(0, 2).map(bytes.fromUtf8)).toEqual(['zen', 'inbox']);
    expect(m.body).toEqual({ order: 'o-7', amount: 1250 });
    expect(hex(encodeMsg(m))).toBe(B().msg.cbor);
  });

  it('BodyRef', () => {
    const body = val(B().body_ref.body);
    const want = val(B().body_ref.value) as Map<Value, Value>;
    expect(hex(partsDigest(encode(body)))).toBe(hex(want.get(3) as Uint8Array));
    const ref = { ns: 'app', parts: 2, digest: partsDigest(encode(body)) };
    expect(hex(encodeBodyRef(ref))).toBe(B().body_ref.cbor);
  });

  it('event id and group ids', () => {
    const E = B().event_id;
    expect(hex(eventId(unhex(E.topic_id), unhex(E.offset)))).toBe(E.id);
    const fsKeys = zw.FsKeys.fromBundle(unhex(K.fs_epoch0.bundle));
    const t = fsKeys.topic([bytes.utf8('shop'), bytes.utf8('pay')]);
    expect(hex(t.id)).toBe(E.topic_id);
    for (const g of B().group_ids) expect(hex(t.groupId(g.name)), g.name).toBe(g.id);
    t.free();
    fsKeys.free();
  });
});

describe('local records (kind 8, §9.3)', () => {
  it('open under the fixture fs, bound to the store and the key', () => {
    const fsKeys = zw.FsKeys.fromBundle(unhex(K.fs_epoch0.bundle));
    for (const r of V.local.records as J[]) {
      const sealed = unhex(r.sealed);
      expect(hex(fsKeys.openLocal(r.store, unhex(r.key), sealed))).toBe(r.plaintext);
      expect(() => fsKeys.openLocal(`${r.store}x`, unhex(r.key), sealed)).toThrow();
      // Sealing again gives a fresh nonce, and opens the same.
      const again = fsKeys.sealLocal(r.store, unhex(r.key), unhex(r.plaintext));
      expect(hex(again)).not.toBe(r.sealed);
      expect(hex(fsKeys.openLocal(r.store, unhex(r.key), again))).toBe(r.plaintext);
    }
    fsKeys.free();
  });
});

describe('CRDT tables (§19.9)', () => {
  const C = () => V.crdt;

  it('tokens, sealed values and a merged view', () => {
    const fsKeys = zw.FsKeys.fromBundle(unhex(K.fs_epoch0.bundle));
    const dbKeys = fsKeys.db(V.database.ns);
    const tableId = unhex(C().table_id);
    const ck = dbKeys.crdt(tableId);
    const pkEl = encode('card-1');
    expect(hex(pkEl)).toBe(C().pk_element);
    const k = new Keys(dbKeys);
    const object = k.row(tableId, pkEl);
    expect(hex(object)).toBe(C().object);
    for (const [name, tok] of Object.entries(C().fields)) expect(hex(ck.field(name))).toBe(tok);
    const el = C().elem;
    expect(hex(encode(val(el.element)))).toBe(el.element_cbor);
    expect(hex(ck.elem(el.field, pkEl, unhex(el.element_cbor)))).toBe(el.token);
    for (const v of Object.values(C().values) as J[]) {
      const pt = fsKeys.openCrdtValue(object, unhex(v.field), unhex(v.elem), unhex(v.sealed));
      expect(hex(pt)).toBe(v.plaintext);
    }

    // An ObjState of those values, with two installations' counter entries.
    const t: TableRecord = {
      name: 'cards',
      id: tableId,
      pk: ['id'],
      indexes: [],
      pad: false,
      state: 'active',
      merge: 'crdt',
      crdtFields: { title: 'lww', votes: 'counter', labels: 'set' },
    };
    const db = { crdtKeys: () => ck, fs: { keys: fsKeys } } as unknown as Db;
    const vs = C().values;
    const device = new Uint8Array(32).fill(7);
    const s = {
      object,
      row: { hlc: 1n, device, alive: true, value: unhex(vs.row.sealed) },
      lww: [{ field: unhex(vs.lww.field), hlc: 2n, device, value: unhex(vs.lww.sealed) }],
      ctr: [1, 2].map((a) => ({
        field: unhex(vs.counter.field),
        device,
        actor: new Uint8Array(16).fill(a),
        seq: 3n,
        value: unhex(vs.counter.sealed),
      })),
      set: [
        {
          field: unhex(vs.set.field),
          elem: unhex(vs.set.elem),
          dot: new Uint8Array(12),
          device,
          value: unhex(vs.set.sealed),
        },
      ],
      version: new Uint8Array(10),
    };
    expect(viewOf(t, mergeState(db, t, s))).toEqual({
      id: 'card-1',
      title: 'Buy milk',
      votes: 6,
      labels: ['red'],
    });
    expect(viewOf(t, mergeState(db, t, { ...s, row: { ...s.row, alive: false } }))).toBeUndefined();
    // A value sealed for another field doesn't open.
    const bad = { ...s, lww: [{ ...s.lww[0]!, field: unhex(vs.counter.field) }] };
    expect(() => mergeState(db, t, bad)).toThrow('corrupt');
    ck.free();
    dbKeys.free();
    fsKeys.free();
  });
});
