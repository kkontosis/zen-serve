// A request in, a response out (spec/sw.md §3–§7), over the mounts' replicas.
import { bytes } from '@zen/client';
import {
  CONTENT_TYPE,
  decodeXattrs,
  type Entry,
  isLocal,
  nodeType,
  type Replica,
  type ShownVersion,
} from '@zen/fs';
import { sandboxHeaders } from './headers.js';
import { contentType } from './mime.js';
import { type Mount, route } from './mounts.js';
import { type ErrorKind, errorPage, type Listed, listingPage } from './pages.js';
import { parseRange } from './range.js';

/** What the worker serves. */
export interface Site {
  readonly mounts: readonly Mount[];
  /**
   * The replica of a mount's tree. Rejecting with a `ZenError` of code
   * `locked` (no device, or unlocking failed) or `starting` (still
   * loading) gives that 503 (sw.md §7).
   */
  replica(m: Mount): Promise<Replica>;
  /** Told of each error that becomes a 500. */
  onError?(e: unknown, req: Request): void;
}

const ALLOW = 'GET, HEAD';

/** A response; HEAD gets the headers and no body. */
function respond(
  req: Request,
  status: number,
  headers: Headers,
  body: string | ReadableStream<Uint8Array> | null,
): Response {
  if (req.method === 'HEAD' && body instanceof ReadableStream) void body.cancel();
  return new Response(req.method === 'HEAD' ? null : body, { status, headers });
}

function fail(req: Request, kind: ErrorKind, extra: Record<string, string> = {}): Response {
  const e = errorPage(kind);
  const html = bytes.utf8(e.html);
  const h = sandboxHeaders({
    'Content-Type': 'text/html; charset=utf-8',
    'Content-Length': String(html.length),
    ...(e.reason ? { 'Zen-Reason': e.reason } : {}),
    ...extra,
  });
  return respond(req, e.status, h, e.html);
}

function redirect(req: Request, location: string): Response {
  return respond(req, 301, sandboxHeaders({ Location: location }), null);
}

/** The error code of a `ZenError` (or anything carrying one). */
function codeOf(e: unknown): unknown {
  return e instanceof Error ? (e as { code?: unknown }).code : undefined;
}

/** sw.md §7: the replica's errors as responses; anything unexpected is 500. */
function failWith(req: Request, site: Site, e: unknown): Response {
  if (e instanceof Error && e.name === 'ZenCryptoError') return fail(req, 'integrity');
  switch (codeOf(e)) {
    case 'not_found':
    case 'not_dir':
      return fail(req, 404);
    case 'offline':
    case 'network':
      return fail(req, 'offline');
    case 'integrity':
    case 'format':
      return fail(req, 'integrity');
    case 'closed':
    case 'starting':
      return fail(req, 'starting');
    case 'locked':
      return fail(req, 'locked');
    default:
      site.onError?.(e, req);
      return fail(req, 500);
  }
}

/** Serve `req` from `site` (sw.md §3–§7). Never rejects. */
export async function handle(req: Request, site: Site): Promise<Response> {
  if (req.method !== 'GET' && req.method !== 'HEAD') return fail(req, 405, { Allow: ALLOW });
  const url = new URL(req.url);
  const r = route(site.mounts, url);
  if (r.kind === 'redirect') return redirect(req, r.location);
  if (r.kind === 'error') return fail(req, r.status);
  try {
    const rep = await site.replica(r.mount);
    const e = await rep.lookup(r.path);
    const type = e.sibling ? 'file' : nodeType(e.node);
    if (type === 'symlink') return fail(req, 404);
    if (type === 'file') {
      // A file's path with a trailing `/` names nothing.
      if (r.trailing) return fail(req, 404);
      return await serveFile(req, rep, e, r.segments.at(-1) ?? '');
    }
    if (!r.trailing) return redirect(req, `${url.pathname}/${url.search}`);
    const kids = await rep.entries(e.node);
    for (const name of r.mount.index) {
      const x = kids.get(name);
      if (x && (x.sibling || nodeType(x.node) === 'file')) {
        return await serveFile(req, rep, x, name);
      }
    }
    if (!r.mount.listing) return fail(req, 404);
    const listed: Listed[] = [];
    for (const [name, x] of kids) {
      const t = x.sibling ? 'file' : nodeType(x.node);
      if (t !== 'symlink') listed.push({ name, dir: t === 'dir' });
    }
    const title = `/${[r.mount.name, ...r.segments].join('/')}/`;
    const html = listingPage(title, listed, r.segments.length > 0);
    const h = sandboxHeaders({
      'Content-Type': 'text/html; charset=utf-8',
      'Content-Length': String(bytes.utf8(html).length),
    });
    return respond(req, 200, h, html);
  } catch (err) {
    return failWith(req, site, err);
  }
}

/** The strong ETag of a version the server holds (sw.md §6); none for a local one. */
export function etagOf(e: Entry, v: ShownVersion | undefined): string | undefined {
  if (!v || isLocal(v)) return undefined;
  return `"${bytes.hex(e.node.id)}.${bytes.hex(v.dot)}"`;
}

/** Whether `If-None-Match` matches (weak comparison, RFC 9110 §13.1.2). */
function noneMatch(header: string, etag: string | undefined): boolean {
  if (header.trim() === '*') return true;
  if (!etag) return false;
  return header.split(',').some((t) => t.trim().replace(/^W\//, '') === etag);
}

function xattrType(e: Entry): string | undefined {
  const raw = decodeXattrs(e.node.meta?.xattrs).get(CONTENT_TYPE);
  return raw ? new TextDecoder().decode(raw) : undefined;
}

async function serveFile(req: Request, rep: Replica, e: Entry, name: string): Promise<Response> {
  const v = await rep.shown(e);
  const size = v?.size ?? 0;
  const etag = etagOf(e, v);
  const base: Record<string, string> = { 'Accept-Ranges': 'bytes' };
  if (etag) base.ETag = etag;

  const inm = req.headers.get('If-None-Match');
  if (inm !== null && noneMatch(inm, etag)) {
    return respond(req, 304, sandboxHeaders(base), null);
  }
  base['Content-Type'] = contentType(name, xattrType(e));

  const rh = req.headers.get('Range');
  const ifRange = req.headers.get('If-Range');
  const applies = rh !== null && (ifRange === null || (!!etag && ifRange.trim() === etag));
  const range = applies ? parseRange(rh, size) : ({ kind: 'full' } as const);
  if (range.kind === 'unsatisfiable') {
    return fail(req, 416, { ...base, 'Content-Range': `bytes */${size}` });
  }
  const [status, first, last] =
    range.kind === 'part' ? [206, range.first, range.last] : [200, 0, size - 1];
  const h = sandboxHeaders({ ...base, 'Content-Length': String(Math.max(0, last - first + 1)) });
  if (status === 206) h.set('Content-Range', `bytes ${first}-${last}/${size}`);
  if (req.method === 'HEAD' || size === 0) return respond(req, status, h, null);
  return respond(req, status, h, await started(rep.streamOf(e, v, { start: first, end: last })));
}

/**
 * The body, once its first piece is read: an error before then (offline, a
 * chunk that fails to open) becomes the response's status; one after it
 * errors the body (sw.md §7).
 */
async function started(s: ReadableStream<Uint8Array>): Promise<ReadableStream<Uint8Array>> {
  const reader = s.getReader();
  let first: ReadableStreamReadResult<Uint8Array>;
  try {
    first = await reader.read();
  } catch (e) {
    reader.releaseLock();
    throw e;
  }
  return new ReadableStream<Uint8Array>(
    {
      start(c) {
        if (first.done) c.close();
        else c.enqueue(first.value);
      },
      async pull(c) {
        try {
          const r = await reader.read();
          if (r.done) c.close();
          else c.enqueue(r.value);
        } catch (e) {
          c.error(e);
        }
      },
      cancel: (why) => reader.cancel(why),
    },
    { highWaterMark: 0 },
  );
}
