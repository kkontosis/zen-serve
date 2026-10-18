// Joining pushed input (api.md §14.10), and the state encoding (svc.md §7).
import { describe, expect, it } from 'vitest';
import { decodeState, encodeState, Input } from '../src/index.js';

const b = (s: string) => new TextEncoder().encode(s);
const t = (i: Input) => new TextDecoder().decode(i.bytes());

describe('Input', () => {
  it('joins chunks in order, dropping repeats and overlaps', () => {
    const i = new Input(0n);
    expect(i.add(0n, b('abc'))).toBe(true);
    expect(i.add(0n, b('abc'))).toBe(false);
    expect(i.add(1n, b('bcde'))).toBe(true);
    expect(t(i)).toBe('abcde');
    expect(i.end).toBe(5n);
  });

  it('a first chunk may start before in_acked', () => {
    const i = new Input(10n);
    i.add(7n, b('xyzabc'));
    expect(t(i)).toBe('abc');
    expect(i.base).toBe(10n);
  });

  it('holds chunks past a gap until it fills', () => {
    const i = new Input(0n);
    expect(i.add(4n, b('ef'))).toBe(false);
    expect(i.add(2n, b('cd'))).toBe(false);
    expect(i.pending).toBe(4);
    expect(i.add(0n, b('ab'))).toBe(true);
    expect(t(i)).toBe('abcdef');
    expect(i.pending).toBe(0);
  });

  it('advances past acked input', () => {
    const i = new Input(0n);
    i.add(0n, b('hello\nwor'));
    i.add(11n, b('!'));
    i.advance(6n);
    expect(i.base).toBe(6n);
    expect(t(i)).toBe('wor');
    i.add(9n, b('ld'));
    expect(t(i)).toBe('world!');
    i.advance(20n);
    expect(t(i)).toBe('');
    expect(i.end).toBe(20n);
  });
});

describe('state', () => {
  it('round-trips, with an absent app state as null', () => {
    const s = { engine: 'raw', engineState: new Uint8Array(0), app: b('n=3') };
    expect(decodeState(encodeState(s))).toEqual(s);
    expect(decodeState(encodeState({ ...s, app: null })).app).toBeNull();
    expect(() => decodeState(b('x'))).toThrow();
  });
});
