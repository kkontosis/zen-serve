// IMAP's syntax as the handler reads it (apps/mail/src/imapparse.ts):
// framing with literals, the tokenizer, sequence sets, fetch attributes,
// search keys and views.
import { describe, expect, it } from 'vitest';
import {
  frame,
  ImapError,
  inSet,
  matches,
  parseFetchAtts,
  parseSearch,
  parseSeqSet,
  patternMatches,
  quote,
  Reader,
  type Searched,
  seqOf,
  toView,
  uidAt,
  viewAppend,
  viewCount,
  viewRemove,
  viewUids,
} from '../src/imapparse.js';

const enc = (s: string) => new TextEncoder().encode(s);
const reader = (s: string) => new Reader(enc(s));

describe('framing', () => {
  it('a line, literals both kinds, and what is still missing', () => {
    expect(frame(enc('a NOOP\r\nb NOOP\r\n'), 100)).toEqual({ kind: 'command', end: 8 });
    expect(frame(enc('a NOOP'), 100)).toEqual({ kind: 'need', syncs: 0 });
    expect(frame(enc('a LOGIN {3}\r\n'), 100)).toEqual({ kind: 'need', syncs: 1 });
    expect(frame(enc('a LOGIN {3}\r\nbob {2+}\r\n'), 100)).toEqual({ kind: 'need', syncs: 1 });
    const whole = 'a LOGIN {3}\r\nbob {2+}\r\npw\r\n';
    expect(frame(enc(`${whole}next`), 100)).toEqual({ kind: 'command', end: whole.length });
    // A literal's bytes may hold CRLF and braces.
    const odd = 'a X {5}\r\n{1}\r\n\r\n';
    expect(frame(enc(odd), 100)).toEqual({ kind: 'command', end: odd.length });
  });

  it('over the limit: a synchronizing literal refused before it is sent; otherwise not', () => {
    expect(frame(enc('a X {500}\r\n'), 100)).toEqual({ kind: 'too_long', at: 11 });
    expect(frame(enc('a X {500+}\r\n'), 100)).toEqual({ kind: 'too_long', at: -1 });
    expect(frame(enc('x'.repeat(101)), 100)).toEqual({ kind: 'too_long', at: -1 });
  });
});

describe('tokens', () => {
  it('atoms, quoted strings with escapes, literals, lists and flags', () => {
    const r = reader('t1 LOGIN "a \\"b\\" \\\\c" {3}\r\nx y (\\Seen $Junk) ]x\r\n');
    expect(r.tag()).toBe('t1');
    r.sp();
    expect(r.atom()).toBe('LOGIN');
    r.sp();
    expect(r.string()).toBe('a "b" \\c');
    r.sp();
    expect(r.astring()).toBe('x y');
    r.sp();
    expect(r.list(() => r.flag())).toEqual(['\\Seen', '$Junk']);
    r.sp();
    expect(r.astring()).toBe(']x');
    r.end();
    expect(() => reader('+x').tag()).toThrow(ImapError);
    expect(() => reader('"open\r\n').string()).toThrow(/Unterminated/);
  });

  it('quoting output', () => {
    expect(quote('a "b"')).toBe('"a \\"b\\""');
    expect(quote('é')).toBe('{2}\r\né');
  });

  it('LIST patterns', () => {
    for (const p of ['*', '%', 'INBOX', 'inbox', 'IN*', 'I%X']) {
      expect(patternMatches('', p, 'INBOX')).toBe(true);
    }
    expect(patternMatches('', 'Sent', 'INBOX')).toBe(false);
  });
});

describe('sequence sets', () => {
  it('numbers, ranges either way, and *', () => {
    const s = parseSeqSet('1,3:4,9:7,12:*');
    expect([1, 3, 4, 7, 8, 9, 12, 20].every((n) => inSet(s, n, 20))).toBe(true);
    expect([2, 5, 6, 10, 11].some((n) => inSet(s, n, 20))).toBe(false);
    // `n:*` always holds the largest, even past it.
    expect(inSet(parseSeqSet('30:*'), 20, 20)).toBe(true);
    for (const bad of ['', '0', '1:2:3', 'a', '1,,2']) expect(() => parseSeqSet(bad)).toThrow();
  });
});

describe('fetch attributes', () => {
  const atts = (s: string) => parseFetchAtts(reader(`${s}\r\n`));
  it('items, macros, sections and partials', () => {
    expect(atts('FAST').map((a) => a.kind)).toEqual(['flags', 'internaldate', 'size']);
    expect(atts('(UID FLAGS RFC822.SIZE)').map((a) => a.kind)).toEqual(['uid', 'flags', 'size']);
    expect(atts('BODY.PEEK[HEADER.FIELDS (Subject From)]<0.100>')).toEqual([
      {
        kind: 'body',
        section: { kind: 'fields', not: false, names: ['Subject', 'From'] },
        peek: true,
        partial: [0, 100],
        name: 'BODY[HEADER.FIELDS (SUBJECT FROM)]',
      },
    ]);
    expect(atts('RFC822')).toMatchObject([
      { section: { kind: 'all' }, peek: false, name: 'RFC822' },
    ]);
    expect(atts('RFC822.HEADER')).toMatchObject([{ section: { kind: 'header' }, peek: true }]);
    expect(atts('BODY[TEXT]')).toMatchObject([{ section: { kind: 'text' }, name: 'BODY[TEXT]' }]);
    for (const bad of ['ALL', 'FULL', 'ENVELOPE', 'BODYSTRUCTURE', 'BODY', 'BODY[1]', 'X']) {
      expect(() => atts(bad), bad).toThrow(ImapError);
    }
  });
});

describe('search', () => {
  const m = (o: Partial<Searched>): Searched => ({
    seq: 1,
    uid: 10,
    flags: [],
    size: 100,
    received: Date.UTC(2026, 9, 24, 12),
    fields: { from: 'Boss <boss@corp>', to: 'me@x', cc: '', subject: 'Quarterly Report' },
    ...o,
  });
  const scope = { maxSeq: 5, maxUid: 50 };
  const hit = (q: string, x: Searched) => matches(parseSearch(reader(`${q}\r\n`)), x, scope);

  it('keys, combinations and dates', () => {
    expect(hit('UNSEEN', m({}))).toBe(true);
    expect(hit('SEEN', m({ flags: ['\\Seen'] }))).toBe(true);
    expect(hit('SUBJECT report FROM BOSS', m({}))).toBe(true);
    expect(hit('OR FLAGGED LARGER 50', m({}))).toBe(true);
    expect(hit('NOT (SMALLER 200 TO me)', m({}))).toBe(false);
    expect(hit('ON 24-Oct-2026', m({}))).toBe(true);
    expect(hit('SINCE "25-Oct-2026"', m({}))).toBe(false);
    expect(hit('BEFORE 25-Oct-2026', m({}))).toBe(true);
    expect(hit('UID 5:*', m({}))).toBe(true);
    expect(hit('2:4', m({}))).toBe(false);
    expect(hit('CHARSET UTF-8 NEW', m({}))).toBe(false);
    expect(hit('OLD', m({}))).toBe(true);
  });

  it('refusals: unsupported keys are NO, a bad charset names the ones there are', () => {
    const e = (q: string) => {
      try {
        parseSearch(reader(`${q}\r\n`));
      } catch (x) {
        return x as ImapError;
      }
      throw new Error('no error');
    };
    expect(e('BODY x')).toMatchObject({ kind: 'NO' });
    expect(e('CHARSET KOI8-R ALL')).toMatchObject({
      kind: 'NO',
      code: 'BADCHARSET (UTF-8 US-ASCII)',
    });
    expect(e('FROBNICATE')).toMatchObject({ kind: 'BAD' });
    expect(e('ON 32-Foo-2026')).toMatchObject({ kind: 'BAD' });
  });
});

describe('views', () => {
  it('sequence numbers, removal and appends', () => {
    let v = toView([1, 2, 3, 7, 8, 20]);
    expect(v).toEqual([
      [1, 3],
      [7, 8],
      [20, 20],
    ]);
    expect(viewCount(v)).toBe(6);
    expect([1, 3, 4, 5, 6, 7].map((n) => uidAt(v, n))).toEqual([1, 3, 7, 8, 20, undefined]);
    expect([1, 7, 20, 5].map((u) => seqOf(v, u))).toEqual([1, 4, 6, 0]);
    v = viewRemove(v, 2);
    expect(v).toEqual([
      [1, 1],
      [3, 3],
      [7, 8],
      [20, 20],
    ]);
    v = viewAppend(v, [5, 21, 22, 30]);
    expect(viewUids(v)).toEqual([1, 3, 7, 8, 20, 21, 22, 30]);
  });
});
