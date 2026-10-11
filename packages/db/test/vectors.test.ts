// The TypeScript encoders against spec/test-vectors/zendb.json (zendb.md §17).
import { readFileSync } from 'node:fs';
import { bytes, initWasm, zw } from '@zen/client';
import { beforeAll, describe, expect, it } from 'vitest';
import { decode, encode, type Value } from '../src/cbor.js';
import { Keys, u64 } from '../src/keys.js';
import { cutParts, encodeRow, joinParts, partsDigest, rowBucket } from '../src/row.js';
import { type FieldType, sortKey } from '../src/sortkey.js';

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
