// The headers on every response the worker makes (spec/sw.md §4).

/**
 * The content origin's policy: no scripts, nothing from other origins, the
 * origin kept so the worker controls the page. Tested against sw.md §4's
 * text, as the server's `statics.rs` `SANDBOX_CSP` is.
 */
export const SANDBOX_CSP =
  'sandbox allow-same-origin allow-popups allow-popups-to-escape-sandbox allow-downloads; ' +
  "default-src 'self' data: blob:; script-src 'none'; object-src 'none'; " +
  "style-src 'self' 'unsafe-inline'; base-uri 'none'; form-action 'none'; " +
  "frame-ancestors 'none'";

/** sw.md §4's headers. */
export const SANDBOX_HEADERS: readonly (readonly [string, string])[] = [
  ['Content-Security-Policy', SANDBOX_CSP],
  ['X-Content-Type-Options', 'nosniff'],
  ['Referrer-Policy', 'no-referrer'],
  ['Cross-Origin-Resource-Policy', 'same-origin'],
  ['Cache-Control', 'no-store'],
];

/** New headers: sw.md §4's, then `extra`. */
export function sandboxHeaders(extra: Record<string, string> = {}): Headers {
  const h = new Headers();
  for (const [k, v] of SANDBOX_HEADERS) h.set(k, v);
  for (const [k, v] of Object.entries(extra)) h.set(k, v);
  return h;
}
