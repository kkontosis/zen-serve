// IMAP's syntax (RFC 3501 §9), as far as the handler needs it: framing a
// command with its literals (RFC 7888's LITERAL+ too), a tokenizer, sequence
// sets, fetch attributes and search keys, and a session's view of its
// mailbox as uid ranges.

/** A command or response problem: `BAD` (syntax) or `NO` (refused). */
export class ImapError extends Error {
  constructor(
    readonly kind: 'BAD' | 'NO',
    message: string,
    /** A response code, without its brackets (`CANNOT`, `BADCHARSET (UTF-8)`). */
    readonly code?: string,
  ) {
    super(message);
  }
}

const bad = (m: string) => new ImapError('BAD', m);

// ---------------------------------------------------------------- framing

/** Where the next command of some input ends, if it is complete. */
export type Frame =
  /** A whole command: `end` is just past its final CRLF. */
  | { kind: 'command'; end: number }
  /**
   * More input is needed. `syncs`: synchronizing literals whose header
   * arrived (each needs a continuation before the client sends it).
   */
  | { kind: 'need'; syncs: number }
  /**
   * The command passes `max`. `at`: input up to here can be dropped, if
   * the client waits for a continuation there (a synchronizing literal
   * refused before it was sent); otherwise -1, and the connection can't
   * continue.
   */
  | { kind: 'too_long'; at: number };

/** A literal's header at the end of a line (`{n}` or `{n+}`, before CRLF). */
function literalAt(
  buf: Uint8Array,
  start: number,
  lf: number,
): { n: number; sync: boolean } | null {
  let i = lf - 1;
  if (i >= start && buf[i] === 13) i--;
  if (i < start || buf[i] !== 125) return null; // }
  i--;
  let sync = true;
  if (i >= start && buf[i] === 43) {
    sync = false; // +
    i--;
  }
  let n = 0;
  let mul = 1;
  let digits = 0;
  while (i >= start && buf[i]! >= 48 && buf[i]! <= 57) {
    n += (buf[i]! - 48) * mul;
    mul *= 10;
    digits++;
    i--;
  }
  if (digits === 0 || digits > 10 || i < start || buf[i] !== 123) return null; // {
  return { n, sync };
}

/** Find the end of the command at the start of `buf`, at most `max` bytes. */
export function frame(buf: Uint8Array, max: number): Frame {
  let pos = 0;
  let syncs = 0;
  for (;;) {
    const lf = buf.indexOf(10, pos);
    if (lf < 0) return buf.length > max ? { kind: 'too_long', at: -1 } : { kind: 'need', syncs };
    if (lf + 1 > max) return { kind: 'too_long', at: -1 };
    const lit = literalAt(buf, pos, lf);
    if (!lit) return { kind: 'command', end: lf + 1 };
    if (lf + 1 + lit.n > max) {
      // A synchronizing literal can be refused before it is sent.
      return { kind: 'too_long', at: lit.sync ? lf + 1 : -1 };
    }
    if (lit.sync) syncs++;
    if (lf + 1 + lit.n > buf.length) return { kind: 'need', syncs };
    pos = lf + 1 + lit.n;
  }
}

// ---------------------------------------------------------------- tokens

const latin1 = new TextDecoder('latin1');
const utf8 = new TextDecoder();

/** atom-specials (RFC 3501 §9) other than CTL and SP. */
const SPECIALS = new Set([...'(){%*"\\]'].map((c) => c.charCodeAt(0)));

const isAtomChar = (b: number) => b > 32 && b < 127 && !SPECIALS.has(b);

/** A cursor over one framed command (its CRLF included). */
export class Reader {
  pos = 0;
  constructor(readonly buf: Uint8Array) {}

  peek(): number {
    return this.pos < this.buf.length ? this.buf[this.pos]! : -1;
  }

  /** At the command's CRLF. */
  atEnd(): boolean {
    const b = this.peek();
    return b === 13 || b === 10 || b === -1;
  }

  end(): void {
    if (!this.atEnd()) throw bad('Unexpected arguments');
  }

  sp(): void {
    if (this.peek() !== 32) throw bad('Expected a space');
    this.pos++;
  }

  /** A space, if there is one. */
  maybeSp(): boolean {
    if (this.peek() !== 32) return false;
    this.pos++;
    return true;
  }

  char(c: string): void {
    if (this.peek() !== c.charCodeAt(0)) throw bad(`Expected ${c}`);
    this.pos++;
  }

  maybe(c: string): boolean {
    if (this.peek() !== c.charCodeAt(0)) return false;
    this.pos++;
    return true;
  }

  /** Bytes while `ok` holds. */
  private run(ok: (b: number) => boolean): string {
    const start = this.pos;
    while (this.pos < this.buf.length && ok(this.buf[this.pos]!)) this.pos++;
    return latin1.decode(this.buf.subarray(start, this.pos));
  }

  atom(): string {
    const a = this.run(isAtomChar);
    if (!a) throw bad('Expected an atom');
    return a;
  }

  /** A tag: atom characters but `+`. */
  tag(): string {
    const t = this.run((b) => isAtomChar(b) && b !== 43);
    if (!t) throw bad('Expected a tag');
    return t;
  }

  /** A quoted string or a literal, as bytes. */
  stringBytes(): Uint8Array {
    if (this.maybe('"')) {
      const out: number[] = [];
      for (;;) {
        const b = this.peek();
        if (b === -1 || b === 13 || b === 10) throw bad('Unterminated quoted string');
        this.pos++;
        if (b === 34) break;
        if (b === 92) {
          const n = this.peek();
          if (n !== 34 && n !== 92) throw bad('Bad escape in a quoted string');
          this.pos++;
          out.push(n);
        } else out.push(b);
      }
      return Uint8Array.from(out);
    }
    if (this.maybe('{')) {
      const digits = this.run((b) => b >= 48 && b <= 57);
      this.maybe('+');
      this.char('}');
      this.maybe('\r');
      this.char('\n');
      const n = Number(digits);
      if (!digits || this.pos + n > this.buf.length) throw bad('Bad literal');
      const s = this.buf.subarray(this.pos, this.pos + n);
      this.pos += n;
      return s;
    }
    throw bad('Expected a string');
  }

  string(): string {
    return utf8.decode(this.stringBytes());
  }

  /** An atom (with `]`) or a string. */
  astring(): string {
    const b = this.peek();
    if (b === 34 || b === 123) return this.string();
    const a = this.run((x) => isAtomChar(x) || x === 93);
    if (!a) throw bad('Expected an atom or a string');
    return a;
  }

  /** A LIST pattern: an astring that may hold `%` and `*`. */
  pattern(): string {
    const b = this.peek();
    if (b === 34 || b === 123) return this.string();
    const a = this.run((x) => isAtomChar(x) || x === 93 || x === 37 || x === 42);
    if (!a) throw bad('Expected a mailbox pattern');
    return a;
  }

  number(): number {
    const d = this.run((b) => b >= 48 && b <= 57);
    if (!d || d.length > 10) throw bad('Expected a number');
    return Number(d);
  }

  /** `(a b c)`, each read by `item`. */
  list<T>(item: () => T): T[] {
    this.char('(');
    const out: T[] = [];
    if (this.maybe(')')) return out;
    for (;;) {
      out.push(item());
      if (this.maybe(')')) return out;
      this.sp();
    }
  }

  seqSet(): SeqSet {
    return parseSeqSet(this.run((b) => (b >= 48 && b <= 58) || b === 42 || b === 44));
  }

  /** A flag: `\Name` or a keyword atom. */
  flag(): string {
    if (this.maybe('\\')) return `\\${this.atom()}`;
    return this.atom();
  }
}

// ---------------------------------------------------------------- sequence sets

/** A number of a set: `*` is the largest in use. */
export type SeqNum = number | '*';

/** A sequence set (RFC 3501 §9 `sequence-set`). */
export type SeqSet = [SeqNum, SeqNum][];

export function parseSeqSet(text: string): SeqSet {
  if (!text) throw bad('Expected a sequence set');
  const num = (s: string): SeqNum => {
    if (s === '*') return '*';
    if (!/^[1-9][0-9]{0,9}$/.test(s)) throw bad(`Bad sequence number ${s}`);
    return Number(s);
  };
  return text.split(',').map((part) => {
    const [a, b, more] = part.split(':');
    if (more !== undefined || a === undefined) throw bad(`Bad sequence range ${part}`);
    const x = num(a);
    return [x, b === undefined ? x : num(b)];
  });
}

/** Whether `n` is in the set, `*` being `max` (a range's ends in either order). */
export function inSet(set: SeqSet, n: number, max: number): boolean {
  for (const [a, b] of set) {
    const x = a === '*' ? max : a;
    const y = b === '*' ? max : b;
    if (n >= Math.min(x, y) && n <= Math.max(x, y)) return true;
  }
  return false;
}

/** The set's largest number, `*` being `max`. */
export function setMax(set: SeqSet, max: number): number {
  let m = 0;
  for (const [a, b] of set) m = Math.max(m, a === '*' ? max : a, b === '*' ? max : b);
  return m;
}

// ---------------------------------------------------------------- views

/** A session's uids, as sorted inclusive ranges: sequence numbers are positions in it. */
export type View = [number, number][];

/** Ranges of sorted, distinct uids. */
export function toView(uids: number[]): View {
  const v: View = [];
  for (const u of uids) {
    const last = v[v.length - 1];
    if (last && u === last[1] + 1) last[1] = u;
    else v.push([u, u]);
  }
  return v;
}

export function viewCount(v: View): number {
  return v.reduce((n, [a, b]) => n + b - a + 1, 0);
}

/** The uid at a sequence number (from 1), or undefined. */
export function uidAt(v: View, seq: number): number | undefined {
  let left = seq;
  for (const [a, b] of v) {
    const n = b - a + 1;
    if (left <= n) return a + left - 1;
    left -= n;
  }
  return undefined;
}

/** A uid's sequence number, or 0. */
export function seqOf(v: View, uid: number): number {
  let before = 0;
  for (const [a, b] of v) {
    if (uid < a) return 0;
    if (uid <= b) return before + uid - a + 1;
    before += b - a + 1;
  }
  return 0;
}

/** Every uid, in order. */
export function viewUids(v: View): number[] {
  const out: number[] = [];
  for (const [a, b] of v) for (let u = a; u <= b; u++) out.push(u);
  return out;
}

export function viewMax(v: View): number {
  return v.length ? v[v.length - 1]![1] : 0;
}

/** Remove a uid. */
export function viewRemove(v: View, uid: number): View {
  const out: View = [];
  for (const [a, b] of v) {
    if (uid < a || uid > b) out.push([a, b]);
    else {
      if (a < uid) out.push([a, uid - 1]);
      if (uid < b) out.push([uid + 1, b]);
    }
  }
  return out;
}

/** Add uids larger than every uid in it, in order. */
export function viewAppend(v: View, uids: number[]): View {
  const out: View = v.map(([a, b]) => [a, b]);
  for (const u of uids) {
    const last = out[out.length - 1];
    if (last && u <= last[1]) continue;
    if (last && u === last[1] + 1) last[1] = u;
    else out.push([u, u]);
  }
  return out;
}

// ---------------------------------------------------------------- fetch

/** A body section (RFC 3501 §6.4.5): the whole message, its header, a subset of it, or its text. */
export type Section =
  | { kind: 'all' }
  | { kind: 'header' }
  | { kind: 'text' }
  | { kind: 'fields'; not: boolean; names: string[] };

/** A fetch item. `peek`: don't set `\Seen`. `name`: as the response names it. */
export type FetchAtt =
  | { kind: 'uid' | 'flags' | 'internaldate' | 'size' }
  | {
      kind: 'body';
      section: Section;
      peek: boolean;
      partial?: [number, number];
      name: string;
    };

/** Fetch attributes: one, a macro, or a list. */
export function parseFetchAtts(r: Reader): FetchAtt[] {
  if (r.peek() === 40) return r.list(() => fetchAtt(r));
  const save = r.pos;
  const word = r.atom().toUpperCase();
  if (word === 'FAST') {
    return [{ kind: 'flags' }, { kind: 'internaldate' }, { kind: 'size' }];
  }
  if (word === 'ALL' || word === 'FULL') throw bad(`${word} needs ENVELOPE, which isn't supported`);
  r.pos = save;
  return [fetchAtt(r)];
}

function fetchAtt(r: Reader): FetchAtt {
  // Letters, digits and dots: `BODY.PEEK`, `RFC822.SIZE`.
  const start = r.pos;
  while (/[A-Za-z0-9.]/.test(String.fromCharCode(r.peek()))) r.pos++;
  const word = latin1.decode(r.buf.subarray(start, r.pos)).toUpperCase();
  switch (word) {
    case 'UID':
      return { kind: 'uid' };
    case 'FLAGS':
      return { kind: 'flags' };
    case 'INTERNALDATE':
      return { kind: 'internaldate' };
    case 'RFC822.SIZE':
      return { kind: 'size' };
    case 'RFC822':
      return { kind: 'body', section: { kind: 'all' }, peek: false, name: 'RFC822' };
    case 'RFC822.HEADER':
      return { kind: 'body', section: { kind: 'header' }, peek: true, name: 'RFC822.HEADER' };
    case 'RFC822.TEXT':
      return { kind: 'body', section: { kind: 'text' }, peek: false, name: 'RFC822.TEXT' };
    case 'BODY':
    case 'BODY.PEEK': {
      if (r.peek() !== 91) throw bad('BODY without a section (BODYSTRUCTURE) is not supported');
      const section = parseSection(r);
      let partial: [number, number] | undefined;
      if (r.maybe('<')) {
        const o = r.number();
        r.char('.');
        const n = r.number();
        if (n === 0) throw bad('A partial of 0 bytes');
        r.char('>');
        partial = [o, n];
      }
      return {
        kind: 'body',
        section,
        peek: word === 'BODY.PEEK',
        ...(partial ? { partial } : {}),
        name: `BODY[${sectionText(section)}]`,
      };
    }
    case 'ENVELOPE':
    case 'BODYSTRUCTURE':
      throw bad(`${word} is not supported`);
    default:
      throw bad(`Unknown fetch attribute ${word || String.fromCharCode(r.peek())}`);
  }
}

function parseSection(r: Reader): Section {
  r.char('[');
  if (r.maybe(']')) return { kind: 'all' };
  const start = r.pos;
  while (/[A-Za-z0-9.]/.test(String.fromCharCode(r.peek()))) r.pos++;
  const word = latin1.decode(r.buf.subarray(start, r.pos)).toUpperCase();
  let s: Section;
  if (word === 'HEADER') s = { kind: 'header' };
  else if (word === 'TEXT') s = { kind: 'text' };
  else if (word === 'HEADER.FIELDS' || word === 'HEADER.FIELDS.NOT') {
    r.sp();
    const names = r.list(() => r.astring());
    if (!names.length) throw bad('HEADER.FIELDS needs field names');
    s = { kind: 'fields', not: word.endsWith('.NOT'), names };
  } else throw bad(`Section ${word || '?'} is not supported (no MIME parts)`);
  r.char(']');
  return s;
}

export function sectionText(s: Section): string {
  switch (s.kind) {
    case 'all':
      return '';
    case 'header':
      return 'HEADER';
    case 'text':
      return 'TEXT';
    default:
      return `HEADER.FIELDS${s.not ? '.NOT' : ''} (${s.names.map((n) => n.toUpperCase()).join(' ')})`;
  }
}

// ---------------------------------------------------------------- search

/** A search key (RFC 3501 §6.4.4), as far as it is supported. */
export type SearchKey =
  | { k: 'all' | 'none' }
  | { k: 'flag'; flag: string; set: boolean }
  | { k: 'field'; field: 'from' | 'to' | 'cc' | 'subject'; text: string }
  | { k: 'larger' | 'smaller'; n: number }
  | { k: 'before' | 'on' | 'since'; day: number }
  | { k: 'seq' | 'uid'; set: SeqSet }
  | { k: 'not'; key: SearchKey }
  | { k: 'or'; a: SearchKey; b: SearchKey }
  | { k: 'and'; keys: SearchKey[] };

const MONTHS = ['JAN', 'FEB', 'MAR', 'APR', 'MAY', 'JUN', 'JUL', 'AUG', 'SEP', 'OCT', 'NOV', 'DEC'];

/** A date (`1-Feb-1994`), as days since the epoch. */
function parseDate(r: Reader): number {
  const quoted = r.maybe('"');
  const t = r.atom();
  if (quoted) r.char('"');
  const m = /^(\d{1,2})-([A-Za-z]{3})-(\d{4})$/.exec(t);
  const mon = m ? MONTHS.indexOf(m[2]!.toUpperCase()) : -1;
  if (!m || mon < 0) throw bad(`Bad date ${t}`);
  return Math.floor(Date.UTC(Number(m[3]), mon, Number(m[1])) / 86_400_000);
}

const FLAG_KEYS: Record<string, [string, boolean]> = {
  ANSWERED: ['\\Answered', true],
  DELETED: ['\\Deleted', true],
  DRAFT: ['\\Draft', true],
  FLAGGED: ['\\Flagged', true],
  SEEN: ['\\Seen', true],
  UNANSWERED: ['\\Answered', false],
  UNDELETED: ['\\Deleted', false],
  UNDRAFT: ['\\Draft', false],
  UNFLAGGED: ['\\Flagged', false],
  UNSEEN: ['\\Seen', false],
};

const UNSUPPORTED = new Set([
  'BODY',
  'TEXT',
  'HEADER',
  'BCC',
  'KEYWORD',
  'UNKEYWORD',
  'SENTBEFORE',
  'SENTON',
  'SENTSINCE',
]);

/** Search keys up to the command's end; a leading `CHARSET` is checked. */
export function parseSearch(r: Reader): SearchKey {
  const save = r.pos;
  const first = isAtomChar(r.peek()) ? r.atom().toUpperCase() : '';
  if (first === 'CHARSET') {
    r.sp();
    const cs = r.astring().toUpperCase();
    if (cs !== 'UTF-8' && cs !== 'US-ASCII') {
      throw new ImapError('NO', `Charset ${cs} is not supported`, 'BADCHARSET (UTF-8 US-ASCII)');
    }
    r.sp();
  } else r.pos = save;
  const keys: SearchKey[] = [];
  for (;;) {
    keys.push(searchKey(r));
    if (r.atEnd()) break;
    r.sp();
  }
  return keys.length === 1 ? keys[0]! : { k: 'and', keys };
}

function searchKey(r: Reader): SearchKey {
  if (r.maybe('(')) {
    const keys: SearchKey[] = [];
    for (;;) {
      keys.push(searchKey(r));
      if (r.maybe(')')) break;
      r.sp();
    }
    return { k: 'and', keys };
  }
  const c = r.peek();
  if ((c >= 48 && c <= 57) || c === 42) return { k: 'seq', set: r.seqSet() };
  const word = r.atom().toUpperCase();
  const flag = FLAG_KEYS[word];
  if (flag) return { k: 'flag', flag: flag[0], set: flag[1] };
  switch (word) {
    case 'ALL':
    case 'OLD':
      // Nothing is recent here (`\Recent` isn't kept).
      return { k: 'all' };
    case 'NEW':
    case 'RECENT':
      return { k: 'none' };
    case 'FROM':
    case 'TO':
    case 'CC':
    case 'SUBJECT':
      r.sp();
      return {
        k: 'field',
        field: word.toLowerCase() as 'from' | 'to' | 'cc' | 'subject',
        text: r.astring(),
      };
    case 'LARGER':
    case 'SMALLER':
      r.sp();
      return { k: word === 'LARGER' ? 'larger' : 'smaller', n: r.number() };
    case 'BEFORE':
    case 'ON':
    case 'SINCE':
      r.sp();
      return { k: word.toLowerCase() as 'before' | 'on' | 'since', day: parseDate(r) };
    case 'UID':
      r.sp();
      return { k: 'uid', set: r.seqSet() };
    case 'NOT':
      r.sp();
      return { k: 'not', key: searchKey(r) };
    case 'OR': {
      r.sp();
      const a = searchKey(r);
      r.sp();
      return { k: 'or', a, b: searchKey(r) };
    }
    default:
      if (UNSUPPORTED.has(word)) throw new ImapError('NO', `SEARCH ${word} is not supported`);
      throw bad(`Unknown search key ${word}`);
  }
}

/** What a search key looks at. */
export interface Searched {
  seq: number;
  uid: number;
  flags: string[];
  size: number;
  /** Unix milliseconds. */
  received: number;
  fields: { from: string; to: string; cc: string; subject: string };
}

/** The largest sequence number and uid in the view, for `*`. */
export interface SearchScope {
  maxSeq: number;
  maxUid: number;
}

export function matches(k: SearchKey, m: Searched, scope: SearchScope): boolean {
  switch (k.k) {
    case 'all':
      return true;
    case 'none':
      return false;
    case 'flag':
      return m.flags.includes(k.flag) === k.set;
    case 'field':
      return m.fields[k.field].toLowerCase().includes(k.text.toLowerCase());
    case 'larger':
      return m.size > k.n;
    case 'smaller':
      return m.size < k.n;
    case 'before':
      return Math.floor(m.received / 86_400_000) < k.day;
    case 'on':
      return Math.floor(m.received / 86_400_000) === k.day;
    case 'since':
      return Math.floor(m.received / 86_400_000) >= k.day;
    case 'seq':
      return inSet(k.set, m.seq, scope.maxSeq);
    case 'uid':
      return inSet(k.set, m.uid, scope.maxUid);
    case 'not':
      return !matches(k.key, m, scope);
    case 'or':
      return matches(k.a, m, scope) || matches(k.b, m, scope);
    case 'and':
      return k.keys.every((x) => matches(x, m, scope));
  }
}

// ---------------------------------------------------------------- output

/** A string as IMAP sends it: quoted when it can be, else a literal. */
export function quote(s: string): string {
  if (/^[\x20-\x7e]*$/.test(s)) return `"${s.replace(/[\\"]/g, (c) => `\\${c}`)}"`;
  return `{${new TextEncoder().encode(s).length}}\r\n${s}`;
}

const MON = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

/** An INTERNALDATE: `"17-Jul-1996 02:44:25 +0000"`, in UTC. */
export function internalDate(ms: number): string {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, '0');
  return (
    `"${p(d.getUTCDate())}-${MON[d.getUTCMonth()]}-${d.getUTCFullYear()} ` +
    `${p(d.getUTCHours())}:${p(d.getUTCMinutes())}:${p(d.getUTCSeconds())} +0000"`
  );
}

/** A LIST pattern's match of a mailbox name (`*` and `%`; this server has no hierarchy). */
export function patternMatches(ref: string, pattern: string, name: string): boolean {
  const full = ref + pattern;
  const re = new RegExp(
    `^${full.replace(/[.+?^${}()|[\]\\]/g, '\\$&').replace(/[*%]/g, '.*')}$`,
    name.toUpperCase() === 'INBOX' ? 'i' : '',
  );
  return re.test(name);
}
