// Extended attributes: the `xattrs` field of a node's meta (formats.md
// §11.2), a CBOR map from text names to byte (or text) values, possibly
// empty. Only that shape is read and written here; the map is encoded
// deterministically (RFC 8949 §4.2.1: shortest heads, keys sorted by their
// encoding).
import { bytes, ZenError } from '@zen/client';

const bad = (why: string) => new ZenError(0, 'format', `xattrs: ${why}`);

function head(major: number, n: number): Uint8Array {
  const m = major << 5;
  if (n < 24) return new Uint8Array([m | n]);
  if (n < 0x100) return new Uint8Array([m | 24, n]);
  if (n < 0x10000) return new Uint8Array([m | 25, n >> 8, n & 0xff]);
  const out = new Uint8Array(5);
  out[0] = m | 26;
  new DataView(out.buffer).setUint32(1, n);
  return out;
}

/** Encode xattrs (empty: zero bytes, as the meta format allows). */
export function encodeXattrs(x: Map<string, Uint8Array>): Uint8Array {
  if (!x.size) return new Uint8Array(0);
  const entries = [...x].map(([k, v]) => {
    const kb = bytes.utf8(k);
    return { key: bytes.concat(head(3, kb.length), kb), value: bytes.concat(head(2, v.length), v) };
  });
  entries.sort((a, b) => bytes.compare(a.key, b.key));
  return bytes.concat(head(5, entries.length), ...entries.flatMap((e) => [e.key, e.value]));
}

/** Decode xattrs: text values come back as their UTF-8 bytes. */
export function decodeXattrs(b: Uint8Array | undefined): Map<string, Uint8Array> {
  const out = new Map<string, Uint8Array>();
  if (!b?.length) return out;
  const view = new DataView(b.buffer, b.byteOffset, b.byteLength);
  let at = 0;
  const arg = (): [number, number] => {
    if (at >= b.length) throw bad('short');
    const ib = b[at++]!;
    const major = ib >> 5;
    const info = ib & 31;
    let n: number;
    if (info < 24) n = info;
    else if (info === 24) n = b[at]!;
    else if (info === 25) n = view.getUint16(at);
    else if (info === 26) n = view.getUint32(at);
    else throw bad('unsupported length');
    at += info < 24 ? 0 : info === 24 ? 1 : info === 25 ? 2 : 4;
    if (at > b.length) throw bad('short');
    return [major, n];
  };
  const str = (major: number, n: number): Uint8Array => {
    if (major !== 2 && major !== 3) throw bad('not a string');
    if (at + n > b.length) throw bad('short');
    const s = b.slice(at, at + n);
    at += n;
    return s;
  };
  const [m, count] = arg();
  if (m !== 5) throw bad('not a map');
  for (let i = 0; i < count; i++) {
    const [km, kn] = arg();
    if (km !== 3) throw bad('a name that is not text');
    const k = bytes.fromUtf8(str(km, kn));
    const [vm, vn] = arg();
    out.set(k, str(vm, vn));
  }
  if (at !== b.length) throw bad('trailing bytes');
  return out;
}
