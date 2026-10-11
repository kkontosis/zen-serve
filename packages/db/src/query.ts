// Queries (spec/zendb.md §6): one access path, the other predicates on the
// client. Paths: pk, unique and fast equality, a private or sealed range,
// the table scan.
import { bytes } from '@zen/client';
import type { IndexDef, IndexField, TableRecord } from './catalog.js';
import { encode, same, type Value } from './cbor.js';
import { DbError } from './errors.js';
import { privateScan, sealedScan } from './index/private.js';
import { entryPk } from './index/unique.js';
import { decodeRow, type Fields } from './row.js';
import { decodeSortKey, encodeComponent, encodePrefix, fitsType } from './sortkey.js';
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
  readonly [CURSOR]: Position;
}

/** A scan's or an index's position: the last pk, or the last sort key. */
interface Position {
  path: string;
  pk?: Value;
  key?: Uint8Array;
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

/** A range on a private or sealed index (§6.1 step 4). */
interface RangePlan {
  ix: IndexDef;
  lo: Uint8Array;
  hi: Uint8Array | undefined;
  /** No entry can match. */
  empty: boolean;
  /** Scan descending sort keys. */
  reverse: boolean;
  /** The scan gives the `orderBy` (or there is none). */
  ordered: boolean;
  /** Conditions the bounds enforce exactly. */
  covered: Set<Cond>;
}

const RANGE_OPS = new Set<Op>(['<', '<=', '>', '>=']);
const FLIP: Record<string, Op> = { '<': '>', '<=': '>=', '>': '<', '>=': '<=' };

const usableEq = (c: Cond, [f, type]: IndexField) =>
  c.field === f && c.op === '=' && c.value !== null && fitsType(c.value, type);

function usableRange(c: Cond, [f, type]: IndexField): boolean {
  if (c.field !== f || c.value === null) return false;
  if (RANGE_OPS.has(c.op)) return fitsType(c.value, type);
  return (
    c.op === 'prefix' &&
    ((type === 'text' && typeof c.value === 'string') ||
      (type === 'bytes' && c.value instanceof Uint8Array))
  );
}

function concat(a: Uint8Array, b: Uint8Array): Uint8Array {
  const out = new Uint8Array(a.length + b.length);
  out.set(a);
  out.set(b, a.length);
  return out;
}

/** Lexicographic `a > b`. */
function greater(a: number[], b: number[]): boolean {
  const i = a.findIndex((x, j) => x !== b[j]);
  return i >= 0 && a[i]! > b[i]!;
}

/**
 * Pick a private or sealed index whose leading fields are bound by `=`
 * and whose next field carries a range or the `orderBy` (or that every
 * `=` binds), and compute its sort-key bounds. Prefers an index giving the
 * order, then more bound fields, a range, and private over sealed.
 */
function planRange(
  t: TableRecord,
  conds: Cond[],
  order: { field: string; desc: boolean } | undefined,
): RangePlan | undefined {
  let best:
    | { ix: IndexDef; eqs: Cond[]; ranges: Cond[]; ordered: boolean; score: number[] }
    | undefined;
  for (const ix of usable(t)) {
    if (ix.kind !== 'private' && ix.kind !== 'sealed') continue;
    const eqs: Cond[] = [];
    for (const f of ix.fields) {
      const c = conds.find((c) => usableEq(c, f));
      if (!c) break;
      eqs.push(c);
    }
    const next = ix.fields[eqs.length];
    const ranges = next ? conds.filter((c) => usableRange(c, next)) : [];
    const ordersNext = !!order && next?.[0] === order.field;
    if (!next || ranges.length || ordersNext) {
      const ordered =
        !order || ordersNext || ix.fields.slice(0, eqs.length).some(([f]) => f === order.field);
      const score = [
        ordered ? 1 : 0,
        eqs.length,
        ranges.length ? 1 : 0,
        ix.kind === 'private' ? 1 : 0,
      ];
      if (!best || greater(score, best.score)) best = { ix, eqs, ranges, ordered, score };
    }
  }
  if (!best) return undefined;
  const { ix, eqs, ranges, ordered } = best;

  let p: Uint8Array = new Uint8Array(0);
  eqs.forEach((c, i) => {
    const [, type, desc] = ix.fields[i]!;
    p = concat(p, encodeComponent(c.value, type, desc));
  });
  let lo: Uint8Array = p;
  let hi = p.length ? bytes.prefixEnd(p) : undefined;
  let empty = false;
  const atLeast = (x: Uint8Array | undefined) => {
    if (!x) empty = true;
    else if (bytes.compare(x, lo) > 0) lo = x;
  };
  const below = (x: Uint8Array | undefined) => {
    if (x && (!hi || bytes.compare(x, hi) < 0)) hi = x;
  };
  const next = ix.fields[eqs.length];
  for (const c of ranges) {
    const [, type, desc] = next!;
    if (c.op === 'prefix') {
      const e = concat(p, encodePrefix(c.value as string | Uint8Array, desc));
      atLeast(e);
      below(bytes.prefixEnd(e));
      continue;
    }
    const e = concat(p, encodeComponent(c.value, type, desc));
    switch (desc ? FLIP[c.op] : c.op) {
      case '>=':
        atLeast(e);
        break;
      case '>':
        atLeast(bytes.prefixEnd(e));
        break;
      case '<':
        below(e);
        break;
      case '<=':
        below(bytes.prefixEnd(e));
        break;
    }
  }
  const reverse = !!order && next?.[0] === order.field && order.desc !== next[2];
  // `<`/`<=` on an ascending field also take nulls, which the row filter keeps too.
  return { ix, lo, hi, empty, reverse, ordered, covered: new Set([...eqs, ...ranges]) };
}

/** A query builder. Run it with `page()` or `all()`. */
export class Query<T extends Fields = Fields> {
  private readonly conds: Cond[] = [];
  private order: { field: string; desc: boolean } | undefined;
  private max: number | undefined;
  private from: Position | undefined;

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
    tx.live();
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

    // 4. a private or sealed range, when it gives the order asked for;
    // 3. fast equality; 4. a range otherwise; 5. a table scan.
    const range = planRange(t, this.conds, this.order);
    const fast = usable(t).find((i) => i.kind === 'fast' && bound(i.fields.map(([f]) => f)));
    let rows: { pk: Value; row: Fields; key?: Uint8Array }[];
    let path: string;
    let ordered = !this.order;
    if (range && (!fast || (this.order && range.ordered))) {
      path = `${range.ix.kind === 'private' ? 'p' : 's'}:${bytes.hex(range.ix.id)}`;
      ordered = range.ordered;
      rows = await this.rangeRows(tx, t, range, path, keep, ordered);
    } else {
      path = fast ? `f:${bytes.hex(fast.id)}` : 'scan';
      rows = await this.keyRows(tx, t, fast, eq, path, keep);
    }

    if (!ordered) {
      const { field, desc } = this.order!;
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
      const last = rows.at(-1)!;
      out.cursor = { [CURSOR]: last.key ? { path, key: last.key } : { path, pk: last.pk } };
    }
    return out;
  }

  /** Rows wanted from a path: the limit, or every row when they are sorted afterwards. */
  private want(ordered: boolean): number {
    return ordered ? (this.max ?? Number.POSITIVE_INFINITY) : Number.POSITIVE_INFINITY;
  }

  private position(path: string): Position | undefined {
    if (this.from && this.from.path !== path) {
      throw new DbError('bad_value', 'a cursor of another query');
    }
    return this.from;
  }

  /** Fast equality or a scan: candidates in stored-key order. */
  private async keyRows(
    tx: DbTransaction,
    t: TableRecord,
    fast: IndexDef | undefined,
    eq: Map<string, Value>,
    path: string,
    keep: (r: Fields) => boolean,
  ): Promise<{ pk: Value; row: Fields }[]> {
    const tt = tx.table(this.table);
    let begin: Uint8Array;
    let end: Uint8Array | undefined;
    let value: Uint8Array | undefined;
    if (fast) {
      value = encode(fast.fields.map(([f]) => eq.get(f)!));
      [begin, end] = tx.keys.fastValue(fast.id, value);
    } else {
      this.warn(`zen-db: query on ${this.table} scans the whole table (no index fits)`);
      [begin, end] = tx.keys.rows(t.id);
    }
    const from = this.position(path);
    if (from) {
      const pkEl = encode(from.pk!);
      begin = bytes.keyAfter(fast ? tx.keys.fast(fast.id, value!, pkEl) : tx.keys.row(t.id, pkEl));
    }

    const want = this.want(this.order === undefined);
    const plain = this.conds.length === 0 || (fast && this.conds.every((c) => c.op === '='));
    const batch = Number.isFinite(want) ? Math.max(plain ? want : want * 2, 16) : undefined;
    const rows: { pk: Value; row: Fields }[] = [];
    let more = true;
    while (more && rows.length < want) {
      tx.live();
      const page = await tx.raw.rangeStored(begin, end, batch ? { limit: batch } : {});
      more = batch !== undefined && page.length >= batch;
      const last = page.at(-1);
      if (last) begin = bytes.keyAfter(last.key);
      if (fast) {
        const pks = page.map((e) => entryPk(e.value));
        const got = await tx.raw.getEntries(pks.map((p) => tx.keys.row(t.id, encode(p))));
        for (let i = 0; i < pks.length && rows.length < want; i++) {
          const e = got[i];
          if (!e) continue; // an entry written with its row: absent only mid-change
          const r = await tt.decode(t, encode(pks[i]!), e.value);
          tx.noteRow(e.key, e.version, r);
          if (keep(r.fields)) rows.push({ pk: pks[i]!, row: r.fields });
        }
      } else {
        for (const e of page) {
          if (rows.length >= want) break;
          const pk = decodeRow(e.value).pk;
          const r = await tt.decode(t, encode(pk), e.value);
          tx.noteRow(e.key, e.version, r);
          if (keep(r.fields)) rows.push({ pk, row: r.fields });
        }
      }
    }
    return rows;
  }

  /** A private or sealed range: entries in sort-key order, then their rows (§6.1 step 4). */
  private async rangeRows(
    tx: DbTransaction,
    t: TableRecord,
    plan: RangePlan,
    path: string,
    keep: (r: Fields) => boolean,
    ordered: boolean,
  ): Promise<{ pk: Value; row: Fields; key: Uint8Array }[]> {
    let { lo, hi } = plan;
    const from = this.position(path);
    if (from) {
      const k = from.key!;
      if (plan.reverse) hi = !hi || bytes.compare(k, hi) < 0 ? k : hi;
      else {
        const after = bytes.keyAfter(k);
        if (bytes.compare(after, lo) > 0) lo = after;
      }
    }
    const rows: { pk: Value; row: Fields; key: Uint8Array }[] = [];
    if (plan.empty || (hi && bytes.compare(lo, hi) >= 0)) return rows;
    const ix = plan.ix;
    const keys = (ix.kind === 'private' ? privateScan : sealedScan)(tx, ix, lo, hi, plan.reverse);
    const dirs = ix.fields.map(([, , desc]) => ({ desc }));
    const tt = tx.table(this.table);
    const want = this.want(ordered);
    const exact = this.conds.every((c) => plan.covered.has(c));
    for (;;) {
      const left = want - rows.length;
      if (left <= 0) break;
      const n = Number.isFinite(left) ? Math.min(Math.max(exact ? left : left * 2, 16), 256) : 256;
      const batch: { key: Uint8Array; pk: Value }[] = [];
      while (batch.length < n) {
        const r = await keys.next();
        if (r.done) break;
        const parts = decodeSortKey(dirs, r.value).pk;
        batch.push({ key: r.value, pk: parts.length === 1 ? parts[0]! : parts });
      }
      if (!batch.length) break;
      tx.live();
      const got = await tx.raw.getEntries(batch.map((b) => tx.keys.row(t.id, encode(b.pk))));
      for (let i = 0; i < batch.length && rows.length < want; i++) {
        const e = got[i];
        if (!e) continue; // long mode: the row went after the index was read
        const r = await tt.decode(t, encode(batch[i]!.pk), e.value);
        tx.noteRow(e.key, e.version, r);
        if (keep(r.fields)) rows.push({ ...batch[i]!, row: r.fields });
      }
      if (batch.length < n) break;
    }
    return rows;
  }
}
