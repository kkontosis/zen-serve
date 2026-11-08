// The replica's chunk-index records (store `index`, spec/fs.md §11.1): what
// it knows of a version's content for reuse by later writes, binary and per
// page, keyed by content so that one record serves a local version and the
// version the server gives it once sent.
//
//   0x70 ‖ page_id                      → u8 1 ‖ n × (chunk_id(16) ‖ u32 len ‖ sha256(32))
//   0x6d ‖ sha256(sealed manifest)[..16] → u8 1 ‖ n × sha256(32)       (an inline version)
//
// Legacy records (JSON of a whole `ChunkIndex`, keyed `node ‖ dot`) are
// still read.
import { type AnyManifest, bytes, type ChunkIndex, type FileVersion } from '@zen/client';

const { concat } = bytes;

const PAGE = 0x70;
const INLINE = 0x6d;
const ENTRY = 52;

export const pageRecordKey = (page: Uint8Array): Uint8Array =>
  bytes.concat(new Uint8Array([PAGE]), page);

export async function inlineRecordKey(sealedManifest: Uint8Array): Promise<Uint8Array> {
  const h = new Uint8Array(
    await crypto.subtle.digest('SHA-256', sealedManifest as Uint8Array<ArrayBuffer>),
  );
  return bytes.concat(new Uint8Array([INLINE]), h.subarray(0, 16));
}

/** A page's record: `ids` (`n × 16`), `lens` and `hashes` (`n × 32`) packed as the index keeps them. */
export function encodePageRecord(ids: Uint8Array, lens: ArrayLike<number>, hashes: Uint8Array) {
  const n = lens.length;
  const out = new Uint8Array(1 + ENTRY * n);
  out[0] = 1;
  const dv = new DataView(out.buffer);
  for (let i = 0; i < n; i++) {
    const at = 1 + ENTRY * i;
    out.set(ids.subarray(16 * i, 16 * i + 16), at);
    dv.setUint32(at + 16, lens[i]!);
    out.set(hashes.subarray(32 * i, 32 * i + 32), at + 20);
  }
  return out;
}

export function decodePageRecord(
  b: Uint8Array,
): { ids: Uint8Array; lens: Uint32Array; hashes: Uint8Array } | undefined {
  if (b[0] !== 1 || (b.length - 1) % ENTRY) return undefined;
  const n = (b.length - 1) / ENTRY;
  const dv = new DataView(b.buffer, b.byteOffset, b.length);
  const ids = new Uint8Array(16 * n);
  const lens = new Uint32Array(n);
  const hashes = new Uint8Array(32 * n);
  for (let i = 0; i < n; i++) {
    const at = 1 + ENTRY * i;
    ids.set(b.subarray(at, at + 16), 16 * i);
    lens[i] = dv.getUint32(at + 16);
    hashes.set(b.subarray(at + 20, at + ENTRY), 32 * i);
  }
  return { ids, lens, hashes };
}

/** An inline version's record: its hashes (`n × 32`). */
export function encodeInlineRecord(hashes: Uint8Array): Uint8Array {
  return concat(new Uint8Array([1]), hashes);
}

export function decodeInlineRecord(b: Uint8Array): Uint8Array | undefined {
  if (b[0] !== 1 || (b.length - 1) % 32) return undefined;
  return b.slice(1);
}

/** Plaintext lengths of an index's chunks. */
export function lensOf(ix: ChunkIndex, size: number): Uint32Array {
  if (ix.lens) return ix.lens;
  const cs = ix.chunkSize!;
  const n = ix.n;
  return Uint32Array.from({ length: n }, (_, i) => (i < n - 1 ? cs : size - cs * (n - 1)));
}

/** The chunk count of each page of a paged manifest. */
export function pageCounts(m: AnyManifest): number[] {
  if (m.version === 1) return [];
  const pages = m.pages ?? [];
  if (m.version === 3) return (m.pages ?? []).map((p) => p.count);
  const ps = m.pageSize ?? 0;
  return pages.map((_, i) => (i < pages.length - 1 ? ps : m.n - ps * (pages.length - 1)));
}

/** The records of a content's index (with hashes): one per page, or one for an inline version. */
export async function recordsOf(
  m: AnyManifest,
  sealedManifest: Uint8Array,
  ix: ChunkIndex,
): Promise<[Uint8Array, Uint8Array][]> {
  const hashes = ix.hashes;
  if (m.version === 1 || !hashes || hashes.length !== 32 * ix.n) return [];
  if (!m.pages?.length) {
    return [[await inlineRecordKey(sealedManifest), encodeInlineRecord(hashes)]];
  }
  const lens = lensOf(ix, m.size);
  const out: [Uint8Array, Uint8Array][] = [];
  let at = 0;
  const counts = pageCounts(m);
  if (counts.reduce((a, c) => a + c, 0) !== ix.n) return [];
  m.pages.forEach((p, i) => {
    const n = counts[i]!;
    out.push([
      pageRecordKey(p.id),
      encodePageRecord(
        ix.ids.subarray(16 * at, 16 * (at + n)),
        lens.subarray(at, at + n),
        hashes.subarray(32 * at, 32 * (at + n)),
      ),
    ]);
    at += n;
  });
  return out;
}

/** A version's index from its records, when every one is there. */
export async function indexFromRecords(
  v: FileVersion,
  get: (key: Uint8Array) => Promise<Uint8Array | undefined>,
): Promise<ChunkIndex | undefined> {
  const m = v.manifest;
  if (m.version === 1) return undefined;
  const n = v.chunkCount;
  const keys = m.fileKey ? { fileKey: m.fileKey, keyEpoch: m.keyEpoch } : {};
  const form = m.version === 3 ? { cdc: m.cdc } : { chunkSize: m.chunkSize };
  if (m.pages?.length) {
    const counts = pageCounts(m);
    if (counts.reduce((a, c) => a + c, 0) !== n) return undefined;
    const ids = new Uint8Array(16 * n);
    const lens = new Uint32Array(n);
    const hashes = new Uint8Array(32 * n);
    let at = 0;
    for (const [i, p] of m.pages.entries()) {
      const raw = await get(pageRecordKey(p.id));
      const r = raw && decodePageRecord(raw);
      if (!r || r.lens.length !== counts[i]) return undefined;
      ids.set(r.ids, 16 * at);
      lens.set(r.lens, at);
      hashes.set(r.hashes, 32 * at);
      at += r.lens.length;
    }
    return {
      ...form,
      n,
      ids,
      ...(m.version === 3 ? { lens } : {}),
      hashes,
      ...keys,
      pageSize: m.pageSize,
      pages: m.pages,
    };
  }
  const raw = await get(await inlineRecordKey(v.sealedManifest));
  const hashes = raw && decodeInlineRecord(raw);
  if (!hashes || hashes.length !== 32 * v.chunks.length) return undefined;
  return {
    ...form,
    n: v.chunks.length,
    ids: concat(...v.chunks),
    ...(m.version === 3 ? { lens: Uint32Array.from((m.chunks ?? []).map((c) => c.len)) } : {}),
    hashes,
    ...keys,
  };
}
