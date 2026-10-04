// A minimal CBOR encoder for test fixtures (WebAuthn attestation objects
// and COSE keys): unsigned and negative integers, byte and text strings,
// arrays and maps, in the order given.

export type CborItem = number | bigint | string | Uint8Array | CborItem[] | CborMap;
/** A map with its entries in encoding order. */
export class CborMap {
  constructor(readonly entries: [CborItem, CborItem][]) {}
}

function head(major: number, n: number | bigint, out: number[]): void {
  const v = BigInt(n);
  const m = major << 5;
  if (v < 24n) out.push(m | Number(v));
  else if (v < 0x100n) out.push(m | 24, Number(v));
  else if (v < 0x10000n) out.push(m | 25, Number(v >> 8n), Number(v & 0xffn));
  else if (v < 0x100000000n) {
    out.push(m | 26);
    for (let s = 24n; s >= 0n; s -= 8n) out.push(Number((v >> s) & 0xffn));
  } else {
    out.push(m | 27);
    for (let s = 56n; s >= 0n; s -= 8n) out.push(Number((v >> s) & 0xffn));
  }
}

function item(x: CborItem, out: number[]): void {
  if (typeof x === 'number' || typeof x === 'bigint') {
    const v = BigInt(x);
    if (v >= 0n) head(0, v, out);
    else head(1, -1n - v, out);
  } else if (typeof x === 'string') {
    const b = new TextEncoder().encode(x);
    head(3, b.length, out);
    out.push(...b);
  } else if (x instanceof Uint8Array) {
    head(2, x.length, out);
    out.push(...x);
  } else if (Array.isArray(x)) {
    head(4, x.length, out);
    for (const e of x) item(e, out);
  } else {
    head(5, x.entries.length, out);
    for (const [k, v] of x.entries) {
      item(k, out);
      item(v, out);
    }
  }
}

/** Encode one item. */
export function cbor(x: CborItem): Uint8Array {
  const out: number[] = [];
  item(x, out);
  return Uint8Array.from(out);
}
