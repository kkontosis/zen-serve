// HTTP transport: CBOR POSTs with retries (spec/api.md §1).
import { ZenError } from './errors.js';
import { zw } from './wasm.js';

/** How to reach the server. */
export interface TransportOptions {
  /** A `fetch` to use instead of the global one. */
  fetch?: typeof fetch;
  /**
   * Extra `fetch` init merged into every request. In Node this carries an
   * undici `dispatcher`, for example one that presents a TLS client
   * certificate (sign-in method 5) or trusts a test CA.
   */
  fetchInit?: Record<string, unknown>;
  /** A `WebSocket` constructor to use instead of the global one. */
  WebSocket?: typeof WebSocket;
  /** Attempts for a request refused with 503, or 429 with `Retry-After` (default 3). */
  attempts?: number;
}

const CBOR = 'application/cbor';

/** A request encoder and response decoder of the wire layer. */
export type Enc<T> = (v: T) => Uint8Array;
export type Dec<T> = (b: Uint8Array) => T;

export class Transport {
  readonly base: string;
  readonly opts: TransportOptions;
  private readonly fetchFn: typeof fetch;

  constructor(url: string, opts: TransportOptions = {}) {
    this.base = url.replace(/\/+$/, '');
    this.opts = opts;
    this.fetchFn = opts.fetch ?? globalThis.fetch.bind(globalThis);
  }

  /** The WebSocket URL of a path. */
  wsUrl(path: string): string {
    return this.base.replace(/^http/, 'ws') + path;
  }

  /** POST a CBOR body; returns the CBOR response body. `bearer` is the Authorization value. */
  async post(path: string, body: Uint8Array, bearer?: string): Promise<Uint8Array> {
    const headers: Record<string, string> = { 'content-type': CBOR, accept: CBOR };
    if (bearer !== undefined) headers.authorization = `Bearer ${bearer}`;
    return this.send(path, { method: 'POST', headers, body: body as BodyInit });
  }

  /** GET a path; returns the body. */
  async get(path: string): Promise<Uint8Array> {
    return this.send(path, { method: 'GET', headers: { accept: CBOR } });
  }

  /** A typed POST: encode the request, decode the response. */
  async call<Req, Res>(
    path: string,
    enc: Enc<Req>,
    dec: Dec<Res>,
    req: Req,
    bearer?: string,
  ): Promise<Res> {
    return dec(await this.post(path, enc(req), bearer));
  }

  private async send(path: string, init: RequestInit): Promise<Uint8Array> {
    const attempts = this.opts.attempts ?? 3;
    for (let i = 1; ; i++) {
      let resp: Response;
      try {
        resp = await this.fetchFn(this.base + path, {
          ...(this.opts.fetchInit ?? {}),
          ...init,
        } as RequestInit);
      } catch (e) {
        throw new ZenError(0, 'network', e instanceof Error ? e.message : String(e));
      }
      const body = new Uint8Array(await resp.arrayBuffer());
      if (resp.ok) return body;
      const err = toError(resp, body);
      const again =
        i < attempts &&
        (resp.status === 503 || (resp.status === 429 && err.retryAfterMs !== undefined));
      if (!again) throw err;
      await sleep(Math.min(err.retryAfterMs ?? 100 * 2 ** i, 10_000));
    }
  }
}

function toError(resp: Response, body: Uint8Array): ZenError {
  const ra = resp.headers.get('retry-after');
  const retryAfterMs = ra !== null && /^\d+$/.test(ra) ? Number(ra) * 1000 : undefined;
  try {
    const e = zw.decodeErrorBody(body);
    return new ZenError(resp.status, e.code, e.message, retryAfterMs);
  } catch {
    return new ZenError(resp.status, `http_${resp.status}`, resp.statusText, retryAfterMs);
  }
}

/** Sleep `ms` milliseconds. */
export function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}
