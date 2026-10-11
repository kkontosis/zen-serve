// Queries (spec/zendb.md §6): one access path, the other predicates on the
// client. This file has pk, unique and fast equality and the table scan;
// private and sealed ranges come with those indexes.
import { bytes } from '@zen/client';
import type { IndexDef, TableRecord } from './catalog.js';
import { encode, same, type Value } from './cbor.js';
import { DbError } from './errors.js';
import { entryPk } from './index/unique.js';
import { decodeRow, type Fields } from './row.js';
import type { DbTransaction } from './txn.js';

/** A predicate operator. */
export type Op = '=' | '<' | '<=' | '>' | '>=' | 'prefix' | 'in';

interface Cond {
  field: string;
  op: Op;
  value: Value;
}

const CURSOR = Symbol('zen-db cursor');

/**
 * Where a page ended. It holds plaintext values: keep it in the client,
 * never send it to a server or put it in a URL (§6.2).
 */
export interface Cursor {
  readonly [CURSOR]: { path: string; pk: Value };
}

/** One page of results. */
export interface Page<T> {
  rows: T[];
  /** Present when the page stopped at its limit: pass it to `after`. */
  cursor?: Cursor;
}

const te = new TextEncoder();

function rank(v: Value): number {
  if (v === null || v === undefined) return 0;
  if (typeof v === 'boolean') return 1;
  if (typeof v === 'number' || typeof v === 'bigint') return 2;
  if (typeof v === 'string') return 3;
  if (v instanceof Uint8Array) return 4;
  return 5;
}

/**
 * The order of values, as an index orders them (§5.1): null first, then
 * booleans, numbers, text (by code point), bytes; arrays and maps by their
 * encoding.
 */
export function compareValues(a: Value, b: Value): number {
  const ra = rank(a);
  const rb = rank(b);
  if (ra !== rb) return ra - rb;
  switch (ra) {
    case 0:
      return 0;
    case 1:
      return Number(a) - Number(b);
    case 2:
      return (a as number | bigint) < (b as number | bigint)
        ? -1
        : (a as number | bigint) > (b as number | bigint)
          ? 1
          : 0;
    case 3:
      return bytes.compare(te.encode(a as string), te.encode(b as string));
    case 4:
      return bytes.compare(a as Uint8Array, b as Uint8Array);
    default:
      return bytes.compare(encode(a), encode(b));
  }
}

function matches(row: Fields, c: Cond): boolean {
  const v = row[c.field] ?? null;
  switch (c.op) {
    case '=':
      return same(v, c.value);
    case '<':
      return compareValues(v, c.value) < 0;
    case '<=':
      return compareValues(v, c.value) <= 0;
    case '>':
      return compareValues(v, c.value) > 0;
    case '>=':
      return compareValues(v, c.value) >= 0;
    case 'prefix':
      if (typeof v === 'string' && typeof c.value === 'string') return v.startsWith(c.value);
      if (v instanceof Uint8Array && c.value instanceof Uint8Array) {
        return v.length >= c.value.length && bytes.equal(v.subarray(0, c.value.length), c.value);
      }
      return false;
    case 'in':
      return Array.isArray(c.value) && c.value.some((x) => same(v, x));
  }
}

/** Indexes queries may use (§3.2). */
const usable = (t: TableRecord) => t.indexes.filter((i) => i.state === 'active');

/** Whether an index keeps unique entries (§5.2). */
export const hasUniqueEntries = (i: IndexDef) =>
  i.unique && (i.kind === 'none' || i.kind === 'fast' || i.kind === 'private');

/** A query builder. Run it with `page()` or `all()`. */
export class Query<T extends Fields = Fields> {
  private readonly conds: Cond[] = [];
  private order: { field: string; desc: boolean } | undefined;
  private max: number | undefined;
  private from: { path: string; pk: Value } | undefined;

  constructor(
    readonly table: string,
    private readonly exec: <R>(run: (tx: DbTransaction) => Promise<R>) => Promise<R>,
    private readonly warn: (msg: string) => void = () => {},
  ) {}

  where(field: string, op: Op, value: Value): this {
    if (op === 'in' && !Array.isArray(value)) throw new DbError('bad_value', '"in" takes an array');
    this.conds.push({ field, op, value });
    return this;
  }

  orderBy(field: string, dir: 'asc' | 'desc' = 'asc'): this {
    this.order = { field, desc: dir === 'desc' };
    return this;
  }

  limit(n: number): this {
    this.max = n;
    return this;
  }

  after(cursor: Cursor | undefined): this {
    this.from = cursor?.[CURSOR];
    return this;
  }

  /** One page of rows. */
  page(): Promise<Page<T>> {
    return this.exec((tx) => this.run(tx));
  }

  /** The rows of one page (all of them, without a limit). */
  async all(): Promise<T[]> {
    return (await this.page()).rows;
  }

  private async run(tx: DbTransaction): Promise<Page<T>> {
    const t = await tx.tableRecord(this.table);
    const eq = new Map<string, Value>();
    for (const c of this.conds) if (c.op === '=' && !eq.has(c.field)) eq.set(c.field, c.value);
    const bound = (fields: string[]) => fields.every((f) => eq.has(f) && eq.get(f) !== null);
    const keep = (r: Fields) => this.conds.every((c) => matches(r, c));
    const tt = tx.table(this.table);

    // 1–2. pk or unique equality: at most one row.
    let pk: Value | undefined;
    let lookup = false;
    if (bound(t.pk)) {
      lookup = true;
      const parts = t.pk.map((f) => eq.get(f)!);
      pk = parts.length === 1 ? parts[0]! : parts;
    } else {
      const u = usable(t).find((i) => hasUniqueEntries(i) && bound(i.fields.map(([f]) => f)));
      if (u) {
        lookup = true;
        const value = encode(u.fields.map(([f]) => eq.get(f)!));
        const e = await tx.raw.get(tx.keys.unique(u.id, value));
        if (e) pk = entryPk(e);
      }
    }
    if (lookup) {
      const row = pk === undefined ? undefined : await tt.get(pk);
      return { rows: row && keep(row) ? [row as T] : [] };
    }

    // 3. fast equality, 5. table scan: candidates in stored-key order.
    const fast = usable(t).find((i) => i.kind === 'fast' && bound(i.fields.map(([f]) => f)));
    let path: string;
    let begin: Uint8Array;
    let end: Uint8Array | undefined;
    let value: Uint8Array | undefined;
    if (fast) {
      path = `f:${bytes.hex(fast.id)}`;
      value = encode(fast.fields.map(([f]) => eq.get(f)!));
      [begin, end] = tx.keys.fastValue(fast.id, value);
    } else {
      path = 'scan';
      this.warn(`zen-db: query on ${this.table} scans the whole table (no index fits)`);
      [begin, end] = tx.keys.rows(t.id);
    }
    if (this.from) {
      if (this.from.path !== path) throw new DbError('bad_value', 'a cursor of another query');
      const pkEl = encode(this.from.pk);
      begin = bytes.keyAfter(fast ? tx.keys.fast(fast.id, value!, pkEl) : tx.keys.row(t.id, pkEl));
    }

    const ordered = this.order !== undefined;
    const want = ordered ? Number.POSITIVE_INFINITY : (this.max ?? Number.POSITIVE_INFINITY);
    const plain = this.conds.length === 0 || (fast && this.conds.every((c) => c.op === '='));
    const batch = Number.isFinite(want) ? Math.max(plain ? want : want * 2, 16) : undefined;
    const rows: { pk: Value; row: Fields }[] = [];
    let more = true;
    while (more && rows.length < want) {
      const page = await tx.raw.rangeStored(begin, end, batch ? { limit: batch } : {});
      more = batch !== undefined && page.length >= batch;
      const last = page.at(-1);
      if (last) begin = bytes.keyAfter(last.key);
      if (fast) {
        const pks = page.map((e) => entryPk(e.value));
        const got = await tx.raw.getAll(pks.map((p) => tx.keys.row(t.id, encode(p))));
        for (let i = 0; i < pks.length && rows.length < want; i++) {
          const b = got[i];
          if (!b) continue; // an entry written with its row: absent only mid-change
          const r = await tt.decode(t, encode(pks[i]!), b);
          if (keep(r.fields)) rows.push({ pk: pks[i]!, row: r.fields });
        }
      } else {
        for (const e of page) {
          if (rows.length >= want) break;
          const pk = decodeRow(e.value).pk;
          const r = await tt.decode(t, encode(pk), e.value);
          if (keep(r.fields)) rows.push({ pk, row: r.fields });
        }
      }
    }

    if (this.order) {
      const { field, desc } = this.order;
      if (this.max !== undefined && rows.length > this.max) {
        throw new DbError('needs_index', `orderBy ${field} needs an index for a limited result`);
      }
      rows.sort(
        (a, b) => compareValues(a.row[field] ?? null, b.row[field] ?? null) * (desc ? -1 : 1),
      );
      return { rows: rows.map((r) => r.row as T) };
    }
    const out: Page<T> = { rows: rows.map((r) => r.row as T) };
    if (this.max !== undefined && rows.length >= this.max && rows.length) {
      out.cursor = { [CURSOR]: { path, pk: rows.at(-1)!.pk } };
    }
    return out;
  }
}
