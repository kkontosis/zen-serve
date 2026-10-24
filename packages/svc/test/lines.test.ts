// LineReader: whole lines of a step's input, CRLF or bare LF, with a limit.
import { describe, expect, it } from 'vitest';
import { LineReader } from '../src/lines.js';

const enc = new TextEncoder();
const dec = new TextDecoder();

function all(r: LineReader): string[] {
  const out: string[] = [];
  for (let l = r.nextText(); l !== null; l = r.nextText()) out.push(l);
  return out;
}

describe('LineReader', () => {
  it('CRLF and bare LF; a partial last line stays unread', () => {
    const r = new LineReader(enc.encode('EHLO a\r\nNOOP\nhalf'));
    expect(all(r)).toEqual(['EHLO a', 'NOOP']);
    expect(r.consumed).toBe(13);
    expect(dec.decode(r.rest())).toBe('half');
    expect(r.tooLong).toBe(false);
  });

  it('empty lines, a lone CR inside a line, and bytes', () => {
    const r = new LineReader(enc.encode('\r\n\na\rb\r\n'));
    expect(r.next()).toEqual(new Uint8Array(0));
    expect(r.next()).toEqual(new Uint8Array(0));
    expect(dec.decode(r.next()!)).toBe('a\rb');
    expect(r.next()).toBeNull();
    expect(r.consumed).toBe(8);
  });

  it('rest() after a command: the data that follows it', () => {
    const r = new LineReader(enc.encode('DATA\r\nSubject: x\r\n'));
    expect(r.nextText()).toBe('DATA');
    expect(dec.decode(r.rest())).toBe('Subject: x\r\n');
  });

  it('max counts the terminator; a longer line, complete or not, is tooLong', () => {
    expect(all(new LineReader(enc.encode('abc\r\n'), { max: 5 }))).toEqual(['abc']);
    const r = new LineReader(enc.encode('ok\r\nabcd\r\n'), { max: 5 });
    expect(r.nextText()).toBe('ok');
    expect(r.nextText()).toBeNull();
    expect(r.tooLong).toBe(true);
    expect(r.consumed).toBe(4);
    expect(r.nextText()).toBeNull();
    // No terminator yet, but already over the limit.
    const p = new LineReader(enc.encode('abcdef'), { max: 5 });
    expect(p.next()).toBeNull();
    expect(p.tooLong).toBe(true);
    // Within the limit, a partial line just waits.
    const q = new LineReader(enc.encode('abcd'), { max: 5 });
    expect(q.next()).toBeNull();
    expect(q.tooLong).toBe(false);
  });
});
