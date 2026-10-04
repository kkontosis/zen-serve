// Byte helpers.

const enc = new TextEncoder();
const dec = new TextDecoder('utf-8', { fatal: true });

/** UTF-8 bytes of a string. */
export function utf8(s: string): Uint8Array {
  return enc.encode(s);
}

/** A string from UTF-8 bytes (throws on invalid UTF-8). */
export function fromUtf8(b: Uint8Array): string {
  return dec.decode(b);
}

/** Lowercase hex. */
export function hex(b: Uint8Array): string {
  let s = '';
  for (const x of b) s += x.toString(16).padStart(2, '0');
  return s;
}

/** Bytes from hex. */
export function fromHex(s: string): Uint8Array {
  if (s.length % 2 !== 0 || /[^0-9a-fA-F]/.test(s)) throw new Error('invalid hex');
  const out = new Uint8Array(s.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = Number.parseInt(s.slice(2 * i, 2 * i + 2), 16);
  return out;
}

/** base64url without padding. */
export function b64url(b: Uint8Array): string {
  let bin = '';
  for (const x of b) bin += String.fromCharCode(x);
  return btoa(bin).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

/** Bytes from base64url (padding optional). */
export function fromB64url(s: string): Uint8Array {
  const b64 = s.replace(/-/g, '+').replace(/_/g, '/');
  const bin = atob(b64 + '='.repeat((4 - (b64.length % 4)) % 4));
  return Uint8Array.from(bin, (c) => c.charCodeAt(0));
}

/** Concatenate. */
export function concat(...parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let i = 0;
  for (const p of parts) {
    out.set(p, i);
    i += p.length;
  }
  return out;
}

/** Byte equality. */
export function equal(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

/** Bytewise order: negative, zero or positive. */
export function compare(a: Uint8Array, b: Uint8Array): number {
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i++) if (a[i] !== b[i]) return a[i]! - b[i]!;
  return a.length - b.length;
}

/** Random bytes from the platform RNG. */
export function randomBytes(n: number): Uint8Array {
  const out = new Uint8Array(n);
  crypto.getRandomValues(out);
  return out;
}

/** The smallest key after every key with this prefix, or undefined for none. */
export function prefixEnd(prefix: Uint8Array): Uint8Array | undefined {
  const out = prefix.slice();
  for (let i = out.length - 1; i >= 0; i--) {
    if (out[i]! < 0xff) {
      out[i]! += 1;
      return out.slice(0, i + 1);
    }
  }
  return undefined;
}

/** `key ‖ 0x00`: the first key after `key`. */
export function keyAfter(key: Uint8Array): Uint8Array {
  return concat(key, new Uint8Array([0]));
}
