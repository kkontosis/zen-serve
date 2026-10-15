import { expect, it } from 'vitest';
import { keyBetween, keysBetween } from '../fractional.js';

it('keys sort strictly between their neighbours', () => {
  let s = 1;
  const rand = () => {
    s ^= s << 13;
    s ^= s >>> 17;
    s ^= s << 5;
    return (s >>> 0) / 2 ** 32;
  };
  const keys = [keyBetween()];
  for (let i = 0; i < 2000; i++) {
    const at = Math.floor(rand() * (keys.length + 1));
    const k = keyBetween(keys[at - 1], keys[at]);
    if (at > 0) expect(k > keys[at - 1]!).toBe(true);
    if (at < keys.length) expect(k < keys[at]!).toBe(true);
    expect(k.endsWith('0')).toBe(false);
    keys.splice(at, 0, k);
  }
  expect([...keys].sort()).toEqual(keys);
  expect(new Set(keys).size).toBe(keys.length);
});

it('appending and prepending stay short', () => {
  const tail = keysBetween(undefined, undefined, 500);
  expect([...tail].sort()).toEqual(tail);
  expect(Math.max(...tail.map((k) => k.length))).toBeLessThan(20);
  let head = 'V';
  for (let i = 0; i < 100; i++) head = keyBetween(undefined, head);
  expect(head.length).toBeLessThan(6);
});

it('refuses bad input', () => {
  expect(() => keyBetween('b', 'a')).toThrow(RangeError);
  expect(() => keyBetween('a0')).toThrow(RangeError);
  expect(() => keyBetween('a', 'a')).toThrow(RangeError);
});
