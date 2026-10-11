// Catalog records (spec/zendb.md §3, §8.1): CBOR maps with small integer keys.
import { decode, encode, type Value } from './cbor.js';
import { DbError } from './errors.js';
import type { FieldType } from './sortkey.js';

/** The database record (§3.1). */
export interface DbRecord {
  format: number;
  schema: number;
  integrity: string;
  createdHlc: bigint;
}

/** An index kind (§3.2). */
export type IndexKind = 'private' | 'fast' | 'none' | 'sealed' | 'oblivious';

/** One indexed field: name, declared type, direction. */
export type IndexField = [name: string, type: FieldType, desc: boolean];

/** An index definition (§3.2). */
export interface IndexDef {
  name: string;
  id: Uint8Array;
  fields: IndexField[];
  kind: IndexKind;
  unique: boolean;
  state: 'building' | 'active' | 'dropping';
  fanout?: number;
  shards?: number;
  builtTo?: Uint8Array;
  decoys?: number;
  maxBytes?: number;
  blocks?: number;
}

/** A table's change topic (§12.7). */
export interface ChangeDef {
  topic: Uint8Array[];
  image: 'keys' | 'full';
}

/** A table record (§3.2). */
export interface TableRecord {
  name: string;
  id: Uint8Array;
  pk: string[];
  indexes: IndexDef[];
  changes?: ChangeDef;
  pad: boolean;
  state: 'active' | 'dropping';
  merge?: 'txn' | 'crdt';
  crdtFields?: Record<string, 'lww' | 'counter' | 'set'>;
}

/** A migration step's record (§8.1). */
export interface MigrationRecord {
  step: number;
  state: 'running' | 'done';
  progress?: Uint8Array;
  hlc: bigint;
}

const corrupt = (what: string) => new DbError('corrupt', `a malformed ${what}`);

function record(bytes: Uint8Array, what: string): Map<Value, Value> {
  const m = decode(bytes);
  if (!(m instanceof Map)) throw corrupt(what);
  return m;
}

function text(m: Map<Value, Value>, k: number, what: string): string {
  const v = m.get(k);
  if (typeof v !== 'string') throw corrupt(what);
  return v;
}

function uint(m: Map<Value, Value>, k: number, what: string): number {
  const v = m.get(k);
  if (typeof v !== 'number' || !Number.isInteger(v) || v < 0) throw corrupt(what);
  return v;
}

function optUint(m: Map<Value, Value>, k: number, what: string): number | undefined {
  return m.has(k) ? uint(m, k, what) : undefined;
}

function u64v(m: Map<Value, Value>, k: number, what: string): bigint {
  const v = m.get(k);
  if (typeof v === 'number' && Number.isInteger(v) && v >= 0) return BigInt(v);
  if (typeof v === 'bigint' && v >= 0n) return v;
  throw corrupt(what);
}

function bytesv(m: Map<Value, Value>, k: number, what: string): Uint8Array {
  const v = m.get(k);
  if (!(v instanceof Uint8Array)) throw corrupt(what);
  return v;
}

function bool(m: Map<Value, Value>, k: number, what: string): boolean {
  const v = m.get(k);
  if (typeof v !== 'boolean') throw corrupt(what);
  return v;
}

/** A map without its `undefined` entries. */
function rec(entries: [number, Value | undefined][]): Map<Value, Value> {
  return new Map(entries.filter((e): e is [number, Value] => e[1] !== undefined));
}

export function encodeDbRecord(r: DbRecord): Uint8Array {
  return encode(
    rec([
      [1, r.format],
      [2, r.schema],
      [3, r.integrity],
      [4, r.createdHlc],
    ]),
  );
}

export function decodeDbRecord(b: Uint8Array): DbRecord {
  const w = 'DbRecord';
  const m = record(b, w);
  return {
    format: uint(m, 1, w),
    schema: uint(m, 2, w),
    integrity: text(m, 3, w),
    createdHlc: u64v(m, 4, w),
  };
}

function indexValue(d: IndexDef): Value {
  return rec([
    [1, d.name],
    [2, d.id],
    [3, d.fields.map(([n, t, desc]) => [n, t, desc])],
    [4, d.kind],
    [5, d.unique],
    [6, d.state],
    [7, d.fanout],
    [8, d.shards],
    [9, d.builtTo],
    [10, d.decoys],
    [11, d.maxBytes],
    [12, d.blocks],
  ]);
}

const KINDS = ['private', 'fast', 'none', 'sealed', 'oblivious'];
const TYPES = ['text', 'bytes', 'int', 'float', 'bool'];

function decodeIndex(v: Value): IndexDef {
  const w = 'IndexDef';
  if (!(v instanceof Map)) throw corrupt(w);
  const fields = v.get(3);
  if (!Array.isArray(fields)) throw corrupt(w);
  const kind = text(v, 4, w);
  const state = text(v, 6, w);
  if (!KINDS.includes(kind) || !['building', 'active', 'dropping'].includes(state)) {
    throw corrupt(w);
  }
  const d: IndexDef = {
    name: text(v, 1, w),
    id: bytesv(v, 2, w),
    fields: fields.map((f) => {
      if (
        !Array.isArray(f) ||
        typeof f[0] !== 'string' ||
        !TYPES.includes(f[1] as string) ||
        typeof f[2] !== 'boolean'
      ) {
        throw corrupt(w);
      }
      return [f[0], f[1] as FieldType, f[2]];
    }),
    kind: kind as IndexKind,
    unique: bool(v, 5, w),
    state: state as IndexDef['state'],
  };
  const opt = { fanout: 7, shards: 8, decoys: 10, maxBytes: 11, blocks: 12 } as const;
  for (const [name, k] of Object.entries(opt)) {
    const x = optUint(v, k, w);
    if (x !== undefined) d[name as keyof typeof opt] = x;
  }
  if (v.has(9)) d.builtTo = bytesv(v, 9, w);
  return d;
}

export function encodeTableRecord(t: TableRecord): Uint8Array {
  return encode(
    rec([
      [1, t.name],
      [2, t.id],
      [3, t.pk],
      [4, t.indexes.map(indexValue)],
      [
        5,
        t.changes &&
          new Map<Value, Value>([
            [1, t.changes.topic],
            [2, t.changes.image],
          ]),
      ],
      [6, t.pad],
      [7, t.state],
      [8, t.merge],
      [9, t.crdtFields],
    ]),
  );
}

export function decodeTableRecord(b: Uint8Array): TableRecord {
  const w = 'TableRecord';
  const m = record(b, w);
  const pk = m.get(3);
  const indexes = m.get(4);
  if (!Array.isArray(pk) || !pk.length || pk.some((p) => typeof p !== 'string')) throw corrupt(w);
  if (!Array.isArray(indexes)) throw corrupt(w);
  const state = text(m, 7, w);
  if (state !== 'active' && state !== 'dropping') throw corrupt(w);
  const t: TableRecord = {
    name: text(m, 1, w),
    id: bytesv(m, 2, w),
    pk: pk as string[],
    indexes: indexes.map(decodeIndex),
    pad: bool(m, 6, w),
    state,
  };
  const ch = m.get(5);
  if (ch !== undefined) {
    const topic = ch instanceof Map ? ch.get(1) : undefined;
    const image = ch instanceof Map ? ch.get(2) : undefined;
    if (
      !Array.isArray(topic) ||
      topic.some((s) => !(s instanceof Uint8Array)) ||
      (image !== 'keys' && image !== 'full')
    ) {
      throw corrupt(w);
    }
    t.changes = { topic: topic as Uint8Array[], image };
  }
  const merge = m.get(8);
  if (merge !== undefined) {
    if (merge !== 'txn' && merge !== 'crdt') throw corrupt(w);
    t.merge = merge;
  }
  const cf = m.get(9);
  if (cf !== undefined) t.crdtFields = cf as TableRecord['crdtFields'];
  return t;
}

export function encodeMigrationRecord(r: MigrationRecord): Uint8Array {
  return encode(
    rec([
      [1, r.step],
      [2, r.state],
      [3, r.progress],
      [4, r.hlc],
    ]),
  );
}

export function decodeMigrationRecord(b: Uint8Array): MigrationRecord {
  const w = 'MigrationRecord';
  const m = record(b, w);
  const state = text(m, 2, w);
  if (state !== 'running' && state !== 'done') throw corrupt(w);
  const r: MigrationRecord = { step: uint(m, 1, w), state, hlc: u64v(m, 4, w) };
  if (m.has(3)) r.progress = bytesv(m, 3, w);
  return r;
}
