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

export function encodePageRecord(ids: Uint8Array[], lens: number[], hashes: Uint8Array[]) {
  const out = new Uint8Array(1 + ENTRY * ids.length);
  out[0] = 1;
  const dv = new DataView(out.buffer);
  for (let i = 0; i < ids.length; i++) {
    const at = 1 + ENTRY * i;
    out.set(ids[i]!, at);
    dv.setUint32(at + 16, lens[i]!);
    out.set(hashes[i]!, at + 20);
  }
  return out;
}

export function decodePageRecord(
  b: Uint8Array,
): { ids: Uint8Array[]; lens: number[]; hashes: Uint8Array[] } | undefined {
  if (b[0] !== 1 || (b.length - 1) % ENTRY) return undefined;
  const dv = new DataView(b.buffer, b.byteOffset, b.length);
  const ids: Uint8Array[] = [];
  const lens: number[] = [];
  const hashes: Uint8Array[] = [];
  for (let at = 1; at < b.length; at += ENTRY) {
    ids.push(b.slice(at, at + 16));
    lens.push(dv.getUint32(at + 16));
    hashes.push(b.slice(at + 20, at + ENTRY));
  }
  return { ids, lens, hashes };
}

export function encodeInlineRecord(hashes: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(1 + 32 * hashes.length);
  out[0] = 1;
  for (const [i, h] of hashes.entries()) out.set(h, 1 + 32 * i);
  return out;
}

export function decodeInlineRecord(b: Uint8Array): Uint8Array[] | undefined {
  if (b[0] !== 1 || (b.length - 1) % 32) return undefined;
  const out: Uint8Array[] = [];
  for (let at = 1; at < b.length; at += 32) out.push(b.slice(at, at + 32));
  return out;
}

/** Plaintext lengths of an index's chunks. */
export function lensOf(ix: ChunkIndex, size: number): number[] {
  if (ix.lens) return ix.lens;
  const cs = ix.chunkSize!;
  const n = ix.chunks.length;
  return ix.chunks.map((_, i) => (i < n - 1 ? cs : size - cs * (n - 1)));
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
  if (m.version === 1 || !hashes || hashes.length !== ix.chunks.length) return [];
  if (!m.pages?.length) {
    return [[await inlineRecordKey(sealedManifest), encodeInlineRecord(hashes)]];
  }
  const lens = lensOf(ix, m.size);
  const out: [Uint8Array, Uint8Array][] = [];
  let at = 0;
  const counts = pageCounts(m);
  if (counts.reduce((a, c) => a + c, 0) !== ix.chunks.length) return [];
  m.pages.forEach((p, i) => {
    const n = counts[i]!;
    out.push([
      pageRecordKey(p.id),
      encodePageRecord(
        ix.chunks.slice(at, at + n),
        lens.slice(at, at + n),
        hashes.slice(at, at + n),
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
  const ix: ChunkIndex =
    m.version === 3 ? { cdc: m.cdc, chunks: [], lens: [] } : { chunkSize: m.chunkSize, chunks: [] };
  if (m.fileKey) Object.assign(ix, { fileKey: m.fileKey, keyEpoch: m.keyEpoch });
  if (m.pages?.length) {
    const hashes: Uint8Array[] = [];
    const counts = pageCounts(m);
    for (const [i, p] of m.pages.entries()) {
      const raw = await get(pageRecordKey(p.id));
      const r = raw && decodePageRecord(raw);
      if (!r || r.ids.length !== counts[i]) return undefined;
      ix.chunks.push(...r.ids);
      ix.lens?.push(...r.lens);
      hashes.push(...r.hashes);
    }
    Object.assign(ix, { hashes, pageSize: m.pageSize, pages: m.pages });
    return ix;
  }
  const raw = await get(await inlineRecordKey(v.sealedManifest));
  const hashes = raw && decodeInlineRecord(raw);
  if (!hashes || hashes.length !== v.chunks.length) return undefined;
  ix.chunks = v.chunks;
  if (m.version === 3) ix.lens = (m.chunks ?? []).map((c) => c.len);
  ix.hashes = hashes;
  return ix;
}
